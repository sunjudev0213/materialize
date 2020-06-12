// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! TBD: Currently, `sql::func` handles matching arguments to their respective
//! built-in functions (for most built-in functions, at least).

use std::collections::HashMap;
use std::fmt;

use failure::bail;
use lazy_static::lazy_static;
use repr::ScalarType;
use sql_parser::ast::{BinaryOperator, Expr};

use super::cast::{rescale_decimal, CastTo};
use super::expr::{BinaryFunc, CoercibleScalarExpr, ScalarExpr, UnaryFunc, VariadicFunc};
use super::query::{CoerceTo, ExprContext};
use crate::unsupported;

#[derive(Clone, Debug, Eq, PartialEq)]
/// Mirrored from [PostgreSQL's `typcategory`][typcategory].
///
/// Note that Materialize also uses a number of pseudotypes when planning, but
/// we have yet to need to integrate them with `TypeCategory`.
///
/// [typcategory]:
/// https://www.postgresql.org/docs/9.6/catalog-pg-type.html#CATALOG-TYPCATEGORY-TABLE
pub enum TypeCategory {
    Bool,
    DateTime,
    Numeric,
    String,
    Timespan,
    UserDefined,
}

impl TypeCategory {
    /// Extracted from PostgreSQL 9.6.
    /// ```ignore
    /// SELECT array_agg(typname), typcategory
    /// FROM pg_catalog.pg_type
    /// WHERE typname IN (
    ///  'bool', 'bytea', 'date', 'float4', 'float8', 'int4', 'int8', 'interval', 'jsonb',
    ///  'numeric', 'text', 'time', 'timestamp', 'timestamptz'
    /// )
    /// GROUP BY typcategory
    /// ORDER BY typcategory;
    /// ```
    fn from_type(typ: &ScalarType) -> TypeCategory {
        match typ {
            ScalarType::Bool => TypeCategory::Bool,
            ScalarType::Bytes | ScalarType::Jsonb | ScalarType::List(_) => {
                TypeCategory::UserDefined
            }
            ScalarType::Date
            | ScalarType::Time
            | ScalarType::Timestamp
            | ScalarType::TimestampTz => TypeCategory::DateTime,
            ScalarType::Decimal(..)
            | ScalarType::Float32
            | ScalarType::Float64
            | ScalarType::Int32
            | ScalarType::Int64 => TypeCategory::Numeric,
            ScalarType::Interval => TypeCategory::Timespan,
            ScalarType::String => TypeCategory::String,
        }
    }

    /// Extracted from PostgreSQL 9.6.
    /// ```ignore
    /// SELECT typcategory, typname, typispreferred
    /// FROM pg_catalog.pg_type
    /// WHERE typispreferred = true
    /// ORDER BY typcategory;
    /// ```
    fn preferred_type(&self) -> Option<ScalarType> {
        match self {
            TypeCategory::Bool => Some(ScalarType::Bool),
            TypeCategory::DateTime => Some(ScalarType::TimestampTz),
            TypeCategory::Numeric => Some(ScalarType::Float64),
            TypeCategory::String => Some(ScalarType::String),
            TypeCategory::Timespan => Some(ScalarType::Interval),
            TypeCategory::UserDefined => None,
        }
    }
}

#[derive(Debug, Clone)]
/// Describes a single function's implementation.
pub struct FuncImpl {
    params: ParamList,
    op: OperationType,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
/// Describes possible types of function parameters.
///
/// Note that this is not exhaustive and will likely require additions.
pub enum ParamList {
    Exact(Vec<ParamType>),
    Repeat(Vec<ParamType>),
}

impl ParamList {
    /// Validates that the number of input elements are viable for this set of
    /// parameters.
    fn validate_arg_len(&self, input_len: usize) -> bool {
        match self {
            Self::Exact(p) => p.len() == input_len,
            Self::Repeat(p) => input_len % p.len() == 0 && input_len > 0,
        }
    }

