// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Provides a publicly available interface to transform our SQL ASTs.

use std::collections::{HashMap, HashSet};

use super::visit::{self, Visit};
use super::visit_mut::{self, VisitMut};
use super::{Expr, Ident, ObjectName, Query, Statement};
use crate::names::FullName;

/// Changes the `name` used in an item's `CREATE` statement. To complete a
/// rename operation, you must also call `create_stmt_rename_refs` on all dependent
/// items.
pub fn create_stmt_rename(create_stmt: &mut Statement, to_name: String) {
    // TODO(sploiselle): Support renaming schemas and databases.
    match create_stmt {
        Statement::CreateIndex { name, .. } => {
            *name = Some(Ident::new(to_name));
        }
        Statement::CreateSink { name, .. }
        | Statement::CreateSource { name, .. }
        | Statement::CreateView { name, .. } => {
            name.0[2] = Ident::new(to_name);
        }
        _ => unreachable!("Internal error: only catalog items can be renamed"),
    }
}

/// Updates all references of `from_name` in `create_stmt` to `to_name` or
/// errors if request is ambiguous.
///
/// Requests are considered ambiguous if `create_stmt` is a
/// `Statement::CreateView`, and any of the following apply to its `query`:
/// - `to_name.item` is used as an [`Ident`] in `query`.
/// - `from_name.item` does not unambiguously refer to an item in the query,
///   e.g. it is also used as a schema, or not all references to the item are
///   sufficiently qualified.
/// - `to_name.item` does not unambiguously refer to an item in the query after
///   the rename. Right now, given the first condition, this is just a coherence
///   check, but will be more meaningful once the first restriction is lifted.
pub fn create_stmt_rename_refs(
    create_stmt: &mut Statement,
    from_name: FullName,
    to_name: FullName,
) -> Result<(), String> {
    let from_object = ObjectName::from(from_name.clone());
    let maybe_update_object_name = |object_name: &mut ObjectName| {
        if object_name.0 == from_object.0 {
            object_name.0[2] = Ident::new(to_name.item.clone());
        }
    };

    // TODO(sploiselle): Support renaming schemas and databases.
    match create_stmt {
        Statement::CreateIndex { on_name, .. } => {
            maybe_update_object_name(on_name);
        }
        Statement::CreateSink { from, .. } => {
            maybe_update_object_name(from);
        }
        Statement::CreateSource { .. } => {}
        Statement::CreateView { query, .. } => {
            rewrite_query(from_name, to_name, query)?;
        }
        _ => unreachable!("Internal error: only catalog items need to update item refs"),
    }

    Ok(())
}

