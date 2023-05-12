// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;
use std::fmt::Debug;

use mz_repr::adt::datetime::DateTimeUnits;
use mz_repr::{ColumnType, Datum, RelationType, Row, RowArena, ScalarType};

use crate::{
    BinaryFunc, EvalError, MapFilterProject, MfpPlan, MirScalarExpr, UnaryFunc,
    UnmaterializableFunc, VariadicFunc,
};

/// Tracks whether a function is monotonic, either increasing or decreasing.
/// This property is useful for static analysis because a monotonic function maps ranges to ranges:
/// ie. if `a` is between `b` and `c` inclusive, then `f(a)` is between `f(b)` and `f(c)` inclusive.
/// (However, if either `f(b)` and `f(c)` error or return null, we don't assume anything about `f(a)`.
///
/// This should likely be moved to a function annotation in the future.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Monotonic {
    /// Monotonic in this argument. (Either increasing or decreasing, non-strict.)
    Yes,
    /// We haven't classified this one yet. (Conservatively treated as "no", but a distinct
    /// enum so it's easy to tell whether someone's thought about a particular function yet.)
    /// This should likely be removed when this becomes a function annotation.
    Maybe,
    /// We don't know any useful properties of this function.
    No,
}

fn unary_monotonic(func: &UnaryFunc) -> Monotonic {
    use crate::func::impls as funcs;
    use Monotonic::*;
    use UnaryFunc::*;
    match func {
        // Casts are generally monotonic.
        CastUint64ToNumeric(_)
        | CastNumericToUint64(_)
        | CastNumericToMzTimestamp(_)
        | CastUint64ToMzTimestamp(_)
        | CastInt64ToMzTimestamp(_)
        | CastTimestampToMzTimestamp(_)
        | CastTimestampTzToMzTimestamp(_) => Yes,
        // Including JSON casts.
        CastJsonbToNumeric(_)
        | CastJsonbToBool(_)
        | CastJsonbToString(_)
        | CastJsonbToInt64(_)
        | CastJsonbToFloat64(_) => Yes,

        // Extracting the "most significant bits" of the a timestamp is monotonic.
        ExtractTimestamp(funcs::ExtractTimestamp(units))
        | ExtractTimestampTz(funcs::ExtractTimestampTz(units)) => match units {
            DateTimeUnits::Epoch
            | DateTimeUnits::Millennium
            | DateTimeUnits::Century
            | DateTimeUnits::Decade
            | DateTimeUnits::Year => Yes,
            _ => No,
        },
        DateTruncTimestamp(_) | DateTruncTimestampTz(_) => Yes,
        // Negation is monotonically decreasing, but that's fine.
        NegInt64(_) | NegNumeric(_) | Not(_) => Yes,
        // Trivially monotonic, since all non-null values map to `false`.
        IsNull(_) => Yes,
        AbsInt64(_) => No,
        _ => Maybe,
    }
}

/// Describes the pointwise behaviour of each of the two arguments of the function.
/// (ie. the first element of the tuple is `Monotonic::Yes` if, for any value of the second argument
/// increasing the first argument causes the result of the function to either monotonically
/// increase or decrease.) For example, subtraction is considered monotonic in both arguments:
/// in the first because `a - C` increases monotonically as `a` increases, and in the second because
/// `C - b` decreases monotonically as `b` increases.
fn binary_monotonic(func: &BinaryFunc) -> (Monotonic, Monotonic) {
    use BinaryFunc::*;
    use Monotonic::*;

    match func {
        AddInt64 | MulInt64 | AddNumeric | MulNumeric | SubInt64 | SubNumeric => (Yes, Yes),
        AddInterval | SubInterval => (Yes, Yes),
        AddTimestampInterval
        | AddTimestampTzInterval
        | SubTimestampInterval
        | SubTimestampTzInterval => (Yes, Yes),
        // Monotonic in the left argument, but the right hand side has a discontinuity.
        // Could be treated as monotonic if we also captured the valid domain of the function args.
        DivInt64 | DivNumeric => (Yes, No),
        Lt | Lte | Gt | Gte => (Yes, Yes),
        _ => (Maybe, Maybe),
    }
}

/// Describes the pointwise behaviour of each of the arguments to our variadic function.
/// (ie. returns `Monotonic::Yes` if, for each argument of the function, increasing that argument
/// causes the result of the function to either monotonically increase or decrease.)
fn variadic_monotonic(func: &VariadicFunc) -> Monotonic {
    use Monotonic::*;
    use VariadicFunc::*;
    match func {
        And | Or | Coalesce => Yes,
        _ => Maybe,
    }
}

/// An inclusive range of non-null datum values.
#[derive(Clone, Eq, PartialEq, Debug)]
enum Values<'a> {
    /// This range contains no values.
    Empty,
    /// An inclusive range. Invariant: the first element is always <= the second.
    // TODO: a variant for small sets of data would avoid losing precision here.
    Within(Datum<'a>, Datum<'a>),
    /// Constraints on structured fields, useful for recursive structures like maps.
    /// Fields that are not present in the map default to Values::All.
    // TODO: consider using this variant, or similar, for Datum::List.
    Nested(BTreeMap<Datum<'a>, ResultSpec<'a>>),
    /// This range might contain any value. Since we're overapproximating, this is often used
    /// as a "safe fallback" when we can't determine the right boundaries for a range.
    All,
}

