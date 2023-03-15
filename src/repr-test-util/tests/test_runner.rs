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
#![warn(clippy::collapsible_if)]
#![warn(clippy::collapsible_else_if)]
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

#[cfg(test)]
mod tests {
    use mz_lowertest::{deserialize_optional_generic, tokenize};
    use mz_ore::str::separated;
    use mz_repr::ScalarType;
    use mz_repr_test_util::*;

    fn build_datum(s: &str) -> Result<String, String> {
        // 1) Convert test spec to the row containing the datum.
        let mut stream_iter = tokenize(s)?.into_iter();
        let litval =
            extract_literal_string(&stream_iter.next().ok_or("Empty test")?, &mut stream_iter)?
                .unwrap();
        let scalar_type = get_scalar_type_or_default(&litval[..], &mut stream_iter)?;
        let row = test_spec_to_row(std::iter::once((&litval[..], &scalar_type)))?;
        // 2) It should be possible to unpack the row and then convert the datum
        // back to the test spec.
        let datum = row.unpack_first();
        let roundtrip_s = datum_to_test_spec(datum);
        if roundtrip_s != litval {
            Err(format!(
                "Round trip failed. Old spec: {}. New spec: {}.",
                litval, roundtrip_s
            ))
        } else {
            Ok(format!("{:?}", datum))
        }
    }

    fn build_row(s: &str) -> Result<String, String> {
        let mut stream_iter = tokenize(s)?.into_iter();
        let litvals = parse_vec_of_literals(
            &stream_iter
                .next()
                .ok_or_else(|| "Empty row spec".to_string())?,
        )?;
        let scalar_types: Option<Vec<ScalarType>> =
            deserialize_optional_generic(&mut stream_iter, "Vec<ScalarType>")?;
        let scalar_types = if let Some(scalar_types) = scalar_types {
            scalar_types
        } else {
            litvals
                .iter()
                .map(|l| get_scalar_type_or_default(l, &mut std::iter::empty()))
                .collect::<Result<Vec<_>, String>>()?
        };
        let row = test_spec_to_row(litvals.iter().map(|s| &s[..]).zip(scalar_types.iter()))?;
        let roundtrip_litvals = row
            .unpack()
            .into_iter()
            .map(datum_to_test_spec)
            .collect::<Vec<_>>();
        if roundtrip_litvals != litvals {
            Err(format!(
                "Round trip failed. Old spec: [{}]. New spec: [{}].",
                separated(" ", litvals),
                separated(" ", roundtrip_litvals)
            ))
        } else {
            Ok(format!(
                "{}",
                separated("\n", row.unpack().into_iter().map(|d| format!("{:?}", d)))
            ))
        }
    }

    fn build_scalar_type(s: &str) -> Result<ScalarType, String> {
        get_scalar_type_or_default("", &mut tokenize(s)?.into_iter())
    }

    #[test]
    #[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `decContextDefault` on OS `linux`
    fn run() {
        datadriven::walk("tests/testdata", |f| {
            f.run(move |s| -> String {
                match s.directive.as_str() {
                    "build-scalar-type" => match build_scalar_type(&s.input) {
                        Ok(scalar_type) => format!("{:?}\n", scalar_type),
                        Err(err) => format!("error: {}\n", err),
                    },
                    "build-datum" => match build_datum(&s.input) {
                        Ok(result) => format!("{}\n", result),
                        Err(err) => format!("error: {}\n", err),
                    },
                    "build-row" => match build_row(&s.input) {
                        Ok(result) => format!("{}\n", result),
                        Err(err) => format!("error: {}\n", err),
                    },
                    _ => panic!("unknown directive: {}", s.directive),
                }
            })
        });
    }
}