    /// Matches a `&[ScalarType]` derived from the user's function argument
    /// against this `ParamList`'s permitted arguments.
    fn match_scalartypes(&self, types: &[ScalarType]) -> bool {
        use ParamList::*;
        match self {
            Exact(p) => p.iter().zip(types.iter()).all(|(p, t)| p.accepts(t)),
            Repeat(p) => types
                .iter()
                .enumerate()
                .all(|(i, t)| p[i % p.len()].accepts(t)),
        }
    }
}

/// Provides a shorthand function for writing `ParamList::Exact`.
impl From<Vec<ParamType>> for ParamList {
    fn from(p: Vec<ParamType>) -> ParamList {
        ParamList::Exact(p)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
/// Describes parameter types; these are essentially just `ScalarType` with some
/// added flexibility.
pub enum ParamType {
    Plain(ScalarType),
    /// A pseudotype permitting any type, but requires it to be cast to a `ScalarType::String`.
    StringAny,
    /// A pseudotype permitting any type, but requires it to be cast to a
    /// [`ScalarType::Jsonb`], or an element within a `Jsonb`.
    JsonbAny,
}

impl ParamType {
    // Does `self` accept arguments of type `t`?
    fn accepts(&self, t: &ScalarType) -> bool {
        match (self, t) {
            (ParamType::Plain(s), o) => *s == o.desaturate(),
            (ParamType::StringAny, _) | (ParamType::JsonbAny, _) => true,
        }
    }

    // Does `t`'s [`TypeCategory`] prefer `self`? This question can make
    // more sense with the understanding that pseudotypes are never preferred.
    fn is_preferred_by(&self, t: &ScalarType) -> bool {
        if let Some(pt) = TypeCategory::from_type(t).preferred_type() {
            *self == pt
        } else {
            false
        }
    }
}

impl PartialEq<ScalarType> for ParamType {
    fn eq(&self, other: &ScalarType) -> bool {
        match (self, other) {
            (ParamType::Plain(s), o) => *s == o.desaturate(),
            // Pseudotypes do not equal concrete types.
            (ParamType::StringAny, _) | (ParamType::JsonbAny, _) => false,
        }
    }
}

impl PartialEq<ParamType> for ScalarType {
    fn eq(&self, other: &ParamType) -> bool {
        other == self
    }
}

impl From<&ParamType> for ScalarType {
    fn from(p: &ParamType) -> ScalarType {
        match p {
            ParamType::Plain(s) => s.clone(),
            ParamType::StringAny => ScalarType::String,
            ParamType::JsonbAny => ScalarType::Jsonb,
        }
    }
}

pub trait Params {
    fn into_params(self) -> Vec<ParamType>;
}

/// Provides a shorthand for converting a `Vec<ScalarType>` to `Vec<ParamType>`.
/// Note that this moves the values out of `self` and into the resultant `Vec<ParamType>`.
impl Params for Vec<ScalarType> {
    fn into_params(self) -> Vec<ParamType> {
        self.into_iter().map(ParamType::Plain).collect()
    }
}

#[derive(Clone)]
/// Represents generalizable operations that can return [`ScalarExpr`]s.
pub enum OperationType {
    /// Returns the `ScalarExpr` that is output from
    /// `ArgImplementationMatcher::generate_param_exprs`.
    ExprOnly,
    Unary(UnaryFunc),
    /// Embeds a [`BinaryFunc`]
    BFunc(BinaryFunc),
    /// Embeds a binary-operator-like function pointer.
    BClosure(fn(&ExprContext, ScalarExpr, ScalarExpr) -> ScalarExpr),
    VFunc(VariadicFunc),
    VClosure(fn(&ExprContext, Vec<ScalarExpr>) -> ScalarExpr),
}

impl fmt::Debug for OperationType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            OperationType::ExprOnly => f.write_str("ExprOnly"),
            OperationType::Unary(func) => write!(f, "Unary({:?})", func),
            OperationType::BFunc(func) => write!(f, "BFunc({:?})", func),
            OperationType::BClosure(_) => f.write_str("BClosure"),
            OperationType::VFunc(func) => write!(f, "VFunc({:?}", func),
            OperationType::VClosure(_) => write!(f, "VClosure"),
        }
    }
}

impl From<UnaryFunc> for OperationType {
    fn from(u: UnaryFunc) -> OperationType {
        OperationType::Unary(u)
    }
}

impl From<BinaryFunc> for OperationType {
    fn from(b: BinaryFunc) -> OperationType {
        OperationType::BFunc(b)
    }
}

impl From<VariadicFunc> for OperationType {
    fn from(v: VariadicFunc) -> OperationType {
        OperationType::VFunc(v)
    }
}

#[derive(Debug, Clone)]
/// Tracks candidate implementations.
pub struct Candidate {
    /// The implementation under consideration.
    fimpl: FuncImpl,
    /// The argument types that will be used if we we choose `fimpl`.
    arg_types: Vec<ScalarType>,
    exact_matches: usize,
    preferred_types: usize,
}

#[derive(Clone, Debug)]
/// Determines best implementation to use given some user-provided arguments.
/// For more detail, see `ArgImplementationMatcher::select_implementation`.
pub struct ArgImplementationMatcher<'a> {
    ident: &'a str,
    ecx: &'a ExprContext<'a>,
}

impl<'a> ArgImplementationMatcher<'a> {
    /// Selects the best implementation given the provided `args` using a
    /// process similar to [PostgreSQL's parser][pgparser], and returns the
    /// `ScalarExpr` to invoke that function.
    ///
    /// # Errors
    /// - When the provided arguments are not valid for any implementation, e.g.
    ///   cannot be converted to the appropriate types.
    /// - When all implementations are equally valid.
    ///
    /// [pgparser]: https://www.postgresql.org/docs/current/typeconv-oper.html
    pub fn select_implementation(
        ident: &'a str,
        ecx: &'a ExprContext<'a>,
        impls: &[FuncImpl],
        args: &[Expr],
    ) -> Result<ScalarExpr, failure::Error> {
        // Immediately remove all `impls` we know are invalid.
        let l = args.len();
        let impls = impls
            .iter()
            .filter(|i| i.params.validate_arg_len(l))
            .cloned()
            .collect();
        let mut m = Self { ident, ecx };

        let mut exprs = Vec::new();
        for arg in args {
            let expr = super::query::plan_coercible_expr(ecx, arg)?.0;
            exprs.push(expr);
        }

        let f = m.find_match(&exprs, impls)?;

        let mut exprs = m.generate_param_exprs(exprs, f.params)?;

        Ok(match f.op {
            OperationType::ExprOnly => exprs.remove(0),
            OperationType::Unary(func) => ScalarExpr::CallUnary {
                func,
                expr: Box::new(exprs.remove(0)),
            },
            OperationType::BFunc(func) => ScalarExpr::CallBinary {
                func,
                expr1: Box::new(exprs.remove(0)),
                expr2: Box::new(exprs.remove(0)),
            },
            OperationType::BClosure(f) => f(ecx, exprs.remove(0), exprs.remove(0)),
            OperationType::VFunc(func) => ScalarExpr::CallVariadic { func, exprs },
            OperationType::VClosure(f) => f(ecx, exprs),
        })
    }