impl<'a> Values<'a> {
    fn just(a: Datum<'a>) -> Values<'a> {
        match a {
            Datum::Map(datum_map) => Values::Nested(
                datum_map
                    .iter()
                    .map(|(key, val)| (key.into(), ResultSpec::value(val)))
                    .collect(),
            ),
            other => Self::Within(other, other),
        }
    }

    fn union(self, other: Values<'a>) -> Values<'a> {
        match (self, other) {
            (Values::Empty, r) => r,
            (r, Values::Empty) => r,
            (Values::Within(a0, a1), Values::Within(b0, b1)) => {
                Values::Within(a0.min(b0), a1.max(b1))
            }
            (Values::Nested(mut a), Values::Nested(mut b)) => {
                a.retain(|datum, values| {
                    if let Some(other_values) = b.remove(datum) {
                        *values = values.clone().union(other_values);
                    }
                    *values != ResultSpec::anything()
                });
                if a.is_empty() {
                    Values::All
                } else {
                    Values::Nested(a)
                }
            }
            _ => Values::All,
        }
    }

    fn intersect(self, other: Values<'a>) -> Values<'a> {
        match (self, other) {
            (Values::Empty, _) => Values::Empty,
            (_, Values::Empty) => Values::Empty,
            (Values::Within(a0, a1), Values::Within(b0, b1)) => {
                let min = a0.max(b0);
                let max = a1.min(b1);
                if min <= max {
                    Values::Within(min, max)
                } else {
                    Values::Empty
                }
            }
            (Values::Nested(mut a), Values::Nested(b)) => {
                for (datum, other_spec) in b {
                    let spec = a.entry(datum).or_insert_with(ResultSpec::anything);
                    *spec = spec.clone().intersect(other_spec);
                }
                Values::Nested(a)
            }
            (Values::All, v) => v,
            (v, Values::All) => v,
            (Values::Nested(_), Values::Within(_, _)) => Values::Empty,
            (Values::Within(_, _), Values::Nested(_)) => Values::Empty,
        }
    }

    fn may_contain(&self, value: Datum<'a>) -> bool {
        match self {
            Values::Empty => false,
            Values::Within(min, max) => *min <= value && value <= *max,
            Values::All => true,
            Values::Nested(field_map) => match value {
                Datum::Map(datum_map) => {
                    datum_map
                        .iter()
                        .all(|(key, val)| match field_map.get(&key.into()) {
                            None => true,
                            Some(nested) => nested.may_contain(val),
                        })
                }
                _ => false,
            },
        }
    }
}

/// An approximation of the set of values an expression might have, including whether or not it
/// might be null or an error value. This is generally an _overapproximation_, in the sense that
/// [ResultSpec::may_contain] may return true even if the argument is not included in the set.
/// (However, it should never return false when the value _is_ included!)
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ResultSpec<'a> {
    /// True if the expression may evaluate to [Datum::Null].
    nullable: bool,
    /// True if the expression may evaluate to an error.
    fallible: bool,
    /// The range of possible (non-null) values that the expression may evaluate to.
    values: Values<'a>,
}

impl<'a> ResultSpec<'a> {
    /// No results match this spec. (For example, an empty table.)
    pub fn nothing() -> Self {
        ResultSpec {
            nullable: false,
            fallible: false,
            values: Values::Empty,
        }
    }

    /// Every result matches this spec.
    pub fn anything() -> Self {
        ResultSpec {
            nullable: true,
            fallible: true,
            values: Values::All,
        }
    }

    /// A spec that only matches null.
    pub fn null() -> Self {
        ResultSpec {
            nullable: true,
            ..Self::nothing()
        }
    }

    /// A spec that only matches error values.
    pub fn fails() -> Self {
        ResultSpec {
            fallible: true,
            ..Self::nothing()
        }
    }

    /// A spec that matches all values of a given type.
    pub fn has_type(col: &ColumnType, fallible: bool) -> ResultSpec<'a> {
        let values = match &col.scalar_type {
            ScalarType::Bool => Values::Within(Datum::False, Datum::True),
            // TODO: add bounds for other bounded types, like integers
            _ => Values::All,
        };
        ResultSpec {
            nullable: col.nullable,
            fallible,
            values,
        }
    }

    /// A spec that only matches the given value.
    pub fn value(value: Datum<'a>) -> ResultSpec<'a> {
        match value {
            Datum::Null => Self::null(),
            nonnull => ResultSpec {
                values: Values::just(nonnull),
                ..Self::nothing()
            },
        }
    }

    /// A spec that matches values between the given (non-null) min and max.
    pub fn value_between(min: Datum<'a>, max: Datum<'a>) -> ResultSpec<'a> {
        assert!(!min.is_null());
        assert!(!max.is_null());
        assert!(min <= max);
        ResultSpec {
            values: Values::Within(min, max),
            ..ResultSpec::nothing()
        }
    }

    /// A spec that matches any non-null value.
    pub fn value_all() -> ResultSpec<'a> {
        ResultSpec {
            values: Values::All,
            ..ResultSpec::nothing()
        }
    }