/// Rewrites `query`'s references of `from` to `to` or errors if too ambiguous.
fn rewrite_query(from: FullName, to: FullName, query: &mut Query) -> Result<(), String> {
    let from_ident = Ident::new(from.item.clone());
    let to_ident = Ident::new(to.item.clone());
    let qual_depth =
        QueryIdentAgg::determine_qual_depth(&from_ident, Some(to_ident.clone()), query)?;
    CreateSqlRewriter::rewrite_query_with_qual_depth(from, to, qual_depth, query);
    // Ensure that our rewrite didn't didn't introduce ambiguous
    // references to `to_name`.
    match QueryIdentAgg::determine_qual_depth(&to_ident, None, query) {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

fn ambiguous_err(n: &Ident, t: &str) -> String {
    format!("\"{}\" potentially used ambiguously as item and {}", n, t)
}

/// Visits a [`Query`], assessing catalog item [`Ident`]s' use of a specified `Ident`.
struct QueryIdentAgg<'a> {
    /// The name whose usage you want to assess.
    name: &'a Ident,
    /// Tracks all second-level qualifiers used on `name` in a `HashMap`, as
    /// well as any third-level qualifiers used on those second-level qualifiers
    /// in a `HashSet`.
    qualifiers: HashMap<Ident, HashSet<Ident>>,
    /// Tracks the least qualified instance of `name` seen.
    min_qual_depth: usize,
    /// Provides an option to fail the visit if encounters a specified `Ident`.
    fail_on: Option<Ident>,
    err: Option<String>,
}

impl<'a> QueryIdentAgg<'a> {
    /// Determines the depth of qualification needed to unambiguously reference
    /// catalog items in a [`Query`].
    ///
    /// Includes an option to fail if a given `Ident` is encountered.
    ///
    /// `Result`s of `Ok(usize)` indicate that `name` can be unambiguously
    /// referred to with `usize` parts, e.g. 2 requires schema and item name
    /// qualification.
    ///
    /// `Result`s of `Err` indicate that we cannot unambiguously reference
    /// `name` or encountered `fail_on`, if it's provided.
    fn determine_qual_depth(
        name: &Ident,
        fail_on: Option<Ident>,
        query: &Query,
    ) -> Result<usize, String> {
        let mut v = QueryIdentAgg {
            qualifiers: HashMap::new(),
            min_qual_depth: usize::MAX,
            err: None,
            name,
            fail_on,
        };

        // Aggregate identities in `v`.
        v.visit_query(query);
        // Not possible to have a qualification depth of 0;
        assert!(v.min_qual_depth > 0);

        if let Some(e) = v.err {
            return Err(e);
        }

        // Check if there was more than one 3rd-level (e.g.
        // database) qualification used for any reference to `name`.
        let req_depth = if v.qualifiers.values().any(|v| v.len() > 1) {
            3
        // Check if there was more than one 2nd-level (e.g. schema)
        // qualification used for any reference to `name`.
        } else if v.qualifiers.len() > 1 {
            2
        } else {
            1
        };

        if v.min_qual_depth < req_depth {
            Err(format!(
                "\"{}\" is not sufficiently qualified to support renaming",
                name
            ))
        } else {
            Ok(req_depth)
        }
    }

    // Assesses `v` for uses of `self.name` and `self.fail_on`.
    fn check_failure(&mut self, v: &[Ident]) {
        // Fail if we encounter `self.fail_on`.
        if let Some(f) = &self.fail_on {
            if v.iter().any(|i| i == f) {
                self.err = Some(format!(
                    "found reference to \"{}\"; cannot rename \"{}\" to any identity \
                    used in any existing view definitions",
                    f, self.name
                ));
                return;
            }
        }
    }
}

impl<'a, 'ast> Visit<'ast> for QueryIdentAgg<'a> {
    fn visit_expr(&mut self, e: &'ast Expr) {
        match e {
            Expr::Identifier(i) => {
                self.check_failure(i);
                if let Some(p) = i.iter().rposition(|e| e == self.name) {
                    if p == i.len() - 1 {
                        // `self.name` used as a column if it's in the final
                        // position here, e.g. `SELECT view.col FROM ...`
                        self.err = Some(ambiguous_err(self.name, "column"));
                        return;
                    }
                    self.min_qual_depth = std::cmp::min(p + 1, self.min_qual_depth);
                }
            }
            Expr::QualifiedWildcard(i) => {
                self.check_failure(i);
                if let Some(p) = i.iter().rposition(|e| e == self.name) {
                    self.min_qual_depth = std::cmp::min(p + 1, self.min_qual_depth);
                }
            }
            _ => visit::visit_expr(self, e),
        }
    }
    fn visit_ident(&mut self, ident: &'ast Ident) {
        self.check_failure(&vec![ident.clone()]);
        // This is an unqualified item using `self.name`, e.g. an alias, which
        // we cannot unambiguously resolve.
        if ident == self.name {
            self.err = Some(ambiguous_err(self.name, "alias or column"));
        }
    }
    fn visit_object_name(&mut self, object_name: &'ast ObjectName) {
        let names = &object_name.0;
        self.check_failure(names);
        // Every item is used as an `ObjectName` at least once, which
        // lets use track all items named `self.name`.
        if let Some(p) = names.iter().rposition(|e| e == self.name) {
            // Name used as last element of `<db>.<schema>.<item>`
            if p == names.len() - 1 && names.len() == 3 {
                self.qualifiers
                    .entry(names[1].clone())
                    .or_default()
                    .insert(names[0].clone());
                self.min_qual_depth = std::cmp::min(3, self.min_qual_depth);
            } else {
                // Any other use is a database or schema
                self.err = Some(ambiguous_err(self.name, "database, schema, or function"))
            }
        }
    }
}

struct CreateSqlRewriter {
    from: Vec<Ident>,
    to: Vec<Ident>,
}

impl CreateSqlRewriter {
    fn rewrite_query_with_qual_depth(
        from_name: FullName,
        to_name: FullName,
        qual_depth: usize,
        query: &mut Query,
    ) {
        let (from, to) = match qual_depth {
            1 => (
                vec![Ident::new(from_name.item)],
                vec![Ident::new(to_name.item)],
            ),
            2 => (
                vec![Ident::new(from_name.schema), Ident::new(from_name.item)],
                vec![Ident::new(to_name.schema), Ident::new(to_name.item)],
            ),
            3 => (
                vec![
                    Ident::new(from_name.database.to_string()),
                    Ident::new(from_name.schema),
                    Ident::new(from_name.item),
                ],
                vec![
                    Ident::new(to_name.database.to_string()),
                    Ident::new(to_name.schema),
                    Ident::new(to_name.item),
                ],
            ),
            _ => unreachable!(),
        };
        let mut v = CreateSqlRewriter { from, to };
        v.visit_query_mut(query);
    }

    fn maybe_rewrite_idents(&mut self, name: &mut Vec<Ident>) {
        // We don't want to rewrite if the item we're rewriting is shorter than
        // the values we want to replace them with.
        if name.len() < self.from.len() {
            return;
        }
        let from_len = self.from.len();
        for i in 0..name.len() - from_len + 1 {
            // If subset of `name` matches `self.from`...
            if name[i..i + from_len] == self.from[..] {
                // ...splice `self.to` into `name` in that subset's location.
                name.splice(i..i + from_len, self.to.iter().cloned());
                return;
            }
        }
    }
}

impl<'ast> VisitMut<'ast> for CreateSqlRewriter {
    fn visit_expr_mut(&mut self, e: &'ast mut Expr) {
        match e {
            Expr::Identifier(i) | Expr::QualifiedWildcard(i) => {
                self.maybe_rewrite_idents(i);
            }
            _ => visit_mut::visit_expr_mut(self, e),
        }
    }
    fn visit_object_name_mut(&mut self, object_name: &'ast mut ObjectName) {
        self.maybe_rewrite_idents(&mut object_name.0);
    }
}