    /// Finds an exact match based on the arguments, or, if no exact match,
    /// finds the best match available. Patterned after [PostgreSQL's type
    /// conversion matching algorithm][pgparser].
    ///
    /// Inline prefixed with number are taken from the "Function Type
    /// Resolution" section of the aforelinked page.
    ///
    /// [pgparser]: https://www.postgresql.org/docs/current/typeconv-func.html
    fn find_match(
        &mut self,
        exprs: &[CoercibleScalarExpr],
        impls: Vec<FuncImpl>,
    ) -> Result<FuncImpl, failure::Error> {
        let types: Vec<_> = exprs
            .iter()
            .map(|e| self.ecx.column_type(e).map(|t| t.scalar_type))
            .collect();
        let all_types_known = types.iter().all(|t| t.is_some());

        // Check for exact match.
        if all_types_known {
            let types: Vec<_> = types.iter().map(|t| t.clone().unwrap()).collect();
            let matching_impls: Vec<&FuncImpl> = impls
                .iter()
                .filter(|i| i.params.match_scalartypes(&types))
                .collect();

            if matching_impls.len() == 1 {
                let func = Self::saturate_decimals(matching_impls[0], &types);
                return Ok(func);
            }
        }

        // No exact match. Apply PostgreSQL's best match algorithm.
        let mut max_exact_matches = 0;

        // Generate candidates by assessing their compatibility with each
        // implementation's parameters.
        let mut candidates: Vec<Candidate> = Vec::new();
        for fimpl in impls {
            let mut valid_candidate = true;
            let mut arg_types = Vec::new();
            let mut exact_matches = 0;
            let mut preferred_types = 0;

            for (i, raw_arg_type) in types.iter().enumerate() {
                let param_type = match &fimpl.params {
                    ParamList::Exact(p) => &p[i],
                    ParamList::Repeat(p) => &p[i % p.len()],
                };

                let arg_type = match raw_arg_type {
                    Some(raw_arg_type) if param_type == raw_arg_type => {
                        exact_matches += 1;
                        raw_arg_type.clone()
                    }
                    Some(raw_arg_type) => {
                        if !self.is_coercion_possible(raw_arg_type, &param_type) {
                            valid_candidate = false;
                            break;
                        }
                        if param_type.is_preferred_by(raw_arg_type) {
                            preferred_types += 1;
                        }
                        param_type.into()
                    }
                    None => {
                        let s: ScalarType = param_type.into();
                        if param_type.is_preferred_by(&s) {
                            preferred_types += 1;
                        }
                        s
                    }
                };
                arg_types.push(arg_type);
            }

            // 4.a. Discard candidate functions for which the input types do not match
            // and cannot be converted (using an implicit conversion) to match.
            // unknown literals are assumed to be convertible to anything for this
            // purpose.
            if valid_candidate {
                max_exact_matches = std::cmp::max(max_exact_matches, exact_matches);
                candidates.push(Candidate {
                    fimpl,
                    arg_types,
                    exact_matches,
                    preferred_types,
                });
            }
        }

        if candidates.is_empty() {
            bail!(
                "arguments cannot be implicitly cast to any implementation's parameters; \
                 try providing explicit casts"
            )
        }

        if let Some(func) = self.maybe_get_last_candidate(&candidates) {
            return Ok(func);
        }

        // 4.c. Run through all candidates and keep those with the most exact matches on
        // input types. Keep all candidates if none have exact matches.
        candidates.retain(|c| c.exact_matches >= max_exact_matches);

        if let Some(func) = self.maybe_get_last_candidate(&candidates) {
            return Ok(func);
        }

        // 4.d. Run through all candidates and keep those that accept preferred types
        // (of the input data type's type category) at the most positions where
        // type conversion will be required.
        let mut max_preferred_types = 0;
        for c in &candidates {
            max_preferred_types = std::cmp::max(max_preferred_types, c.preferred_types);
        }
        candidates.retain(|c| c.preferred_types >= max_preferred_types);

        if let Some(func) = self.maybe_get_last_candidate(&candidates) {
            return Ok(func);
        }

        if all_types_known {
            bail!(
                "unable to determine which implementation to use; try providing \
                 explicit casts to match parameter types"
            )
        }

        let mut found_unknown = false;
        let mut found_known = false;
        let mut types_match = true;
        let mut common_type: Option<ScalarType> = None;

        for (i, raw_arg_type) in types.iter().enumerate() {
            let mut selected_category: Option<TypeCategory> = None;
            let mut found_string_candidate = false;
            let mut categories_match = true;

            match raw_arg_type {
                // 4.e. If any input arguments are unknown, check the type categories accepted
                // at those argument positions by the remaining candidates.
                None => {
                    found_unknown = true;

                    for c in candidates.iter() {
                        let this_category = TypeCategory::from_type(&c.arg_types[i]);
                        match (&selected_category, &this_category) {
                            // 4.e. cont: At each  position, select the string category if
                            // any candidate accepts that category. (This bias
                            // towards string is appropriate since an
                            // unknown-type literal looks like a string.)
                            (Some(TypeCategory::String), _) => {}
                            (_, TypeCategory::String) => {
                                found_string_candidate = true;
                                selected_category = Some(TypeCategory::String);
                            }
                            // 4.e. cont: Otherwise, if all the remaining candidates accept
                            // the same type category, select that category.
                            (Some(selected_category), this_category) => {
                                categories_match =
                                    *selected_category == *this_category && categories_match
                            }
                            (None, this_category) => {
                                selected_category = Some(this_category.clone())
                            }
                        }
                    }

                    // 4.e. cont: Otherwise fail because the correct choice cannot be
                    // deduced without more clues.
                    if !found_string_candidate && !categories_match {
                        bail!(
                            "unable to determine which implementation to use; try providing \
                            explicit casts to match parameter types"
                        )
                    }

                    // 4.e. cont: Now discard candidates that do not accept the selected
                    // type category. Furthermore, if any candidate accepts a
                    // preferred type in that category, discard candidates that
                    // accept non-preferred types for that argument.
                    let selected_category = selected_category.unwrap();

                    let preferred_type = selected_category.preferred_type();
                    let mut found_preferred_type_candidate = false;
                    candidates.retain(|c| {
                        if let Some(typ) = &preferred_type {
                            found_preferred_type_candidate =
                                c.arg_types[i] == *typ || found_preferred_type_candidate;
                        }
                        selected_category == TypeCategory::from_type(&c.arg_types[i])
                    });

                    if found_preferred_type_candidate {
                        let preferred_type = preferred_type.unwrap();
                        candidates.retain(|c| c.arg_types[i] == preferred_type);
                    }
                }
                Some(typ) => {
                    found_known = true;
                    // Track if all known types are of the same type; use this info in 4.f.
                    if let Some(common_type) = &common_type {
                        types_match = types_match && *common_type == *typ
                    } else {
                        common_type = Some(typ.clone());
                    }
                }
            }
        }

        if let Some(func) = self.maybe_get_last_candidate(&candidates) {
            return Ok(func);
        }

        // 4.f. If there are both unknown and known-type arguments, and all the
        // known-type arguments have the same type, assume that the unknown
        // arguments are also of that type, and check which candidates can
        // accept that type at the unknown-argument positions.
        if found_known && found_unknown && types_match {
            let common_type = common_type.unwrap();
            for (i, raw_arg_type) in types.iter().enumerate() {
                if raw_arg_type.is_none() {
                    candidates.retain(|c| common_type == c.arg_types[i]);
                }
            }

            if let Some(func) = self.maybe_get_last_candidate(&candidates) {
                return Ok(func);
            }
        }

        bail!(
            "unable to determine which implementation to use; try providing \
             explicit casts to match parameter types"
        )
    }