    /// A spec that matches Datum::Maps of the given type.
    pub fn map_spec(map: BTreeMap<Datum<'a>, ResultSpec<'a>>) -> ResultSpec<'a> {
        ResultSpec {
            values: Values::Nested(map),
            ..ResultSpec::nothing()
        }
    }

    /// Given two specs, returns a new spec that matches anything that either original spec would match.
    pub fn union(self, other: ResultSpec<'a>) -> ResultSpec<'a> {
        ResultSpec {
            nullable: self.nullable || other.nullable,
            fallible: self.fallible || other.fallible,
            values: self.values.union(other.values),
        }
    }

    /// Given two specs, returns a new spec that only matches things that both original specs would match.
    pub fn intersect(self, other: ResultSpec<'a>) -> ResultSpec<'a> {
        ResultSpec {
            nullable: self.nullable && other.nullable,
            fallible: self.fallible && other.fallible,
            values: self.values.intersect(other.values),
        }
    }

    /// Check if a particular value matches the spec.
    pub fn may_contain(&self, value: Datum<'a>) -> bool {
        if value == Datum::Null {
            return self.nullable;
        }

        self.values.may_contain(value)
    }

    /// Check if an error value matches the spec.
    pub fn may_fail(&self) -> bool {
        self.fallible
    }

    /// This method "maps" a function across the `ResultSpec`.
    ///
    /// As mentioned above, `ResultSpec` represents an approximate set of results.
    /// If we actually stored each result in the set, `flat_map` could be implemented by passing
    /// each result to the function one-by-one and unioning the resulting sets. This is possible
    /// when our values set is empty or contains a single datum, but when it contains a range,
    /// we can't enumerate all possible values of the set. We handle this by:
    /// - tracking whether the function is monotonic, in which case we can map the range by just
    ///   mapping the endpoints;
    /// - using a safe default when we can't infer a tighter bound on the set, eg. [Self::anything].
    fn flat_map(
        &self,
        is_monotonic: Monotonic,
        mut result_map: impl FnMut(Result<Datum<'a>, EvalError>) -> ResultSpec<'a>,
    ) -> ResultSpec<'a> {
        let null_spec = if self.nullable {
            result_map(Ok(Datum::Null))
        } else {
            ResultSpec::nothing()
        };

        let error_spec = if self.fallible {
            // Since we only care about whether / not an error is possible, and not the specific
            // error, create an arbitrary error here.
            // NOTE! This assumes that functions do not discriminate on the type of the error.
            result_map(Err(EvalError::Internal(String::new())))
        } else {
            ResultSpec::nothing()
        };

        let values_spec = match self.values {
            Values::Empty => ResultSpec::nothing(),
            // If this range contains a single datum, just call the function.
            Values::Within(min, max) if min == max => result_map(Ok(min)),
            // If this is a range of booleans, we know all the values... just try them.
            Values::Within(Datum::False, Datum::True) => {
                result_map(Ok(Datum::False)).union(result_map(Ok(Datum::True)))
            }
            // Otherwise, if our function is monotonic, we can try mapping the input
            // range to an output range.
            Values::Within(min, max) => match is_monotonic {
                Monotonic::Yes => {
                    let min_column = result_map(Ok(min));
                    let max_column = result_map(Ok(max));
                    if min_column.nullable
                        || min_column.fallible
                        || max_column.nullable
                        || max_column.fallible
                    {
                        ResultSpec::anything()
                    } else {
                        min_column.union(max_column)
                    }
                }
                Monotonic::Maybe | Monotonic::No => ResultSpec::anything(),
            },
            // TODO: we could return a narrower result for eg. `Values::Nested` with all-`Within` fields.
            Values::Nested(_) | Values::All => ResultSpec::anything(),
        };

        null_spec.union(error_spec).union(values_spec)
    }
}

/// [Abstract interpretation](https://en.wikipedia.org/wiki/Abstract_interpretation) for
/// [MirScalarExpr].
///
/// [MirScalarExpr::eval] implements a "concrete interpreter" for the expression type: given an
/// expression and specific column values as input, it returns a specific value for the output.
/// This could be reimplemented using this trait... but it's most useful for "abstract"
/// interpretations of the expression, where we generalize about sets of possible inputs and outputs.
/// See [Trace] and [ColumnSpecs] for how this can be useful in practice.
pub trait Interpreter {
    type Summary: Clone + Debug + Sized;

    /// A column of the input row.
    fn column(&self, id: usize) -> Self::Summary;

    /// A literal value.
    /// (Stored as a row, because we can't own a Datum.)
    fn literal(&self, result: &Result<Row, EvalError>, col_type: &ColumnType) -> Self::Summary;

    /// A call to an unmaterializable function.
    ///
    /// These functions cannot be evaluated by `MirScalarExpr::eval`. They must
    /// be transformed away by a higher layer.
    fn unmaterializable(&self, func: &UnmaterializableFunc) -> Self::Summary;

    /// A function call that takes one expression as an argument.
    fn unary(&self, func: &UnaryFunc, expr: Self::Summary) -> Self::Summary;

    /// A function call that takes two expressions as arguments.
    fn binary(&self, func: &BinaryFunc, left: Self::Summary, right: Self::Summary)
        -> Self::Summary;

    /// A function call that takes an arbitrary number of arguments.
    fn variadic(&self, func: &VariadicFunc, exprs: Vec<Self::Summary>) -> Self::Summary;

    /// Conditionally evaluated expressions.
    fn cond(&self, cond: Self::Summary, then: Self::Summary, els: Self::Summary) -> Self::Summary;

    /// Evaluate an entire expression, by delegating to the fine-grained methods on [Interpreter].
    fn expr(&self, expr: &MirScalarExpr) -> Self::Summary {
        match expr {
            MirScalarExpr::Column(id) => self.column(*id),
            MirScalarExpr::Literal(value, col_type) => self.literal(value, col_type),
            MirScalarExpr::CallUnmaterializable(func) => self.unmaterializable(func),
            MirScalarExpr::CallUnary { func, expr } => {
                let expr_range = self.expr(expr);
                self.unary(func, expr_range)
            }
            MirScalarExpr::CallBinary { func, expr1, expr2 } => {
                let expr1_range = self.expr(expr1);
                let expr2_range = self.expr(expr2);
                self.binary(func, expr1_range, expr2_range)
            }
            MirScalarExpr::CallVariadic { func, exprs } => {
                let exprs: Vec<_> = exprs.into_iter().map(|e| self.expr(e)).collect();
                self.variadic(func, exprs)
            }
            MirScalarExpr::If { cond, then, els } => {
                let cond_range = self.expr(cond);
                let then_range = self.expr(then);
                let els_range = self.expr(els);
                self.cond(cond_range, then_range, els_range)
            }
        }
    }

    /// Specifically, this evaluates the map and filters stages of an MFP: summarize each of the
    /// map expressions, then `and` together all of the filters.
    fn mfp_filter(&self, mfp: &MapFilterProject) -> Self::Summary {
        let mfp_eval = MfpEval::new(self, mfp.input_arity, &mfp.expressions);
        // NB: self should not be used after this point!
        let predicates = mfp
            .predicates
            .iter()
            .map(|(_, e)| mfp_eval.expr(e))
            .collect();
        mfp_eval.variadic(&VariadicFunc::And, predicates)
    }

    /// Similar to [Self::mfp_filter], but includes the additional temporal filters that have been
    /// broken out.
    fn mfp_plan_filter(&self, plan: &MfpPlan) -> Self::Summary {
        let mfp_eval = MfpEval::new(self, plan.mfp.input_arity, &plan.mfp.expressions);
        // NB: self should not be used after this point!
        let mut results: Vec<_> = plan
            .mfp
            .predicates
            .iter()
            .map(|(_, e)| mfp_eval.expr(e))
            .collect();
        let mz_now = mfp_eval.unmaterializable(&UnmaterializableFunc::MzNow);
        for bound in &plan.lower_bounds {
            let bound_range = mfp_eval.expr(bound);
            let result = mfp_eval.binary(&BinaryFunc::Lte, bound_range, mz_now.clone());
            results.push(result);
        }
        for bound in &plan.upper_bounds {
            let bound_range = mfp_eval.expr(bound);
            let result = mfp_eval.binary(&BinaryFunc::Gte, bound_range, mz_now.clone());
            results.push(result);
        }
        self.variadic(&VariadicFunc::And, results)
    }
}

/// Wrap another interpreter, but tack a few extra columns on at the end. An internal implementation
/// detail of `eval_mfp` and `eval_mfp_plan`.
struct MfpEval<'a, E: Interpreter + ?Sized> {
    evaluator: &'a E,
    input_arity: usize,
    expressions: Vec<E::Summary>,
}

