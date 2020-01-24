// Copyright 2020 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use timely::dataflow::Scope;

use differential_dataflow::collection::AsCollection;
use differential_dataflow::difference::DiffPair;
use differential_dataflow::hashable::Hashable;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::operators::reduce::Count;
use differential_dataflow::operators::reduce::Reduce;
use differential_dataflow::operators::reduce::Threshold;
use differential_dataflow::trace::implementations::ord::OrdValSpine;
use differential_dataflow::Collection;

use expr::{AggregateExpr, AggregateFunc, EvalEnv, RelationExpr};
use repr::{Datum, Row, RowArena, RowPacker};

use super::context::Context;

impl<G, T> Context<G, RelationExpr, Row, T>
where
    G: Scope,
    G::Timestamp: Lattice + timely::progress::timestamp::Refines<T>,
    T: timely::progress::Timestamp + Lattice,
{
    pub fn render_reduce_robust(
        &mut self,
        relation_expr: &RelationExpr,
        env: &EvalEnv,
        scope: &mut G,
        worker_index: usize,
    ) {
        if let RelationExpr::Reduce {
            input,
            group_key,
            aggregates,
        } = relation_expr
        {
            // The reduce operator may have multiple aggregation functions, some of
            // which should only be applied to distinct values for each key. We need
            // to build a non-trivial dataflow fragment to robustly implement these
            // aggregations, including:
            //
            // 1. Different reductions for each aggregation, to avoid maintaining
            //    state proportional to the cross-product of values.
            //
            // 2. Distinct operators before each reduction which requires distinct
            //    inputs, to avoid recomputation when the distinct set is stable.
            //
            // 3. Hierachical aggregation for operators like min and max that we
            //    cannot perform in the diff field.
            //
            // Our plan is to perform these actions, and the re-integrate the results
            // in a final reduce whose output arrangement looks just as if we had
            // applied a single reduction (which should be good for any consumers
            // of the operator and its arrangement).

            use differential_dataflow::operators::reduce::ReduceCore;

            let keys_clone = group_key.clone();

            self.ensure_rendered(input, env, scope, worker_index);
            let input = self.collection(input).unwrap();

            // Distinct is a special case, as there are no aggregates to aggregate.
            // In this case, we use a special implementation that does not rely on
            // collating aggregates.
            if aggregates.is_empty() {
                let arrangement = input
                    .map({
                        let env = env.clone();
                        let group_key = group_key.clone();
                        move |row| {
                            let temp_storage = RowArena::new();
                            let datums = row.unpack();
                            (
                                Row::pack(
                                    group_key
                                        .iter()
                                        .map(|i| i.eval(&datums, &env, &temp_storage)),
                                ),
                                (),
                            )
                        }
                    })
                    .reduce_abelian::<_, OrdValSpine<_, _, _, _>>("DistinctBy", {
                        |key, _input, output| {
                            output.push((key.clone(), 1));
                        }
                    });

                let index = (0..keys_clone.len()).collect::<Vec<_>>();
                self.set_local_columns(relation_expr, &index[..], arrangement);
            } else {
                // We'll accumulate partial aggregates here, where each contains updates
                // of the form `(key, (index, value))`. This is eventually concatenated,
                // and fed into a final reduce to put the elements in order.
                // TODO: When there is a single aggregate, we can skip the complexity.
                let mut partials = Vec::with_capacity(aggregates.len());
                // Bound the complex dataflow in a region, for better interpretability.
                scope.region(|region| {
                    // Create an iterator over collections, where each is the application
                    // of one aggregation function whose results are annotated with its
                    // position in the final results. To be followed by a merge reduction.
                    for (index, aggr) in aggregates.iter().enumerate() {
                        // Extract and bind each aspect of the aggregation.
                        let AggregateExpr {
                            func,
                            expr,
                            distinct,
                        } = aggr.clone();

                        // The `partial` collection contains `(key, val)` pairs.
                        let mut partial = input.enter(region).map({
                            let env = env.clone();
                            let group_key = group_key.clone();
                            move |row| {
                                let temp_storage = RowArena::new();
                                let datums = row.unpack();
                                (
                                    Row::pack(
                                        group_key
                                            .iter()
                                            .map(|i| i.eval(&datums, &env, &temp_storage)),
                                    ),
                                    Row::pack(Some(expr.eval(&datums, &env, &temp_storage))),
                                )
                            }
                        });

                        // If `distinct` is set, we restrict ourselves to the distinct `(key, val)`.
                        if distinct {
                            partial = partial.distinct();
                        }

                        // Our strategy will depend on whether the function is accumulable in-place,
                        // or can be subjected to hierarchical aggregation. At the moment all functions
                        // are one of the two, but this should work even with methods that are neither.
                        let (accumulable, hierarchical) = accumulable_hierarchical(&func);

                        partial = if accumulable {
                            build_accumulable(partial, func)
                        } else {
                            // If hierarchical, we can repeatedly digest the groups, to minimize the incremental
                            // update costs on relatively small updates.
                            if hierarchical {
                                partial = build_hierarchical(partial, &func, env.clone())
                            }

                            // Perform a final aggregation, on potentially hierarchically reduced data.
                            // The same code should work on data that can not be hierarchically reduced.
                            partial.reduce_named("ReduceInaccumulable", {
                                let env = env.clone();
                                move |_key, source, target| {
                                    // We respect the multiplicity here (unlike in hierarchical aggregation)
                                    // because we don't know that the aggregation method is not sensitive
                                    // to the number of records.
                                    let iter = source.iter().flat_map(|(v, w)| {
                                        std::iter::repeat(v.iter().next().unwrap())
                                            .take(*w as usize)
                                    });
                                    target.push((
                                        Row::pack(Some(func.eval(iter, &env, &RowArena::new()))),
                                        1,
                                    ));
                                }
                            })
                        };

                        // Collect the now-aggregated partial result, annotated with its position.
                        partials.push(partial.map(move |(key, val)| (key, (index, val))).leave());
                    }
                });

                // Our final action is to collect the partial results into one record.
                //
                // We concatenate the partial results and lay out the fields as indicated by their
                // recorded positions. All keys should contribute exactly one value for each of the
                // aggregates, which we check with assertions; this is true independent of transient
                // change and inconsistency in the inputs; if this is not the case there is a defect
                // in differential dataflow.
                let arrangement =
                    differential_dataflow::collection::concatenate::<_, _, _, _>(scope, partials)
                        .reduce_abelian::<_, OrdValSpine<_, _, _, _>>("ReduceCollation", {
                        let aggregates_len = aggregates.len();
                        move |key, input, output| {
                            // The intent, unless things are terribly wrong, is that `input`
                            // contains, in order, the values to drop into `output`.
                            assert_eq!(input.len(), aggregates_len);
                            let mut result = RowPacker::new();
                            result.extend(key.iter());
                            for (index, ((pos, val), cnt)) in input.iter().enumerate() {
                                assert_eq!(*pos, index);
                                assert_eq!(*cnt, 1);
                                result.push(val.unpack().pop().unwrap());
                            }
                            output.push((result.finish(), 1));
                        }
                    });

                let index = (0..keys_clone.len()).collect::<Vec<_>>();
                self.set_local_columns(relation_expr, &index[..], arrangement);
            }
        }
    }
}

