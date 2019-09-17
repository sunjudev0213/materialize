// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

//! Handles SQLs scoping rules.

//! A scope spans a single SQL `Query`.
//! Nested subqueries create new scopes.
//! Names are resolved against the innermost scope first.
//! * If a match is found, it is returned
//! * If no matches are found, the name is resolved against the parent scope
//! * If multiple matches are found, the name is ambigious and we return an error to the user

//! Matching rules:
//! * `bar` will match any column in the scope named `bar`
//! * `foo.bar` will match any column in the scope named `bar` that originated from a table named `foo`
//! * Table aliases such as `foo as quux` replace the old table name.
//! * Functions create unnamed columns, which can be named with columns aliases `(bar + 1) as more_bar`

//! Many sql expressions do strange and arbitrary things to scopes. Rather than try to capture them all here, we just expose the internals of `Scope` and handle it in the appropriate place in `super::query`.

use super::expr::ColumnRef;
pub use super::session::Session;
use failure::bail;
use ore::option::OptionExt;
use repr::{ColumnType, RelationType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeItemName {
    pub table_name: Option<String>,
    pub column_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeItem {
    pub names: Vec<ScopeItemName>,
    pub typ: ColumnType,
}

#[derive(Debug, Clone)]
pub struct Scope {
    // items in this query
    pub items: Vec<ScopeItem>,
    // items inherited from an enclosing query
    pub outer_items: Vec<ScopeItem>,
}

#[derive(Debug)]
enum Resolution<'a> {
    NotFound,
    Found((usize, &'a ScopeItem)),
    Ambiguous,
}

impl Scope {
    pub fn empty(outer_scope: Option<Scope>) -> Self {
        Scope {
            items: vec![],
            outer_items: if let Some(outer_scope) = outer_scope {
                outer_scope
                    .outer_items
                    .into_iter()
                    .chain(outer_scope.items.into_iter())
                    .collect()
            } else {
                vec![]
            },
        }
    }

    pub fn from_source(
        table_name: Option<&str>,
        typ: RelationType,
        outer_scope: Option<Scope>,
    ) -> Self {
        let mut scope = Scope::empty(outer_scope);
        scope.items = typ
            .column_types
            .into_iter()
            .map(|typ| ScopeItem {
                names: vec![ScopeItemName {
                    table_name: table_name.owned(),
                    column_name: typ.name.clone(),
                }],
                typ,
            })
            .collect();
        scope
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    fn resolve<'a, Matches>(
        &'a self,
        matches: Matches,
        name_in_error: &str,
    ) -> Result<(ColumnRef, &'a ScopeItem), failure::Error>
    where
        Matches: Fn(&ScopeItemName) -> bool,
    {
        let resolve_over = |items: &'a [ScopeItem]| {
            let mut results = items
                .iter()
                .enumerate()
                .map(|(pos, item)| item.names.iter().map(move |name| (pos, item, name)))
                .flatten()
                .filter(|(_, _, name)| (matches)(name));
            match results.next() {
                None => Resolution::NotFound,
                Some((pos, item, _name)) => {
                    if results.find(|(pos2, _item, _name)| pos != *pos2).is_none() {
                        Resolution::Found((pos, item))
                    } else {
                        Resolution::Ambiguous
                    }
                }
            }
        };
        match resolve_over(&self.items) {
            Resolution::NotFound => match resolve_over(&self.outer_items) {
                Resolution::NotFound => bail!("No column named {} in scope", name_in_error),
                Resolution::Found((pos, item)) => Ok((ColumnRef::Outer(pos), item)),
                Resolution::Ambiguous => bail!("Column name {} is ambiguous", name_in_error),
            },
            Resolution::Found((pos, item)) => Ok((ColumnRef::Inner(pos), item)),
            Resolution::Ambiguous => bail!("Column name {} is ambiguous", name_in_error),
        }
    }

    pub fn resolve_column<'a>(
        &'a self,
        column_name: &str,
    ) -> Result<(ColumnRef, &'a ScopeItem), failure::Error> {
        self.resolve(
            |item: &ScopeItemName| item.column_name.as_deref() == Some(column_name),
            column_name,
        )
    }

    pub fn resolve_table_column<'a>(
        &'a self,
        table_name: &str,
        column_name: &str,
    ) -> Result<(ColumnRef, &'a ScopeItem), failure::Error> {
        self.resolve(
            |item: &ScopeItemName| {
                item.table_name.as_deref() == Some(table_name)
                    && item.column_name.as_deref() == Some(column_name)
            },
            &format!("{}.{}", table_name, column_name),
        )
    }

    pub fn product(self, right: Self) -> Self {
        assert!(self.outer_items == right.outer_items);
        Scope {
            items: self
                .items
                .into_iter()
                .chain(right.items.into_iter())
                .collect(),
            outer_items: self.outer_items,
        }
    }

    pub fn project(&self, columns: &[usize]) -> Self {
        Scope {
            items: columns.iter().map(|&i| self.items[i].clone()).collect(),
            outer_items: self.outer_items.clone(),
        }
    }
}
