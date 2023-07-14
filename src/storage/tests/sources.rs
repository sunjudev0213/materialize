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

//! Basic unit tests for sources.

use std::collections::BTreeMap;

use mz_storage::source::testscript::ScriptCommand;
use mz_storage_client::types::sources::encoding::SourceDataEncoding;
use mz_storage_client::types::sources::SourceEnvelope;

mod setup;

#[mz_ore::test]
#[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `rocksdb_create_default_env` on OS `linux`
fn test_datadriven() {
    datadriven::walk("tests/datadriven", |f| {
        let mut sources: BTreeMap<
            String,
            (Vec<ScriptCommand>, SourceDataEncoding, SourceEnvelope),
        > = BTreeMap::new();

        // Note we unwrap and panic liberally here as we
        // expect tests to be properly written.
        f.run(move |tc| -> String {
            match tc.directive.as_str() {
                "register-source" => {
                    // we just use the serde json representations.
                    let source: serde_json::Value = serde_json::from_str(&tc.input).unwrap();
                    let source = source.as_object().unwrap();
                    sources.insert(
                        tc.args["name"][0].clone(),
                        (
                            serde_json::from_value(source["script"].clone()).unwrap(),
                            serde_json::from_value(source["encoding"].clone()).unwrap(),
                            serde_json::from_value(source["envelope"].clone()).unwrap(),
                        ),
                    );

                    "<empty>\n".to_string()
                }
                "run-source" => {
                    let (script, encoding, envelope) = sources[&tc.args["name"][0]].clone();

                    // We just use the `Debug` representation here.
                    // REWRITE=true makes this reasonable!
                    format!(
                        "{:#?}\n",
                        setup::run_script_source(
                            script,
                            encoding,
                            envelope,
                            tc.args["expected_len"][0].parse().unwrap(),
                        )
                        .unwrap()
                    )
                }
                _ => panic!("unknown directive {:?}", tc),
            }
        })
    });
}