/// Builds the dataflow for a reduction that can be performed in-place.
///
/// The incoming values are moved to the update's "difference" field, at which point
/// they can be accumulated in place. The `count` operator promotes the accumulated
/// values to data, at which point a final map applies operator-specific logic to
/// yield the final aggregate.
fn build_accumulable<G>(
    collection: Collection<G, (Row, Row)>,
    aggr: AggregateFunc,
) -> Collection<G, (Row, Row)>
where
    G: Scope,
    G::Timestamp: Lattice,
{
    use timely::dataflow::operators::map::Map;

    let float_scale = f64::from(1 << 24);

    collection
        .inner
        .map(|(d, t, r)| (d, t, r as i128))
        .as_collection()
        .explode({
            let aggr = aggr.clone();
            move |(key, row)| {
                let datum = row.unpack()[0];
                let (aggs, nonnulls) = match aggr {
                    AggregateFunc::CountAll => {
                        // Nothing beyond the accumulated count is needed.
                        (0i128, 0i128)
                    }
                    AggregateFunc::Count => {
                        // Count needs to distinguish nulls from zero.
                        (1, if datum.is_null() { 0 } else { 1 })
                    }
                    AggregateFunc::Any => match datum {
                        Datum::True => (1, 0),
                        Datum::Null => (0, 0),
                        Datum::False => (0, 1),
                        x => panic!("Invalid argument to AggregateFunc::Any: {:?}", x),
                    },
                    AggregateFunc::All => match datum {
                        Datum::True => (1, 0),
                        Datum::Null => (0, 0),
                        Datum::False => (0, 1),
                        x => panic!("Invalid argument to AggregateFunc::All: {:?}", x),
                    },
                    _ => {
                        // Other accumulations need to disentangle the accumulable
                        // value from its NULL-ness, which is not quite as easily
                        // accumulated.
                        match datum {
                            Datum::Int32(i) => (i128::from(i), 1),
                            Datum::Int64(i) => (i128::from(i), 1),
                            Datum::Float32(f) => ((f64::from(*f) * float_scale) as i128, 1),
                            Datum::Float64(f) => ((*f * float_scale) as i128, 1),
                            Datum::Decimal(d) => (d.as_i128(), 1),
                            Datum::Null => (0, 0),
                            x => panic!("Accumulating non-integer data: {:?}", x),
                        }
                    }
                };
                Some((key, DiffPair::new(1i128, DiffPair::new(aggs, nonnulls))))
            }
        })
        .count()
        .map(move |(key, accum)| {
            let tot = accum.element1;

            // For most aggregations, the first aggregate is the "data" and the second is the number
            // of non-null elements (so that we can determine if we should produce 0 or a Null).
            // For Any and All, the two aggregates are the numbers of true and false records, resp.
            let agg1 = accum.element2.element1;
            let agg2 = accum.element2.element2;

            // The finished value depends on the aggregation function in a variety of ways.
            let value = match (&aggr, agg2) {
                (AggregateFunc::Count, _) => Datum::Int64(agg2 as i64),
                (AggregateFunc::CountAll, _) => Datum::Int64(tot as i64),
                (AggregateFunc::All, _) => {
                    // If any false, else if all true, else must be no false and some nulls.
                    if agg2 > 0 {
                        Datum::False
                    } else if tot == agg1 {
                        Datum::True
                    } else {
                        Datum::Null
                    }
                }
                (AggregateFunc::Any, _) => {
                    // If any true, else if all false, else must be no true and some nulls.
                    if agg1 > 0 {
                        Datum::True
                    } else if tot == agg2 {
                        Datum::False
                    } else {
                        Datum::Null
                    }
                }
                // Below this point, anything with only nulls should be null.
                (_, 0) => Datum::Null,
                // If any non-nulls, just report the aggregate.
                (AggregateFunc::SumInt32, _) => Datum::Int32(agg1 as i32),
                (AggregateFunc::SumInt64, _) => Datum::Int64(agg1 as i64),
                (AggregateFunc::SumFloat32, _) => {
                    Datum::Float32((((agg1 as f64) / float_scale) as f32).into())
                }
                (AggregateFunc::SumFloat64, _) => {
                    Datum::Float64(((agg1 as f64) / float_scale).into())
                }
                (AggregateFunc::SumDecimal, _) => Datum::from(agg1),
                (AggregateFunc::SumNull, _) => Datum::Null,
                x => panic!("Unexpected accumulable aggregation: {:?}", x),
            };
            // Pack the value with the key as the result.
            (key, Row::pack(Some(value)))
        })
}