    fn maybe_get_last_candidate(&self, candidates: &[Candidate]) -> Option<FuncImpl> {
        if candidates.len() == 1 {
            Some(Self::saturate_decimals(
                &candidates[0].fimpl,
                &candidates[0].arg_types,
            ))
        } else {
            None
        }
    }

    /// Rewrite any `Decimal` values in `FuncImpl` to use the users' arguments'
    /// scale, rather than the default value we use for matching implementations.
    fn saturate_decimals(f: &FuncImpl, types: &[ScalarType]) -> FuncImpl {
        use OperationType::*;
        use ParamType::*;
        use ScalarType::*;

        let mut f = f.clone();

        f.op = match f.op {
            Unary(UnaryFunc::CeilDecimal(_)) => match types[0] {
                ScalarType::Decimal(_, s) => Unary(UnaryFunc::CeilDecimal(s)),
                _ => unreachable!(),
            },
            Unary(UnaryFunc::FloorDecimal(_)) => match types[0] {
                ScalarType::Decimal(_, s) => Unary(UnaryFunc::FloorDecimal(s)),
                _ => unreachable!(),
            },
            Unary(UnaryFunc::RoundDecimal(_)) => match types[0] {
                ScalarType::Decimal(_, s) => Unary(UnaryFunc::RoundDecimal(s)),
                _ => unreachable!(),
            },
            BFunc(BinaryFunc::RoundDecimal(_)) => match types[0] {
                ScalarType::Decimal(_, s) => BFunc(BinaryFunc::RoundDecimal(s)),
                _ => unreachable!(),
            },
            Unary(UnaryFunc::SqrtDec(_)) => match types[0] {
                ScalarType::Decimal(_, s) => Unary(UnaryFunc::SqrtDec(s)),
                _ => unreachable!(),
            },
            other => other,
        };
        // TODO(sploiselle): Add support for saturating decimals in other
        // contexts.
        if let ParamList::Exact(ref mut param_list) = f.params {
            for (i, param) in param_list.iter_mut().enumerate() {
                if let Plain(Decimal(..)) = param {
                    *param = Plain(types[i].clone());
                }
            }
        }

        f
    }

    /// Plans `args` as `ScalarExprs` of that match the `ParamList`'s specified types.
    fn generate_param_exprs(
        &self,
        args: Vec<CoercibleScalarExpr>,
        params: ParamList,
    ) -> Result<Vec<ScalarExpr>, failure::Error> {
        match params {
            ParamList::Exact(p) => {
                let mut exprs = Vec::new();
                for (arg, param) in args.into_iter().zip(p.iter()) {
                    exprs.push(self.coerce_arg_to_type(arg, param)?);
                }
                Ok(exprs)
            }
            ParamList::Repeat(p) => {
                let mut exprs = Vec::new();
                for (i, arg) in args.into_iter().enumerate() {
                    exprs.push(self.coerce_arg_to_type(arg, &p[i % p.len()])?);
                }
                Ok(exprs)
            }
        }
    }

    /// Checks that `arg_type` is coercible to the parameter type without
    /// actually planning the coercion.
    fn is_coercion_possible(&self, arg_type: &ScalarType, to_typ: &ParamType) -> bool {
        use CastTo::*;

        let cast_to = match to_typ {
            ParamType::Plain(s) => Implicit(s.clone()),
            ParamType::JsonbAny => JsonbAny,
            ParamType::StringAny => Explicit(ScalarType::String),
        };

        super::cast::get_cast(arg_type, &cast_to).is_some()
    }