impl<'a, E: Interpreter + ?Sized> MfpEval<'a, E> {
    fn new(evaluator: &'a E, input_arity: usize, expressions: &[MirScalarExpr]) -> Self {
        let mut mfp_eval = MfpEval {
            evaluator,
            input_arity,
            expressions: vec![],
        };
        for expr in expressions {
            let result = mfp_eval.expr(expr);
            mfp_eval.expressions.push(result);
        }
        mfp_eval
    }
}

impl<'a, E: Interpreter + ?Sized> Interpreter for MfpEval<'a, E> {
    type Summary = E::Summary;

    fn column(&self, id: usize) -> Self::Summary {
        if id < self.input_arity {
            self.evaluator.column(id)
        } else {
            self.expressions[id - self.input_arity].clone()
        }
    }

    fn literal(&self, result: &Result<Row, EvalError>, col_type: &ColumnType) -> Self::Summary {
        self.evaluator.literal(result, col_type)
    }

    fn unmaterializable(&self, func: &UnmaterializableFunc) -> Self::Summary {
        self.evaluator.unmaterializable(func)
    }

    fn unary(&self, func: &UnaryFunc, expr: Self::Summary) -> Self::Summary {
        self.evaluator.unary(func, expr)
    }

    fn binary(
        &self,
        func: &BinaryFunc,
        left: Self::Summary,
        right: Self::Summary,
    ) -> Self::Summary {
        self.evaluator.binary(func, left, right)
    }

    fn variadic(&self, func: &VariadicFunc, exprs: Vec<Self::Summary>) -> Self::Summary {
        self.evaluator.variadic(func, exprs)
    }

    fn cond(&self, cond: Self::Summary, then: Self::Summary, els: Self::Summary) -> Self::Summary {
        self.evaluator.cond(cond, then, els)
    }
}

/// For a given column and expression, does the range of the column influence the range of the
/// expression? (ie. if the expression is a filter, could we tell that the filter
/// will never return True?)
#[derive(Ord, PartialOrd, Eq, PartialEq, Copy, Clone, Debug)]
pub enum Pushdownable {
    /// We don't have any information to push down filters on this column:
    /// either it's not referenced at all, or we've applied some non-monotonic function that we can't
    /// push filters through. (This is the default.)
    No,
    /// We may or may not be able to push down filters on the given column. (The expression
    /// includes a function we haven't classified yet.)
    Maybe,
    /// We can definitely push down a filter on this column. (The expression references the column
    /// in such a way that the range of possible values of the column affects the range of output
    /// values of the expression in a particular way.)
    Yes,
}

/// Maps column identifiers to pushdown information. We can't tell how many columns are in the
/// relation just from inspecting the expression, so the `Vec` may not include every column; an entry
/// of `No` is treated the same as the entry not being present.
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct RelationTrace(pub Vec<Pushdownable>);

/// An interpreter that traces how information about the source columns flows through the expression.
#[derive(Debug)]
pub struct Trace;

impl RelationTrace {
    pub fn new() -> RelationTrace {
        RelationTrace(vec![])
    }

    pub fn get(&mut self, id: usize) -> Pushdownable {
        self.0.get(id).copied().unwrap_or(Pushdownable::No)
    }

    pub fn set(&mut self, id: usize, value: Pushdownable) {
        if self.0.len() <= id {
            self.0.resize(id + 1, Pushdownable::No);
        }
        self.0[id] = value;
    }

    fn apply_fn(mut self, is_monotonic: Monotonic) -> Self {
        match is_monotonic {
            Monotonic::Yes => {
                // Still correlated, nothing to do.
            }
            Monotonic::No => {
                // A function that we know we can't see through. No reason to expect the range
                // of the columns to affect the range of the expression.
                for col in &mut self.0 {
                    *col = Pushdownable::No;
                }
            }
            Monotonic::Maybe => {
                // A function that we may or may not be able to see through.
                for col in &mut self.0 {
                    *col = Pushdownable::Maybe.min(*col);
                }
            }
        }
        self
    }

    fn union(mut self, other: RelationTrace) -> RelationTrace {
        for (id, pushdownable) in other.0.into_iter().enumerate() {
            let pushdownable = self.get(id).max(pushdownable);
            self.set(id, pushdownable);
        }
        self
    }
}

impl Interpreter for Trace {
    /// For every column in the data, we track whether or not that column is "pusdownable".
    /// (If the column is not present in the summary, we default to `false`.
    type Summary = RelationTrace;

    fn column(&self, id: usize) -> Self::Summary {
        let mut trace = RelationTrace::new();
        trace.set(id, Pushdownable::Yes);
        trace
    }

    fn literal(&self, _result: &Result<Row, EvalError>, _col_type: &ColumnType) -> Self::Summary {
        RelationTrace::new()
    }

    fn unmaterializable(&self, _func: &UnmaterializableFunc) -> Self::Summary {
        RelationTrace::new()
    }

    fn unary(&self, func: &UnaryFunc, expr: Self::Summary) -> Self::Summary {
        expr.apply_fn(unary_monotonic(func))
    }

    fn binary(
        &self,
        func: &BinaryFunc,
        left: Self::Summary,
        right: Self::Summary,
    ) -> Self::Summary {
        // TODO: this is duplicative! If we have more than one or two special-cased functions
        // we should find some way to share the list between `Trace` and `ColumnSpecs`.
        let (left_monotonic, right_monotonic) = match func {
            BinaryFunc::JsonbGetString { stringify: false } => (Monotonic::Yes, Monotonic::No),
            _ => binary_monotonic(func),
        };
        left.apply_fn(left_monotonic)
            .union(right.apply_fn(right_monotonic))
    }

    fn variadic(&self, func: &VariadicFunc, exprs: Vec<Self::Summary>) -> Self::Summary {
        let is_monotonic = variadic_monotonic(func);
        exprs
            .into_iter()
            .fold(RelationTrace::new(), |acc, columns| {
                acc.union(columns.apply_fn(is_monotonic))
            })
    }

    fn cond(&self, cond: Self::Summary, then: Self::Summary, els: Self::Summary) -> Self::Summary {
        cond.union(then.union(els))
    }
}

#[derive(Clone, Debug)]
pub struct ColumnSpec<'a> {
    pub col_type: ColumnType,
    pub range: ResultSpec<'a>,
}

/// An interpreter that:
/// - stores both the type and the range of possible values for every column and
///   unmaterializable function. (See the `push_` methods.)
/// - given an expression (or MFP, etc.), returns the range of possible results that evaluating that
///   expression might have. (See the `eval_` methods.)
#[derive(Clone, Debug)]
pub struct ColumnSpecs<'a> {
    pub relation: &'a RelationType,
    pub columns: Vec<ResultSpec<'a>>,
    pub unmaterializables: BTreeMap<UnmaterializableFunc, ResultSpec<'a>>,
    pub arena: &'a RowArena,
}