/// Builds a dataflow for hierarchical aggregation.
///
/// The dataflow repeatedly applies stages of reductions on progressively more coarse
/// groupings, each of which refines the actual key grouping.
fn build_hierarchical<G>(
    collection: Collection<G, (Row, Row)>,
    aggr: &AggregateFunc,
    env: EvalEnv,
) -> Collection<G, (Row, Row)>
where
    G: Scope,
    G::Timestamp: Lattice,
{
    // Repeatedly apply hierarchical reduction with a progressively coarser key.
    let mut stage = collection.map({ move |(key, row)| ((key, row.hashed()), row) });
    for log_modulus in [60, 56, 52, 48, 44, 40, 36, 32, 28, 24, 20, 16, 12, 8, 4u64].iter() {
        stage = build_hierarchical_stage(stage, aggr.clone(), env.clone(), 1u64 << log_modulus);
    }

    // Discard the hash from the key and return to the format of the input data.
    stage.map(|((key, _hash), val)| (key, val))
}

fn build_hierarchical_stage<G>(
    collection: Collection<G, ((Row, u64), Row)>,
    aggr: AggregateFunc,
    env: EvalEnv,
    modulus: u64,
) -> Collection<G, ((Row, u64), Row)>
where
    G: Scope,
    G::Timestamp: Lattice,
{
    collection
        .map(move |((key, hash), row)| ((key, hash % modulus), row))
        .reduce_named("ReduceHierarchical", {
            move |_key, source, target| {
                // We ignore the count here under the belief that it cannot affect
                // hierarchical aggregations; should that belief be incorrect, we
                // should certainly revising this implementation.
                let iter = source.iter().map(|(val, _cnt)| val.iter().next().unwrap());
                target.push((Row::pack(Some(aggr.eval(iter, &env, &RowArena::new()))), 1));
            }
        })
}