    /// Generates `ScalarExpr` necessary to coerce `Expr` into the `ScalarType`
    /// corresponding to `ParameterType`; errors if not possible. This can only
    /// work within the `func` module because it relies on `ParameterType`.
    fn coerce_arg_to_type(
        &self,
        arg: CoercibleScalarExpr,
        typ: &ParamType,
    ) -> Result<ScalarExpr, failure::Error> {
        use CastTo::*;
        let coerce_to = match typ {
            ParamType::Plain(s) => CoerceTo::Plain(s.clone()),
            ParamType::JsonbAny => CoerceTo::JsonbAny,
            ParamType::StringAny => CoerceTo::Plain(ScalarType::String),
        };

        let arg = super::query::plan_coerce(self.ecx, arg, coerce_to)?;
        let to_typ = match typ {
            ParamType::Plain(s) => Implicit(s.clone()),
            ParamType::JsonbAny => JsonbAny,
            ParamType::StringAny => Explicit(ScalarType::String),
        };
        super::query::plan_cast_internal(self.ident, self.ecx, arg, to_typ)
    }
}

/// Provides shorthand for converting `Vec<ScalarType>` into `Vec<ParamType>`.
macro_rules! params(
    ( $($p:expr),* ) => {
        vec![$($p,)+].into_params()
    };
);

/// Provides shorthand for inserting [`FuncImpl`]s into arbitrary `HashMap`s.
macro_rules! insert_impl(
    ($hashmap:ident, $key:expr, $($params:expr => $op:expr),+) => {
        let impls = vec![
            $(FuncImpl {
                params: $params.into(),
                op: $op.clone().into(),
            },)+
        ];

        $hashmap.entry($key).or_default().extend(impls);
    };
);

/// Provides a macro to write HashMap "literals" for matching some key to
/// `Vec<FuncImpl>`.
macro_rules! impls(
    {
        $(
            $name:expr => {
                $($params:expr => $op:expr),+
            }
        ),+
    } => {{
        let mut m: HashMap<_, Vec<FuncImpl>> = HashMap::new();
        $(
            insert_impl!{m, $name, $($params => $op),+}
        )+
        m
    }};
);

lazy_static! {
    /// Correlates a built-in function name to its implementations.
    static ref BUILTIN_IMPLS: HashMap<&'static str, Vec<FuncImpl>> = {
        use OperationType::*;
        use ParamList::*;
        use ParamType::*;
        use ScalarType::*;
        impls! {
            "abs" => {
                params!(Int32) => UnaryFunc::AbsInt32,
                params!(Int64) => UnaryFunc::AbsInt64,
                params!(Decimal(0, 0)) => UnaryFunc::AbsDecimal,
                params!(Float32) => UnaryFunc::AbsFloat32,
                params!(Float64) => UnaryFunc::AbsFloat64
            },
            "ascii" => {
                params!(String) => UnaryFunc::Ascii
            },
            "btrim" => {
                params!(String) => UnaryFunc::TrimWhitespace,
                params!(String, String) => BinaryFunc::Trim
            },
            "bit_length" => {
                params!(Bytes) => Unary(UnaryFunc::BitLengthBytes),
                params!(String) => Unary(UnaryFunc::BitLengthString)
            },
            "ceil" => {
                params!(Float32) => UnaryFunc::CeilFloat32,
                params!(Float64) => UnaryFunc::CeilFloat64,
                params!(Decimal(0, 0)) => UnaryFunc::CeilDecimal(0)
            },
            "char_length" => {
                params!(String) => Unary(UnaryFunc::CharLength)
            },
            "concat" => {
                Repeat(vec![StringAny]) => VClosure(|_ecx, mut exprs| {
                    // Unlike all other `StringAny` casts, `concat` uses an
                    // implicit behavior for converting bools to strings.
                    for e in &mut exprs {
                        if let ScalarExpr::CallUnary {
                            func: func @ UnaryFunc::CastBoolToStringExplicit,
                            ..
                        } = e {
                            *func = UnaryFunc::CastBoolToStringImplicit;
                        }
                    }
                    ScalarExpr::CallVariadic { func: VariadicFunc::Concat, exprs }
                })
            },
            "convert_from" => {
                params!(Bytes, String) => BinaryFunc::ConvertFrom
            },
            "date_trunc" => {
                params!(String, Timestamp) => BinaryFunc::DateTruncTimestamp,
                params!(String, TimestampTz) => BinaryFunc::DateTruncTimestampTz
            },
            "floor" => {
                params!(Float32) => UnaryFunc::FloorFloat32,
                params!(Float64) => UnaryFunc::FloorFloat64,
                params!(Decimal(0, 0)) => UnaryFunc::FloorDecimal(0)
            },
            "jsonb_array_length" => {
                params!(Jsonb) => UnaryFunc::JsonbArrayLength
            },
            "jsonb_build_array" => {
                Exact(vec![]) => VariadicFunc::JsonbBuildArray,
                Repeat(vec![JsonbAny]) => VariadicFunc::JsonbBuildArray
            },
            "jsonb_build_object" => {
                Exact(vec![]) => VariadicFunc::JsonbBuildObject,
                Repeat(vec![StringAny, JsonbAny]) =>
                    VariadicFunc::JsonbBuildObject
            },
            "jsonb_pretty" => {
                params!(Jsonb) => UnaryFunc::JsonbPretty
            },
            "jsonb_strip_nulls" => {
                params!(Jsonb) => UnaryFunc::JsonbStripNulls
            },
            "jsonb_typeof" => {
                params!(Jsonb) => UnaryFunc::JsonbTypeof
            },
            "length" => {
                params!(Bytes) => Unary(UnaryFunc::ByteLengthBytes),
                params!(String) => Unary(UnaryFunc::CharLength),
                params!(Bytes, String) => BinaryFunc::EncodedBytesCharLength
            },
            "octet_length" => {
                params!(Bytes) => Unary(UnaryFunc::ByteLengthBytes),
                params!(String) => Unary(UnaryFunc::ByteLengthString)
            },
            "ltrim" => {
                params!(String) => UnaryFunc::TrimLeadingWhitespace,
                params!(String, String) => BinaryFunc::TrimLeading
            },
            "replace" => {
                params!(String, String, String) => VariadicFunc::Replace
            },
            "round" => {
                params!(Float32) => UnaryFunc::RoundFloat32,
                params!(Float64) => UnaryFunc::RoundFloat64,
                params!(Decimal(0,0)) => UnaryFunc::RoundDecimal(0),
                params!(Decimal(0,0), Int64) => BinaryFunc::RoundDecimal(0)
            },
            "rtrim" => {
                params!(String) => UnaryFunc::TrimTrailingWhitespace,
                params!(String, String) => BinaryFunc::TrimTrailing
            },
            "substr" => {
                params!(String, Int64) => VariadicFunc::Substr,
                params!(String, Int64, Int64) => VariadicFunc::Substr
            },
            "substring" => {
                params!(String, Int64) => VariadicFunc::Substr,
                params!(String, Int64, Int64) => VariadicFunc::Substr
            },
            "sqrt" => {
                params!(Float32) => UnaryFunc::SqrtFloat32,
                params!(Float64) => UnaryFunc::SqrtFloat64,
                params!(Decimal(0,0)) => UnaryFunc::SqrtDec(0)
            },
            "to_char" => {
                params!(Timestamp, String) => BinaryFunc::ToCharTimestamp,
                params!(TimestampTz, String) => BinaryFunc::ToCharTimestampTz
            },
            // > Returns the value as json or jsonb. Arrays and composites
            // > are converted (recursively) to arrays and objects;
            // > otherwise, if there is a cast from the type to json, the
            // > cast function will be used to perform the conversion;
            // > otherwise, a scalar value is produced. For any scalar type
            // > other than a number, a Boolean, or a null value, the text
            // > representation will be used, in such a fashion that it is a
            // > valid json or jsonb value.
            //
            // https://www.postgresql.org/docs/current/functions-json.html
            "to_jsonb" => {
                vec![JsonbAny] => ExprOnly
            },
            "to_timestamp" => {
                params!(Float64) => UnaryFunc::ToTimestamp
            }
        }
    };
}