impl<'a> ColumnSpecs<'a> {
    /// Create a new, empty set of column specs. (Initially, the only assumption we make about the
    /// data in the column is that it matches the type.)
    pub fn new(relation: &'a RelationType, arena: &'a RowArena) -> Self {
        let columns = relation
            .column_types
            .iter()
            .map(|ct| ResultSpec::has_type(ct, false))
            .collect();
        ColumnSpecs {
            relation,
            columns,
            unmaterializables: Default::default(),
            arena,
        }
    }

    /// Restrict the set of possible values in a given column. (By intersecting it with the existing
    /// spec.)
    pub fn push_column(&mut self, id: usize, update: ResultSpec<'a>) {
        let range = self.columns.get_mut(id).expect("valid column id");
        *range = range.clone().intersect(update);
    }

    /// Restrict the set of possible values a given unmaterializable func might return. (By
    /// intersecting it with the existing spec.)
    pub fn push_unmaterializable(&mut self, func: UnmaterializableFunc, update: ResultSpec<'a>) {
        let range = self
            .unmaterializables
            .entry(func.clone())
            .or_insert_with(|| ResultSpec::has_type(&func.output_type(), true));
        *range = range.clone().intersect(update);
    }

    fn eval_result<'b, E>(&self, result: Result<Datum<'b>, E>) -> ResultSpec<'a> {
        match result {
            Ok(Datum::Null) => ResultSpec {
                nullable: true,
                ..ResultSpec::nothing()
            },
            Ok(d) => ResultSpec {
                values: Values::just(self.arena.make_datum(|packer| packer.push(d))),
                ..ResultSpec::nothing()
            },
            Err(_) => ResultSpec {
                fallible: true,
                ..ResultSpec::nothing()
            },
        }
    }

    fn set_literal(expr: &mut MirScalarExpr, update: Result<Datum, EvalError>) {
        match expr {
            MirScalarExpr::Literal(literal, col_type) => match update {
                Err(error) => *literal = Err(error),
                Ok(datum) => {
                    assert!(
                        datum.is_instance_of(col_type),
                        "{datum:?} must be an instance of {col_type:?}"
                    );
                    match literal {
                        // Reuse the allocation if we can
                        Ok(row) => row.packer().push(datum),
                        literal => *literal = Ok(Row::pack_slice(&[datum])),
                    }
                }
            },
            _ => panic!("not a literal"),
        }
    }

    fn set_argument(expr: &mut MirScalarExpr, arg: usize, value: Result<Datum, EvalError>) {
        match (expr, arg) {
            (MirScalarExpr::CallUnary { expr, .. }, 0) => Self::set_literal(expr, value),
            (MirScalarExpr::CallBinary { expr1, .. }, 0) => Self::set_literal(expr1, value),
            (MirScalarExpr::CallBinary { expr2, .. }, 1) => Self::set_literal(expr2, value),
            (MirScalarExpr::CallVariadic { exprs, .. }, n) if n < exprs.len() => {
                Self::set_literal(&mut exprs[n], value)
            }
            _ => panic!("illegal argument for expression"),
        }
    }

    /// A literal with the given type and a trivial default value. Callers should ensure that
    /// [Self::set_literal] is called on the resulting expression to give it a meaningful value
    /// before evaluating.
    fn placeholder(col_type: ColumnType) -> MirScalarExpr {
        MirScalarExpr::Literal(Err(EvalError::Internal("".to_owned())), col_type)
    }
}

impl<'a> Interpreter for ColumnSpecs<'a> {
    type Summary = ColumnSpec<'a>;

    fn column(&self, id: usize) -> Self::Summary {
        let col_type = self.relation.column_types[id].clone();
        let range = self.columns[id].clone();
        ColumnSpec { col_type, range }
    }

    fn literal(&self, result: &Result<Row, EvalError>, col_type: &ColumnType) -> Self::Summary {
        let col_type = col_type.clone();
        let range = self.eval_result(result.as_ref().map(|row| {
            self.arena
                .make_datum(|packer| packer.push(row.unpack_first()))
        }));
        ColumnSpec { col_type, range }
    }

    fn unmaterializable(&self, func: &UnmaterializableFunc) -> Self::Summary {
        let col_type = func.output_type();
        let range = self
            .unmaterializables
            .get(func)
            .cloned()
            .unwrap_or(ResultSpec::has_type(&func.output_type(), true));
        ColumnSpec { col_type, range }
    }

    fn unary(&self, func: &UnaryFunc, summary: Self::Summary) -> Self::Summary {
        let is_monotonic = unary_monotonic(func);
        let mut expr = MirScalarExpr::CallUnary {
            func: func.clone(),
            expr: Box::new(Self::placeholder(summary.col_type.clone())),
        };
        let mapped_spec = summary.range.flat_map(is_monotonic, |datum| {
            Self::set_argument(&mut expr, 0, datum);
            self.eval_result(expr.eval(&[], self.arena))
        });
        let col_type = func.output_type(summary.col_type);

        let range = mapped_spec.intersect(ResultSpec::has_type(&col_type, true));

        ColumnSpec { col_type, range }
    }