/// Determines whether a function can be accumulated in an update's "difference" field,
/// and whether it can be subjected to recursive (hierarchical) aggregation.
///
/// At present, there is a dichotomy, but this is set up to complain if new aggregations
/// are added that perhaps violate these requirement. For example, a "median" aggregation
/// could be neither accumulable nor hierarchical.
fn accumulable_hierarchical(func: &AggregateFunc) -> (bool, bool) {
    match func {
        AggregateFunc::SumInt32
        | AggregateFunc::SumInt64
        | AggregateFunc::SumFloat32
        | AggregateFunc::SumFloat64
        | AggregateFunc::SumDecimal
        | AggregateFunc::SumNull
        | AggregateFunc::Count
        | AggregateFunc::CountAll
        | AggregateFunc::Any
        | AggregateFunc::All => (true, false),
        AggregateFunc::MaxInt32
        | AggregateFunc::MaxInt64
        | AggregateFunc::MaxFloat32
        | AggregateFunc::MaxFloat64
        | AggregateFunc::MaxDecimal
        | AggregateFunc::MaxBool
        | AggregateFunc::MaxString
        | AggregateFunc::MaxDate
        | AggregateFunc::MaxTimestamp
        | AggregateFunc::MaxTimestampTz
        | AggregateFunc::MaxNull
        | AggregateFunc::MinInt32
        | AggregateFunc::MinInt64
        | AggregateFunc::MinFloat32
        | AggregateFunc::MinFloat64
        | AggregateFunc::MinDecimal
        | AggregateFunc::MinBool
        | AggregateFunc::MinString
        | AggregateFunc::MinDate
        | AggregateFunc::MinTimestamp
        | AggregateFunc::MinTimestampTz
        | AggregateFunc::MinNull => (false, true),
    }
}
