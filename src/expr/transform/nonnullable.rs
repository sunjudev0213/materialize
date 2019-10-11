// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use crate::relation::AggregateExpr;
use crate::AggregateFunc;
use crate::UnaryFunc;
use crate::{RelationExpr, ScalarExpr};
use repr::Datum;
use repr::RelationType;

#[derive(Debug)]
pub struct NonNullable;

impl super::Transform for NonNullable {
    fn transform(&self, relation: &mut RelationExpr) {
        self.transform(relation)
    }
}

impl NonNullable {
    pub fn transform(&self, relation: &mut RelationExpr) {
        relation.visit_mut_pre(&mut |e| {
            self.action(e);
        });
    }

    pub fn action(&self, relation: &mut RelationExpr) {
        match relation {
            RelationExpr::Map { input, scalars } => {
                if scalars.iter().any(|(s, _)| scalar_contains_isnull(s)) {
                    let metadata = input.typ();
                    for (scalar, _typ) in scalars.iter_mut() {
                        scalar_nonnullable(scalar, &metadata);
                    }
                }
            }
            RelationExpr::Filter { input, predicates } => {
                if predicates.iter().any(|s| scalar_contains_isnull(s)) {
                    let metadata = input.typ();
                    for predicate in predicates.iter_mut() {
                        scalar_nonnullable(predicate, &metadata);
                    }
                }
            }
            RelationExpr::Reduce {
                input,
                group_key: _,
                aggregates,
            } => {
                if aggregates.iter().any(|a| {
                    scalar_contains_isnull(&(a.0).expr)
                        || if let AggregateFunc::Count = &(a.0).func {
                            true
                        } else {
                            false
                        }
                }) {
                    let metadata = input.typ();
                    for (aggregate, _typ) in aggregates.iter_mut() {
                        scalar_nonnullable(&mut aggregate.expr, &metadata);
                        aggregate_nonnullable(aggregate, &metadata);
                    }
                }
            }
            _ => {}
        }
    }
}

/// True if the expression contains a "is null" test.
fn scalar_contains_isnull(expr: &ScalarExpr) -> bool {
    let mut result = false;
    expr.visit(&mut |e| {
        if let ScalarExpr::CallUnary {
            func: UnaryFunc::IsNull,
            ..
        } = e
        {
            result = true;
        }
    });
    result
}

/// Transformations to scalar functions, based on nonnullability of columns.
fn scalar_nonnullable(expr: &mut ScalarExpr, metadata: &RelationType) {
    // Tests for null can be replaced by "false" for non-nullable columns.
    expr.visit_mut(&mut |e| {
        if let ScalarExpr::CallUnary {
            func: UnaryFunc::IsNull,
            expr,
        } = e
        {
            if let ScalarExpr::Column(c) = &**expr {
                if !metadata.column_types[*c].nullable {
                    *e = ScalarExpr::Literal(Datum::False);
                }
            }
        }
    })
}

/// Transformations to aggregation functions, based on nonnullability of columns.
fn aggregate_nonnullable(expr: &mut AggregateExpr, metadata: &RelationType) {
    // An aggregate that is a count of non-nullable data can be replaced by a countall.
    if let (AggregateFunc::Count, ScalarExpr::Column(c)) = (&expr.func, &expr.expr) {
        if !metadata.column_types[*c].nullable && !expr.distinct {
            expr.func = AggregateFunc::CountAll;
            expr.expr = ScalarExpr::Literal(Datum::Null);
        }
    }
}