    fn binary(
        &self,
        func: &BinaryFunc,
        left: Self::Summary,
        right: Self::Summary,
    ) -> Self::Summary {
        let (left_monotonic, right_monotonic) = binary_monotonic(func);

        let special_spec = match func {
            BinaryFunc::JsonbGetString { stringify } => {
                let values_spec = match (&left.range.values, &right.range.values) {
                    (Values::Nested(map_spec), Values::Within(key, key2)) if key == key2 => {
                        map_spec
                            .get(key)
                            .cloned()
                            .map_or_else(ResultSpec::anything, |field_spec| {
                                if *stringify {
                                    // We only preserve value-range information when stringification
                                    // is a noop. (Common in real queries.)
                                    let values = match field_spec.values {
                                        Values::Empty => Values::Empty,
                                        Values::Within(
                                            min @ Datum::String(_),
                                            max @ Datum::String(_),
                                        ) => Values::Within(min, max),
                                        Values::Within(_, _) | Values::Nested(_) | Values::All => {
                                            Values::All
                                        }
                                    };
                                    ResultSpec {
                                        values,
                                        ..field_spec
                                    }
                                } else {
                                    field_spec
                                }
                            })
                    }
                    _ => ResultSpec::anything(),
                };
                ResultSpec {
                    nullable: left.range.nullable || right.range.nullable || values_spec.nullable,
                    fallible: left.range.fallible || right.range.fallible || values_spec.fallible,
                    values: values_spec.values,
                }
            }
            _ => ResultSpec::anything(),
        };

        let mut expr = MirScalarExpr::CallBinary {
            func: func.clone(),
            expr1: Box::new(Self::placeholder(left.col_type.clone())),
            expr2: Box::new(Self::placeholder(right.col_type.clone())),
        };
        let mapped_spec = left.range.flat_map(left_monotonic, |left_result| {
            Self::set_argument(&mut expr, 0, left_result);
            right.range.flat_map(right_monotonic, |right_result| {
                Self::set_argument(&mut expr, 1, right_result);
                self.eval_result(expr.eval(&[], self.arena))
            })
        });

        let col_type = func.output_type(left.col_type, right.col_type);

        let range = mapped_spec
            .intersect(ResultSpec::has_type(&col_type, true))
            .intersect(special_spec);
        ColumnSpec { col_type, range }
    }

    fn variadic(&self, func: &VariadicFunc, args: Vec<Self::Summary>) -> Self::Summary {
        fn eval_loop<'a>(
            is_monotonic: Monotonic,
            expr: &mut MirScalarExpr,
            args: &[ColumnSpec<'a>],
            index: usize,
            datum_map: &mut impl FnMut(&MirScalarExpr) -> ResultSpec<'a>,
        ) -> ResultSpec<'a> {
            if index >= args.len() {
                datum_map(expr)
            } else {
                args[index].range.flat_map(is_monotonic, |datum| {
                    ColumnSpecs::set_argument(expr, index, datum);
                    eval_loop(is_monotonic, expr, args, index + 1, datum_map)
                })
            }
        }

        let mut fn_expr = MirScalarExpr::CallVariadic {
            func: func.clone(),
            exprs: args
                .iter()
                .map(|spec| Self::placeholder(spec.col_type.clone()))
                .collect(),
        };
        let mapped_spec = eval_loop(
            variadic_monotonic(func),
            &mut fn_expr,
            &args,
            0,
            &mut |expr| self.eval_result(expr.eval(&[], self.arena)),
        );

        let col_types = args.into_iter().map(|spec| spec.col_type).collect();
        let col_type = func.output_type(col_types);

        let range = mapped_spec.intersect(ResultSpec::has_type(&col_type, true));