/// Gets a built-in scalar function and the `ScalarExpr`s required to invoke it.
pub fn select_scalar_func(
    ecx: &ExprContext,
    ident: &str,
    args: &[Expr],
) -> Result<ScalarExpr, failure::Error> {
    let impls = match BUILTIN_IMPLS.get(ident) {
        Some(i) => i,
        None => unsupported!(ident),
    };

    match ArgImplementationMatcher::select_implementation(ident, ecx, impls, args) {
        Ok(expr) => Ok(expr),
        Err(e) => bail!("Cannot call function '{}': {}", ident, e),
    }
}

lazy_static! {
    /// Correlates a `BinaryOperator` with all of its implementations.
    static ref BINARY_OP_IMPLS: HashMap<BinaryOperator, Vec<FuncImpl>> = {
        use ScalarType::*;
        use BinaryOperator::*;
        use super::expr::BinaryFunc::*;
        use OperationType::*;
        use ParamType::*;
        let mut m = impls! {
            // ARITHMETIC
            Plus => {
                params!(Int32, Int32) => AddInt32,
                params!(Int64, Int64) => AddInt64,
                params!(Float32, Float32) => AddFloat32,
                params!(Float64, Float64) => AddFloat64,
                params!(Decimal(0, 0), Decimal(0, 0)) => {
                    BClosure(|ecx, lhs, rhs| {
                        let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                        lexpr.call_binary(rexpr, AddDecimal)
                    })
                },
                params!(Interval, Interval) => AddInterval,
                params!(Timestamp, Interval) => AddTimestampInterval,
                params!(Interval, Timestamp) => {
                    BClosure(|_ecx, lhs, rhs| rhs.call_binary(lhs, AddTimestampInterval))
                },
                params!(TimestampTz, Interval) => AddTimestampTzInterval,
                params!(Interval, TimestampTz) => {
                    BClosure(|_ecx, lhs, rhs| rhs.call_binary(lhs, AddTimestampTzInterval))
                },
                params!(Date, Interval) => AddDateInterval,
                params!(Interval, Date) => {
                    BClosure(|_ecx, lhs, rhs| rhs.call_binary(lhs, AddDateInterval))
                },
                params!(Date, Time) => AddDateTime,
                params!(Time, Date) => {
                    BClosure(|_ecx, lhs, rhs| rhs.call_binary(lhs, AddDateTime))
                },
                params!(Time, Interval) => AddTimeInterval,
                params!(Interval, Time) => {
                    BClosure(|_ecx, lhs, rhs| rhs.call_binary(lhs, AddTimeInterval))
                }
            },
            Minus => {
                params!(Int32, Int32) => SubInt32,
                params!(Int64, Int64) => SubInt64,
                params!(Float32, Float32) => SubFloat32,
                params!(Float64, Float64) => SubFloat64,
                params!(Decimal(0, 0), Decimal(0, 0)) => BClosure(|ecx, lhs, rhs| {
                    let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                    lexpr.call_binary(rexpr, SubDecimal)
                }),
                params!(Interval, Interval) => SubInterval,
                params!(Timestamp, Timestamp) => SubTimestamp,
                params!(TimestampTz, TimestampTz) => SubTimestampTz,
                params!(Timestamp, Interval) => SubTimestampInterval,
                params!(TimestampTz, Interval) => SubTimestampTzInterval,
                params!(Date, Date) => SubDate,
                params!(Date, Interval) => SubDateInterval,
                params!(Time, Time) => SubTime,
                params!(Time, Interval) => SubTimeInterval,
                params!(Jsonb, Int64) => JsonbDeleteInt64,
                params!(Jsonb, String) => JsonbDeleteString
                // TODO(jamii) there should be corresponding overloads for
                // Array(Int64) and Array(String)
            },
            Multiply => {
                params!(Int32, Int32) => MulInt32,
                params!(Int64, Int64) => MulInt64,
                params!(Float32, Float32) => MulFloat32,
                params!(Float64, Float64) => MulFloat64,
                params!(Decimal(0, 0), Decimal(0, 0)) => BClosure(|ecx, lhs, rhs| {
                    use std::cmp::*;
                    match (ecx.scalar_type(&lhs), ecx.scalar_type(&rhs)) {
                        (Decimal(_, s1), Decimal(_,s2)) => {
                            let so = max(max(min(s1 + s2, 12), s1), s2);
                            let si = s1 + s2;
                            let expr = lhs.call_binary(rhs, MulDecimal);
                            rescale_decimal(expr, si, so)
                        },
                        (_, _) => unreachable!()
                    }
                })
            },
            Divide => {
                params!(Int32, Int32) => DivInt32,
                params!(Int64, Int64) => DivInt64,
                params!(Float32, Float32) => DivFloat32,
                params!(Float64, Float64) => DivFloat64,
                params!(Decimal(0, 0), Decimal(0, 0)) => BClosure(|ecx, lhs, rhs| {
                    use std::cmp::*;
                    match (ecx.scalar_type(&lhs), ecx.scalar_type(&rhs)) {
                        (Decimal(_, s1), Decimal(_,s2)) => {
                            // Pretend all 0-scale numerators were of the same scale as
                            // their denominators for improved accuracy.
                            let s1_mod = if s1 == 0 { s2 } else { s1 };
                            let s = max(min(12, s1_mod + 6), s1_mod);
                            let si = max(s + 1, s2);
                            let lhs = rescale_decimal(lhs, s1, si);
                            let expr = lhs.call_binary(rhs, DivDecimal);
                            rescale_decimal(expr, si - s2, s)
                        },
                        (_, _) => unreachable!()
                    }
                })
            },
            Modulus => {
                params!(Int32, Int32) => ModInt32,
                params!(Int64, Int64) => ModInt64,
                params!(Float32, Float32) => ModFloat32,
                params!(Float64, Float64) => ModFloat64,
                params!(Decimal(0, 0), Decimal(0, 0)) => BClosure(|ecx, lhs, rhs| {
                    let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                    lexpr.call_binary(rexpr, ModDecimal)
                })
            },

            // BOOLEAN OPS
            BinaryOperator::And => {
                params!(Bool, Bool) => BinaryFunc::And
            },
            BinaryOperator::Or => {
                params!(Bool, Bool) => BinaryFunc::Or
            },

            // LIKE
            Like => {
                params!(String, String) => MatchLikePattern
            },
            NotLike => {
                params!(String, String) => BClosure(|_ecx, lhs, rhs| {
                    lhs
                        .call_binary(rhs, MatchLikePattern)
                        .call_unary(UnaryFunc::Not)
                })
            },

            // CONCAT
            Concat => {
                vec![Plain(String), StringAny] => TextConcat,
                vec![StringAny, Plain(String)] => TextConcat,
                params!(String, String) => TextConcat,
                params!(Jsonb, Jsonb) => JsonbConcat
            },

            //JSON
            JsonGet => {
                params!(Jsonb, Int64) => JsonbGetInt64,
                params!(Jsonb, String) => JsonbGetString
            },
            JsonGetAsText => {
                params!(Jsonb, Int64) => BClosure(|_ecx, lhs, rhs| {
                    lhs.call_binary(rhs, BinaryFunc::JsonbGetInt64)
                        .call_unary(UnaryFunc::JsonbStringifyUnlessString)
                }),
                params!(Jsonb, String) => BClosure(|_ecx, lhs, rhs| {
                    lhs.call_binary(rhs, BinaryFunc::JsonbGetString)
                        .call_unary(UnaryFunc::JsonbStringifyUnlessString)
                })
            },
            JsonContainsJson => {
                params!(Jsonb, Jsonb) => JsonbContainsJsonb,
                params!(Jsonb, String) => BClosure(|_ecx, lhs, rhs| {
                    lhs.call_binary(
                        rhs.call_unary(UnaryFunc::CastStringToJsonb),
                        JsonbContainsJsonb,
                    )
                }),
                params!(String, Jsonb) => BClosure(|_ecx, lhs, rhs| {
                    lhs.call_unary(UnaryFunc::CastStringToJsonb)
                        .call_binary(rhs, JsonbContainsJsonb)
                })
            },
            JsonContainedInJson => {
                params!(Jsonb, Jsonb) =>  BClosure(|_ecx, lhs, rhs| {
                    rhs.call_binary(
                        lhs,
                        JsonbContainsJsonb
                    )
                }),
                params!(Jsonb, String) => BClosure(|_ecx, lhs, rhs| {
                    rhs.call_unary(UnaryFunc::CastStringToJsonb)
                        .call_binary(lhs, BinaryFunc::JsonbContainsJsonb)
                }),
                params!(String, Jsonb) => BClosure(|_ecx, lhs, rhs| {
                    rhs.call_binary(
                        lhs.call_unary(UnaryFunc::CastStringToJsonb),
                        BinaryFunc::JsonbContainsJsonb,
                    )
                })
            },
            JsonContainsField => {
                params!(Jsonb, String) => JsonbContainsString
            },
            // COMPARISON OPS
            // n.b. Decimal impls are separated from other types because they
            // require a function pointer, which you cannot dynamically generate.
            BinaryOperator::Lt => {
                params!(Decimal(0, 0), Decimal(0, 0)) => {
                    BClosure(|ecx, lhs, rhs| {
                        let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                        lexpr.call_binary(rexpr, BinaryFunc::Lt)
                    })
                }
            },
            BinaryOperator::LtEq => {
                params!(Decimal(0, 0), Decimal(0, 0)) => {
                    BClosure(|ecx, lhs, rhs| {
                        let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                        lexpr.call_binary(rexpr, BinaryFunc::Lte)
                    })
                }
            },
            BinaryOperator::Gt => {
                params!(Decimal(0, 0), Decimal(0, 0)) => {
                    BClosure(|ecx, lhs, rhs| {
                        let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                        lexpr.call_binary(rexpr, BinaryFunc::Gt)
                    })
                }
            },
            BinaryOperator::GtEq => {
                params!(Decimal(0, 0), Decimal(0, 0)) => {
                    BClosure(|ecx, lhs, rhs| {
                        let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                        lexpr.call_binary(rexpr, BinaryFunc::Gte)
                    })
                }
            },
            BinaryOperator::Eq => {
                params!(Decimal(0, 0), Decimal(0, 0)) => {
                    BClosure(|ecx, lhs, rhs| {
                        let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                        lexpr.call_binary(rexpr, BinaryFunc::Eq)
                    })
                }
            },
            BinaryOperator::NotEq => {
                params!(Decimal(0, 0), Decimal(0, 0)) => {
                    BClosure(|ecx, lhs, rhs| {
                        let (lexpr, rexpr) = rescale_decimals_to_same(ecx, lhs, rhs);
                        lexpr.call_binary(rexpr, BinaryFunc::NotEq)
                    })
                }
            }
        };

        for (op, func) in vec![
            (BinaryOperator::Lt, BinaryFunc::Lt),
            (BinaryOperator::LtEq, BinaryFunc::Lte),
            (BinaryOperator::Gt, BinaryFunc::Gt),
            (BinaryOperator::GtEq, BinaryFunc::Gte),
            (BinaryOperator::Eq, BinaryFunc::Eq),
            (BinaryOperator::NotEq, BinaryFunc::NotEq)
        ] {
            insert_impl!(m, op,
                params!(Bool, Bool) => func,
                params!(Int32, Int32) => func,
                params!(Int64, Int64) => func,
                params!(Float32, Float32) => func,
                params!(Float64, Float64) => func,
                params!(Date, Date) => func,
                params!(Time, Time) => func,
                params!(Timestamp, Timestamp) => func,
                params!(TimestampTz, TimestampTz) => func,
                params!(Interval, Interval) => func,
                params!(Bytes, Bytes) => func,
                params!(String, String) => func,
                params!(Jsonb, Jsonb) => func
            );
        }

        m
    };
}

