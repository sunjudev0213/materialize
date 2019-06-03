// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use std::collections::HashSet;
// use std::hash::BuildHasher;

use timely::dataflow::Scope;
use timely::progress::timestamp::Refines;

use differential_dataflow::{lattice::Lattice, Collection};

use crate::dataflow2::context::Context;
use crate::dataflow2::types::RelationExpr;
use crate::repr::Datum;

pub fn render<G>(
    plan: RelationExpr,
    scope: &mut G,
    context: &mut Context<G, RelationExpr, Datum, crate::clock::Timestamp>,
) -> Collection<G, Vec<Datum>, isize>
where
    G: Scope,
    G::Timestamp: Lattice + Refines<crate::clock::Timestamp>,
    // S: BuildHasher + Clone,
{
    if context.collections.get(&plan).is_none() {
        let collection = match plan.clone() {
            RelationExpr::Constant { rows, .. } => {
                use differential_dataflow::collection::AsCollection;
                use timely::dataflow::operators::{Map, ToStream};
                rows.to_stream(scope)
                    .map(|x| (x, Default::default(), 1))
                    .as_collection()
            }
            RelationExpr::Get { name, typ: _ } => {
                // TODO: something more tasteful.
                // perhaps load an empty collection, warn?
                panic!("Collection {} not pre-loaded", name);
            }
            RelationExpr::Let { name, value, body } => {
                let typ = value.typ();
                let value = render(*value, scope, context);
                let bind = RelationExpr::Get { name, typ };
                let prior = context.collections.insert(bind.clone(), value);
                let result = render(*body, scope, context);
                if let Some(prior) = prior {
                    context.collections.insert(bind, prior);
                }
                result
            }
            RelationExpr::Project { input, outputs } => {
                let input = render(*input, scope, context);
                input.map(move |tuple| outputs.iter().map(|i| tuple[*i].clone()).collect())
            }
            RelationExpr::Map { input, scalars } => {
                let input = render(*input, scope, context);
                input.map(move |mut tuple| {
                    let len = tuple.len();
                    for s in scalars.iter() {
                        let to_push = s.0.eval_on(&tuple[..len]);
                        tuple.push(to_push);
                    }
                    tuple
                })
            }
            RelationExpr::Filter { input, predicates } => {
                let input = render(*input, scope, context);
                input.filter(move |x| {
                    predicates
                        .iter()
                        .all(|predicate| match predicate.eval_on(x) {
                            Datum::True => true,
                            Datum::False => false,
                            _ => unreachable!(),
                        })
                })
            }
            RelationExpr::Join { inputs, variables } => {
                use differential_dataflow::operators::join::Join;

                // For the moment, assert that each relation participates at most
                // once in each equivalence class. If not, we should be able to
                // push a filter upwards, and if we can't do that it means a bit
                // more filter logic in this operator which doesn't exist yet.
                assert!(variables.iter().all(|h| {
                    let len = h.len();
                    let mut list = h.iter().map(|(i, _)| i).collect::<Vec<_>>();
                    list.sort();
                    list.dedup();
                    len == list.len()
                }));

                let arities = inputs.iter().map(|i| i.arity()).collect::<Vec<_>>();

                // The plan is to implement join as a `fold` over `inputs`.
                let mut input_iter = inputs.into_iter().enumerate();
                if let Some((index, input)) = input_iter.next() {
                    let mut joined = render(input, scope, context);

                    // Maintain sources of each in-progress column.
                    let mut columns = (0..arities[index]).map(|c| (index, c)).collect::<Vec<_>>();

                    // The intent is to maintain `joined` as the full cross
                    // product of all input relations so far, subject to all
                    // of the equality constraints in `variables`. This means
                    for (index, input) in input_iter {
                        let input = render(input, scope, context);

                        // Determine keys. there is at most one key for each
                        // equivalence class, and an equivalence class is only
                        // engaged if it contains both a new and an old column.
                        // If the class contains *more than one* new column we
                        // may need to put a `filter` in, or perhaps await a
                        // later join (and ensure that one exists).

                        let mut old_keys = Vec::new();
                        let mut new_keys = Vec::new();

                        for sets in variables.iter() {
                            let new_pos = sets.iter().position(|(i, _)| i == &index);
                            let old_pos = columns.iter().position(|i| sets.contains(i));

                            // If we have both a new and an old column in the constraint ...
                            if let (Some(new_pos), Some(old_pos)) = (new_pos, old_pos) {
                                old_keys.push(old_pos);
                                new_keys.push(new_pos);
                            }
                        }

                        let old_keyed = joined.map(move |tuple| {
                            (
                                old_keys
                                    .iter()
                                    .map(|i| tuple[*i].clone())
                                    .collect::<Vec<_>>(),
                                tuple,
                            )
                        });
                        let new_keyed = input.map(move |tuple| {
                            (
                                new_keys
                                    .iter()
                                    .map(|i| tuple[*i].clone())
                                    .collect::<Vec<_>>(),
                                tuple,
                            )
                        });

                        joined = old_keyed.join_map(&new_keyed, |_keys, old, new| {
                            old.iter().chain(new).cloned().collect::<Vec<_>>()
                        });

                        columns.extend((0..arities[index]).map(|c| (index, c)));
                    }

                    joined
                } else {
                    panic!("Empty join; why?");
                }
            }
            RelationExpr::Reduce {
                input,
                group_key,
                aggregates,
            } => {
                use differential_dataflow::operators::Reduce;
                let input = render(*input, scope, context);
                input
                    .map(move |tuple| {
                        (
                            group_key
                                .iter()
                                .map(|i| tuple[*i].clone())
                                .collect::<Vec<_>>(),
                            tuple,
                        )
                    })
                    .reduce(move |_key, source, target| {
                        let mut result = Vec::with_capacity(aggregates.len());
                        for (_idx, (agg, _typ)) in aggregates.iter().enumerate() {
                            if agg.distinct {
                                let iter = source
                                    .iter()
                                    .flat_map(|(v, w)| {
                                        if *w > 0 {
                                            Some(agg.expr.eval_on(v))
                                        } else {
                                            None
                                        }
                                    })
                                    .collect::<HashSet<_>>();
                                result.push((agg.func.func())(iter));
                            } else {
                                let iter = source.iter().flat_map(|(v, w)| {
                                    let eval = agg.expr.eval_on(v);
                                    std::iter::repeat(eval).take(std::cmp::max(*w, 0) as usize)
                                });
                                result.push((agg.func.func())(iter));
                            }
                        }
                        target.push((result, 1));
                    })
                    .map(|(mut key, agg)| {
                        key.extend(agg.into_iter());
                        key
                    })
            }

            RelationExpr::OrDefault { input, default } => {
                use differential_dataflow::collection::AsCollection;
                use differential_dataflow::operators::reduce::Threshold;
                use differential_dataflow::operators::Join;
                use timely::dataflow::operators::to_stream::ToStream;

                let input = render(*input, scope, context);
                let present = input.map(|_| ()).distinct();
                let default = vec![(((), default), Default::default(), 1isize)]
                    .to_stream(scope)
                    .as_collection()
                    .antijoin(&present)
                    .map(|((), default)| default);

                input.concat(&default)
            }
            RelationExpr::Negate { input } => {
                let input = render(*input, scope, context);
                input.negate()
            }
            RelationExpr::Distinct { input } => {
                use differential_dataflow::operators::reduce::Threshold;
                let input = render(*input, scope, context);
                input.distinct()
            }
            RelationExpr::Union { left, right } => {
                let input1 = render(*left, scope, context);
                let input2 = render(*right, scope, context);
                input1.concat(&input2)
            }
        };

        context.collections.insert(plan.clone(), collection);
    }

    context
        .collections
        .get(&plan)
        .expect("Collection surprisingly absent")
        .clone()
}
