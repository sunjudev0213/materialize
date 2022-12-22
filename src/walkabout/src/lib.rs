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
// Have complaints about the noise? See the note in misc/python/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::needless_collect)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![deny(warnings)]
#![deny(clippy::bool_comparison)]
#![deny(clippy::clone_on_ref_ptr)]
#![deny(clippy::no_effect)]
#![deny(clippy::unnecessary_unwrap)]
#![deny(clippy::dbg_macro)]
#![deny(clippy::todo)]
#![deny(clippy::wildcard_dependencies)]
#![deny(clippy::zero_prefixed_literal)]
#![deny(clippy::borrowed_box)]
#![deny(clippy::deref_addrof)]
#![deny(clippy::double_must_use)]
#![deny(clippy::double_parens)]
#![deny(clippy::extra_unused_lifetimes)]
#![deny(clippy::needless_borrow)]
#![deny(clippy::needless_question_mark)]
#![deny(clippy::needless_return)]
#![deny(clippy::redundant_pattern)]
#![deny(clippy::redundant_slicing)]
#![deny(clippy::redundant_static_lifetimes)]
#![deny(clippy::single_component_path_imports)]
#![deny(clippy::unnecessary_cast)]
#![deny(clippy::useless_asref)]
#![deny(clippy::useless_conversion)]
#![deny(clippy::builtin_type_shadow)]
#![deny(clippy::duplicate_underscore_argument)]
#![deny(clippy::double_neg)]
#![deny(clippy::unnecessary_mut_passed)]
#![deny(clippy::wildcard_in_or_patterns)]
#![deny(clippy::collapsible_if)]
#![deny(clippy::collapsible_else_if)]
#![deny(clippy::crosspointer_transmute)]
#![deny(clippy::excessive_precision)]
#![deny(clippy::overflow_check_conditional)]
#![deny(clippy::as_conversions)]
#![deny(clippy::match_overlapping_arm)]
#![deny(clippy::zero_divided_by_zero)]
#![deny(clippy::must_use_unit)]
#![deny(clippy::suspicious_assignment_formatting)]
#![deny(clippy::suspicious_else_formatting)]
#![deny(clippy::suspicious_unary_op_formatting)]
#![deny(clippy::mut_mutex_lock)]
#![deny(clippy::print_literal)]
#![deny(clippy::same_item_push)]
#![deny(clippy::useless_format)]
#![deny(clippy::write_literal)]
#![deny(clippy::redundant_closure)]
#![deny(clippy::redundant_closure_call)]
#![deny(clippy::unnecessary_lazy_evaluations)]
#![deny(clippy::partialeq_ne_impl)]
#![deny(clippy::redundant_field_names)]
#![deny(clippy::transmutes_expressible_as_ptr_casts)]
#![deny(clippy::unused_async)]
#![deny(clippy::disallowed_methods)]
#![deny(clippy::disallowed_macros)]
#![deny(clippy::from_over_into)]
// END LINT CONFIG

//! Visitor generation for Rust structs and enums.
//!
//! Usage documentation is a work in progress, but for an example of the
//! generated visitor, see the [`sqlparser::ast::visit`] module.
//!
//! Some of our ASTs, which we represent with a tree of Rust structs and enums,
//! are sufficiently complicated that maintaining a visitor by hand is onerous.
//! This crate provides a generalizable framework for parsing Rust struct and
//! enum definitions from source code and automatically generating tree
//! traversal ("visitor") code.
//!
//! Note that the desired structure of the `Visit` and `VisitMut` traits
//! precludes the use of a custom derive procedural macro. We need to consider
//! the entire AST at once to build the `Visit` and `VisitMut` traits, and
//! derive macros only allow you to see one struct at a time.
//!
//! The design of the visitors is modeled after the visitors provided by the
//! [`syn`] crate. See: <https://github.com/dtolnay/syn/tree/master/codegen>
//!
//! The name of this crate is an homage to CockroachDB's
//! [Go package of the same name][crdb-walkabout].
//!
//! [`sqlparser::ast::visit`]: ../sql_parser/ast/visit/
//! [crdb-walkabout]: https://github.com/cockroachdb/walkabout

use std::path::Path;

use anyhow::Result;

mod gen;
mod parse;

pub mod ir;

pub use gen::gen_fold;
pub use gen::gen_visit;
pub use gen::gen_visit_mut;

/// Loads type definitions from the specified module.
///
/// Returns an intermediate representation (IR) that can be fed to the
/// generation functions, like [`gen_visit`].
///
/// Note that parsing a Rust module is complicated. While most of the heavy
/// lifting is performed by [`syn`], syn does not understand the various options
/// for laying out a crate—and there are many attributes and edition settings
/// that control how modules can be laid out on the file system. This function
/// does not attempt to be fully general and only handles the file layout
/// currently required by Materialize.
///
/// Analyzing Rust types is also complicated. This function only handles basic
/// Rust containers, like [`Option`] and [`Vec`]. It does, however, endeavor to
/// produce understandable error messages when it encounters a type it does not
/// know how to handle.
pub fn load<P>(path: P) -> Result<ir::Ir>
where
    P: AsRef<Path>,
{
    let items = parse::parse_mod(path)?;
    ir::analyze(&items)
}