        ColumnSpec { col_type, range }
    }

    fn cond(&self, cond: Self::Summary, then: Self::Summary, els: Self::Summary) -> Self::Summary {
        let col_type = ColumnType {
            scalar_type: then.col_type.scalar_type,
            nullable: then.col_type.nullable || els.col_type.nullable,
        };

        let range = cond
            .range
            .flat_map(Monotonic::Yes, |datum| match datum {
                Ok(Datum::True) => then.range.clone(),
                Ok(Datum::False) => els.range.clone(),
                _ => ResultSpec::fails(),
            })
            .intersect(ResultSpec::has_type(&col_type, true));

        ColumnSpec { col_type, range }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use proptest::sample::{select, Index};

    use mz_repr::{Datum, PropDatum, RowArena, ScalarType};

    use crate::func::*;
    use crate::{BinaryFunc, MirScalarExpr, UnaryFunc};

    use super::*;

    #[derive(Debug)]
    struct ExpressionData {
        relation_type: RelationType,
        rows: Vec<Row>,
        expr: MirScalarExpr,
    }

    // Currently there's no good way to check whether a particular function accepts a particular
    // type as argument, which means we need to list everything out explicitly here. Restrict our interest
    // to a reasonable number of functions, to keep things tractable
    // TODO: replace this with function-level info once it's available.
    const NUM_TYPE: ScalarType = ScalarType::Numeric { max_scale: None };
    static SCALAR_TYPES: &[ScalarType] = &[
        ScalarType::Bool,
        ScalarType::Jsonb,
        NUM_TYPE,
        ScalarType::Timestamp,
        ScalarType::MzTimestamp,
        ScalarType::String,
    ];

    const INTERESTING_UNARY_FUNCS: &[UnaryFunc] = {
        &[
            UnaryFunc::CastNumericToMzTimestamp(CastNumericToMzTimestamp),
            UnaryFunc::NegNumeric(NegNumeric),
            UnaryFunc::CastJsonbToNumeric(CastJsonbToNumeric(None)),
            UnaryFunc::CastJsonbToBool(CastJsonbToBool),
            UnaryFunc::CastJsonbToString(CastJsonbToString),
            UnaryFunc::DateTruncTimestamp(DateTruncTimestamp(DateTimeUnits::Epoch)),
            UnaryFunc::ExtractTimestamp(ExtractTimestamp(DateTimeUnits::Epoch)),
            UnaryFunc::Not(Not),
            UnaryFunc::IsNull(IsNull),
            UnaryFunc::IsFalse(IsFalse),
        ]
    };

    fn unary_typecheck(func: &UnaryFunc, arg: &ColumnType) -> bool {
        use UnaryFunc::*;
        match func {
            CastNumericToMzTimestamp(_) | NegNumeric(_) => arg.scalar_type.base_eq(&NUM_TYPE),
            CastJsonbToNumeric(_) | CastJsonbToBool(_) | CastJsonbToString(_) => {
                arg.scalar_type.base_eq(&ScalarType::Jsonb)
            }
            ExtractTimestamp(_) | DateTruncTimestamp(_) => {
                arg.scalar_type.base_eq(&ScalarType::Timestamp)
            }
            Not(_) => arg.scalar_type.base_eq(&ScalarType::Bool),
            IsNull(_) => true,
            _ => false,
        }
    }

    const INTERESTING_BINARY_FUNCS: &[BinaryFunc] = {
        use BinaryFunc::*;
        &[
            AddTimestampInterval,
            AddNumeric,
            SubNumeric,
            MulNumeric,
            DivNumeric,
            Eq,
            Lt,
            Gt,
            Lte,
            Gte,
            DateTruncTimestamp,
            JsonbGetString { stringify: true },
            JsonbGetString { stringify: false },
        ]
    };

    fn binary_typecheck(func: &BinaryFunc, arg0: &ColumnType, arg1: &ColumnType) -> bool {
        use BinaryFunc::*;
        match func {
            AddTimestampInterval => {
                arg0.scalar_type.base_eq(&ScalarType::Timestamp)
                    && arg1.scalar_type.base_eq(&ScalarType::Interval)
            }
            AddNumeric | SubNumeric | MulNumeric | DivNumeric => {
                arg0.scalar_type.base_eq(&NUM_TYPE) && arg1.scalar_type.base_eq(&NUM_TYPE)
            }
            Eq | Lt | Gt | Lte | Gte => arg0.scalar_type.base_eq(&arg1.scalar_type),
            DateTruncTimestamp => {
                arg0.scalar_type.base_eq(&ScalarType::String)
                    && arg1.scalar_type.base_eq(&ScalarType::Timestamp)
            }
            JsonbGetString { .. } => {
                arg0.scalar_type.base_eq(&ScalarType::Jsonb)
                    && arg1.scalar_type.base_eq(&ScalarType::String)
            }
            _ => false,
        }
    }

    const INTERESTING_VARIADIC_FUNCS: &[VariadicFunc] = {
        use VariadicFunc::*;
        &[Coalesce, Greatest, Least, And, Or]
    };

    fn variadic_typecheck(func: &VariadicFunc, args: &[ColumnType]) -> bool {
        use VariadicFunc::*;
        match func {
            Coalesce | Greatest | Least => match args {
                [] => true,
                [first, rest @ ..] => rest
                    .iter()
                    .all(|t| first.scalar_type.base_eq(&t.scalar_type)),
            },
            And | Or => args
                .iter()
                .all(|t| t.scalar_type.base_eq(&ScalarType::Bool)),
            _ => false,
        }
    }

    fn gen_datums_for_type(typ: &ColumnType) -> BoxedStrategy<Datum<'static>> {
        let mut values: Vec<Datum<'static>> = typ.scalar_type.interesting_datums().collect();
        if typ.nullable {
            values.push(Datum::Null)
        }
        select(values).boxed()
    }

    fn gen_column() -> impl Strategy<Value = (ColumnType, Datum<'static>)> {
        select(SCALAR_TYPES)
            .prop_map(|t| t.nullable(true))
            .prop_filter("need at least one value", |c| {
                c.scalar_type.interesting_datums().count() > 0
            })
            .prop_flat_map(|col| {
                gen_datums_for_type(&col).prop_map(move |datum| (col.clone(), datum))
            })
    }

    fn gen_expr_for_relation(
        relation: &RelationType,
    ) -> BoxedStrategy<(MirScalarExpr, ColumnType)> {
        let column_gen = {
            let column_types = relation.column_types.clone();
            any::<Index>()
                .prop_map(move |idx| {
                    let id = idx.index(column_types.len());
                    (MirScalarExpr::Column(id), column_types[id].clone())
                })
                .boxed()
        };

        let literal_gen = select(SCALAR_TYPES)
            .prop_map(|s| s.nullable(true))
            .prop_flat_map(|ct| {
                let error_gen = any::<EvalError>().prop_map(Err).boxed();
                let value_gen = gen_datums_for_type(&ct)
                    .prop_map(move |datum| Ok(Row::pack_slice(&[datum])))
                    .boxed();
                error_gen.prop_union(value_gen).prop_map(move |result| {
                    (MirScalarExpr::Literal(result, ct.clone()), ct.clone())
                })
            })
            .boxed();

        column_gen
            .prop_union(literal_gen)
            .prop_recursive(4, 64, 8, |self_gen| {
                let unary_gen = (select(INTERESTING_UNARY_FUNCS), self_gen.clone())
                    .prop_filter_map("unary func", |(func, (expr_in, type_in))| {
                        if !unary_typecheck(&func, &type_in) {
                            return None;
                        }
                        let type_out = func.output_type(type_in);
                        let expr_out = MirScalarExpr::CallUnary {
                            func,
                            expr: Box::new(expr_in),
                        };
                        Some((expr_out, type_out))
                    })
                    .boxed();
                let binary_gen = (
                    select(INTERESTING_BINARY_FUNCS),
                    self_gen.clone(),
                    self_gen.clone(),
                )
                    .prop_filter_map(
                        "binary func",
                        |(func, (expr_left, type_left), (expr_right, type_right))| {
                            if !binary_typecheck(&func, &type_left, &type_right) {
                                return None;
                            }
                            let type_out = func.output_type(type_left, type_right);
                            let expr_out = MirScalarExpr::CallBinary {
                                func,
                                expr1: Box::new(expr_left),
                                expr2: Box::new(expr_right),
                            };
                            Some((expr_out, type_out))
                        },
                    )
                    .boxed();
                let variadic_gen = (
                    select(INTERESTING_VARIADIC_FUNCS),
                    prop::collection::vec(self_gen, 1..4),
                )
                    .prop_filter_map("variadic func", |(func, exprs)| {
                        let (exprs_in, type_in): (_, Vec<_>) = exprs.into_iter().unzip();
                        if !variadic_typecheck(&func, &type_in) {
                            return None;
                        }
                        let type_out = func.output_type(type_in);
                        let expr_out = MirScalarExpr::CallVariadic {
                            func,
                            exprs: exprs_in,
                        };
                        Some((expr_out, type_out))
                    })
                    .boxed();

                unary_gen
                    .prop_union(binary_gen)
                    .boxed()
                    .prop_union(variadic_gen)
            })
            .boxed()
    }

    fn gen_expr_data() -> impl Strategy<Value = ExpressionData> {
        let columns = prop::collection::vec(gen_column(), 1..10);
        columns.prop_flat_map(|data| {
            let (columns, datums): (Vec<_>, Vec<_>) = data.into_iter().unzip();
            let relation = RelationType::new(columns);
            let row = Row::pack_slice(&datums);
            gen_expr_for_relation(&relation).prop_map(move |(expr, _)| ExpressionData {
                relation_type: relation.clone(),
                rows: vec![row.clone()],
                expr,
            })
        })
    }

    #[test]
    fn test_trivial_spec_matches() {
        fn check(datum: PropDatum) -> Result<(), TestCaseError> {
            let datum: Datum = (&datum).into();
            let spec = if datum.is_null() {
                ResultSpec::null()
            } else {
                ResultSpec::value(datum)
            };
            assert!(spec.may_contain(datum));
            Ok(())
        }

        proptest!(|(datum in mz_repr::arb_datum())| {
            check(datum)?;
        });

        assert!(ResultSpec::fails().may_fail());
    }

    #[test]
    fn test_equivalence() {
        fn check(data: ExpressionData) -> Result<(), TestCaseError> {
            let ExpressionData {
                relation_type,
                rows,
                expr,
            } = data;
            let num_cols = relation_type.column_types.len();

            // We want to ensure that the spec we get when evaluating an expression using
            // `ColumnSpecs` always contains the _actual_ value of that column when evaluated with
            // eval. (This is an important correctness property of abstract interpretation.)
            let arena = RowArena::new();
            let mut interpreter = ColumnSpecs::new(&relation_type, &arena);
            let mut specs = vec![ResultSpec::nothing(); num_cols];
            for row in &rows {
                for (id, datum) in row.iter().enumerate() {
                    specs[id] = specs[id].clone().union(ResultSpec::value(datum));
                }
            }
            for (id, spec) in specs.into_iter().enumerate() {
                interpreter.push_column(id, spec);
            }

            let spec = interpreter.expr(&expr);

            for row in &rows {
                let datums: Vec<_> = row.iter().collect();
                let eval_result = expr.eval(&datums, &arena);
                match eval_result {
                    Ok(value) => {
                        assert!(spec.range.may_contain(value))
                    }
                    Err(_) => {
                        assert!(spec.range.may_fail());
                    }
                }
            }

            Ok(())
        }

        proptest!(|(data in gen_expr_data())| {
            check(data)?;
        });
    }

    #[test]
    fn test_eval_range() {
        // Example inspired by the tumbling windows temporal filter in the docs
        let period_ms = MirScalarExpr::Literal(
            Ok(Row::pack_slice(&[Datum::Int64(10)])),
            ScalarType::Int64.nullable(false),
        );
        let expr = MirScalarExpr::CallBinary {
            func: BinaryFunc::Gte,
            expr1: Box::new(MirScalarExpr::CallUnmaterializable(
                UnmaterializableFunc::MzNow,
            )),
            expr2: Box::new(MirScalarExpr::CallUnary {
                func: UnaryFunc::CastInt64ToMzTimestamp(CastInt64ToMzTimestamp),
                expr: Box::new(MirScalarExpr::CallBinary {
                    func: BinaryFunc::MulInt64,
                    expr1: Box::new(period_ms.clone()),
                    expr2: Box::new(MirScalarExpr::CallBinary {
                        func: BinaryFunc::DivInt64,
                        expr1: Box::new(MirScalarExpr::Column(0)),
                        expr2: Box::new(period_ms),
                    }),
                }),
            }),
        };
        let relation = RelationType::new(vec![ScalarType::Int64.nullable(false)]);

        {
            // Non-overlapping windows
            let arena = RowArena::new();
            let mut interpreter = ColumnSpecs::new(&relation, &arena);
            interpreter.push_unmaterializable(
                UnmaterializableFunc::MzNow,
                ResultSpec::value_between(
                    Datum::MzTimestamp(10.into()),
                    Datum::MzTimestamp(20.into()),
                ),
            );
            interpreter.push_column(0, ResultSpec::value_between(30i64.into(), 40i64.into()));

            let range_out = interpreter.expr(&expr).range;
            assert!(range_out.may_contain(Datum::False));
            assert!(!range_out.may_contain(Datum::True));
            assert!(!range_out.may_contain(Datum::Null));
            assert!(!range_out.may_fail());
        }

        {
            // Overlapping windows
            let arena = RowArena::new();
            let mut interpreter = ColumnSpecs::new(&relation, &arena);
            interpreter.push_unmaterializable(
                UnmaterializableFunc::MzNow,
                ResultSpec::value_between(
                    Datum::MzTimestamp(10.into()),
                    Datum::MzTimestamp(35.into()),
                ),
            );
            interpreter.push_column(0, ResultSpec::value_between(30i64.into(), 40i64.into()));

            let range_out = interpreter.expr(&expr).range;
            assert!(range_out.may_contain(Datum::False));
            assert!(range_out.may_contain(Datum::True));
            assert!(!range_out.may_contain(Datum::Null));
            assert!(!range_out.may_fail());
        }
    }

    #[test]
    fn test_jsonb() {
        let arena = RowArena::new();

        let expr = MirScalarExpr::CallUnary {
            func: UnaryFunc::CastJsonbToNumeric(CastJsonbToNumeric(None)),
            expr: Box::new(MirScalarExpr::CallBinary {
                func: BinaryFunc::JsonbGetString { stringify: false },
                expr1: Box::new(MirScalarExpr::Column(0)),
                expr2: Box::new(MirScalarExpr::Literal(
                    Ok(Row::pack_slice(&["ts".into()])),
                    ScalarType::String.nullable(false),
                )),
            }),
        };

        let relation = RelationType::new(vec![ScalarType::Jsonb.nullable(true)]);
        let mut interpreter = ColumnSpecs::new(&relation, &arena);
        interpreter.push_column(
            0,
            ResultSpec::map_spec(
                [(
                    "ts".into(),
                    ResultSpec::value_between(
                        Datum::Numeric(100.into()),
                        Datum::Numeric(300.into()),
                    ),
                )]
                .into_iter()
                .collect(),
            ),
        );

        let range_out = interpreter.expr(&expr).range;
        assert!(!range_out.may_contain(Datum::Numeric(0.into())));
        assert!(range_out.may_contain(Datum::Numeric(200.into())));
        assert!(!range_out.may_contain(Datum::Numeric(400.into())));
    }

    #[test]
    fn test_trace() {
        use super::Trace;

        let expr = MirScalarExpr::CallBinary {
            func: BinaryFunc::Gte,
            expr1: Box::new(MirScalarExpr::Column(0)),
            expr2: Box::new(MirScalarExpr::CallBinary {
                func: BinaryFunc::AddInt64,
                expr1: Box::new(MirScalarExpr::Column(1)),
                expr2: Box::new(MirScalarExpr::CallUnary {
                    func: UnaryFunc::AbsInt64(AbsInt64),
                    expr: Box::new(MirScalarExpr::Column(3)),
                }),
            }),
        };
        let trace = Trace.expr(&expr);
        assert_eq!(
            trace,
            RelationTrace(vec![
                Pushdownable::Yes, // one side of a conditional
                Pushdownable::Yes, // other side of the conditional, nested in +
                Pushdownable::No,  // not referenced!
                Pushdownable::No   // referenced, but we can't see through abs()
            ])
        )
    }
}