// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

use mz_adapter::catalog::Catalog;
use mz_ore::collections::CollectionExt;
use mz_ore::now::NOW_ZERO;
use mz_repr::ScalarType;
use mz_sql::plan::PlanContext;

#[mz_ore::test(tokio::test)]
async fn test_parameter_type_inference() {
    let test_cases = vec![
        (
            "SELECT $1, $2, $3",
            vec![ScalarType::String, ScalarType::String, ScalarType::String],
        ),
        (
            "VALUES($1, $2, $3)",
            vec![ScalarType::String, ScalarType::String, ScalarType::String],
        ),
        (
            "SELECT 1 GROUP BY $1, $2, $3",
            vec![ScalarType::String, ScalarType::String, ScalarType::String],
        ),
        (
            "SELECT 1 ORDER BY $1, $2, $3",
            vec![ScalarType::String, ScalarType::String, ScalarType::String],
        ),
        (
            "SELECT ($1), (((($2))))",
            vec![ScalarType::String, ScalarType::String],
        ),
        ("SELECT $1::pg_catalog.int4", vec![ScalarType::Int32]),
        ("SELECT 1 WHERE $1", vec![ScalarType::Bool]),
        ("SELECT 1 HAVING $1", vec![ScalarType::Bool]),
        (
            "SELECT 1 FROM (VALUES (1)) a JOIN (VALUES (1)) b ON $1",
            vec![ScalarType::Bool],
        ),
        (
            "SELECT CASE WHEN $1 THEN 1 ELSE 0 END",
            vec![ScalarType::Bool],
        ),
        (
            "SELECT CASE WHEN true THEN $1 ELSE $2 END",
            vec![ScalarType::String, ScalarType::String],
        ),
        (
            "SELECT CASE WHEN true THEN $1 ELSE 1 END",
            vec![ScalarType::Int32],
        ),
        ("SELECT pg_catalog.abs($1)", vec![ScalarType::Float64]),
        ("SELECT pg_catalog.ascii($1)", vec![ScalarType::String]),
        (
            "SELECT coalesce($1, $2, $3)",
            vec![ScalarType::String, ScalarType::String, ScalarType::String],
        ),
        ("SELECT coalesce($1, 1)", vec![ScalarType::Int32]),
        (
            "SELECT pg_catalog.substr($1, $2)",
            vec![ScalarType::String, ScalarType::Int64],
        ),
        (
            "SELECT pg_catalog.substring($1, $2)",
            vec![ScalarType::String, ScalarType::Int64],
        ),
        (
            "SELECT $1 LIKE $2",
            vec![ScalarType::String, ScalarType::String],
        ),
        ("SELECT NOT $1", vec![ScalarType::Bool]),
        ("SELECT $1 AND $2", vec![ScalarType::Bool, ScalarType::Bool]),
        ("SELECT $1 OR $2", vec![ScalarType::Bool, ScalarType::Bool]),
        ("SELECT +$1", vec![ScalarType::Float64]),
        ("SELECT $1 < 1", vec![ScalarType::Int32]),
        (
            "SELECT $1 < $2",
            vec![ScalarType::String, ScalarType::String],
        ),
        ("SELECT $1 + 1", vec![ScalarType::Int32]),
        (
            "SELECT $1 + 1.0",
            vec![ScalarType::Numeric { max_scale: None }],
        ),
        (
            "SELECT '1970-01-01 00:00:00'::pg_catalog.timestamp + $1",
            vec![ScalarType::Interval],
        ),
        (
            "SELECT $1 + '1970-01-01 00:00:00'::pg_catalog.timestamp",
            vec![ScalarType::Interval],
        ),
        (
            "SELECT $1::pg_catalog.int4, $1 + $2",
            vec![ScalarType::Int32, ScalarType::Int32],
        ),
        (
            "SELECT '[0, 1, 2]'::pg_catalog.jsonb - $1",
            vec![ScalarType::String],
        ),
    ];

    Catalog::with_debug(NOW_ZERO.clone(), |catalog| async move {
        let catalog = catalog.for_system_session();
        for (sql, types) in test_cases {
            let stmt = mz_sql::parse::parse(sql).unwrap().into_element();
            let (stmt, _) = mz_sql::names::resolve(&catalog, stmt).unwrap();
            let desc = mz_sql::plan::describe(&PlanContext::zero(), &catalog, stmt, &[]).unwrap();
            assert_eq!(desc.param_types, types);
        }
    })
    .await
}