/// Rescales two decimals to have the same scale.
fn rescale_decimals_to_same(
    ecx: &ExprContext,
    lhs: ScalarExpr,
    rhs: ScalarExpr,
) -> (ScalarExpr, ScalarExpr) {
    match (ecx.scalar_type(&lhs), ecx.scalar_type(&rhs)) {
        (ScalarType::Decimal(_, s1), ScalarType::Decimal(_, s2)) => {
            let so = std::cmp::max(s1, s2);
            let lexpr = rescale_decimal(lhs, s1, so);
            let rexpr = rescale_decimal(rhs, s2, so);
            (lexpr, rexpr)
        }
        (_, _) => unreachable!(),
    }
}

/// Plans a function compatible with the `BinaryOperator`.
pub fn plan_binary_op<'a>(
    ecx: &ExprContext,
    op: &'a BinaryOperator,
    left: &'a Expr,
    right: &'a Expr,
) -> Result<ScalarExpr, failure::Error> {
    let impls = match BINARY_OP_IMPLS.get(&op) {
        Some(i) => i,
        // TODO: these require sql arrays
        // JsonContainsAnyFields
        // JsonContainsAllFields
        // TODO: these require json paths
        // JsonGetPath
        // JsonGetPathAsText
        // JsonDeletePath
        // JsonContainsPath
        // JsonApplyPathPredicate
        None => unsupported!(op),
    };

    let args = vec![left.clone(), right.clone()];

    match ArgImplementationMatcher::select_implementation(&op.to_string(), ecx, impls, &args) {
        Ok(expr) => Ok(expr),
        Err(e) => {
            let lexpr = super::query::plan_expr(ecx, left, None)?;
            let rexpr = super::query::plan_expr(ecx, right, None)?;
            bail!(
                "no overload for {} {} {}: {}",
                ecx.scalar_type(&lexpr),
                op,
                ecx.scalar_type(&rexpr),
                e
            )
        }
    }
}
