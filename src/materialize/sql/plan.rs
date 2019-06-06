// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use failure::bail;
use sqlparser::sqlast::SQLFunction;

use crate::dataflow::func::BinaryFunc;
use crate::dataflow::{AggregateExpr, RelationExpr, ScalarExpr};
use crate::repr::{ColumnType, RelationType};
use ore::option::OptionExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Name {
    table_name: Option<String>,
    column_name: Option<String>,
    func_hash: Option<u64>,
}

impl Name {
    pub fn none() -> Self {
        Name {
            table_name: None,
            column_name: None,
            func_hash: None,
        }
    }
}

/// Wraps a dataflow relation_expr with a sql scope
#[derive(Debug, Clone)]
pub struct SQLRelationExpr {
    pub relation_expr: RelationExpr,
    pub columns: Vec<(Name, ColumnType)>,
}

impl SQLRelationExpr {
    pub fn from_source(name: &str, types: Vec<ColumnType>) -> Self {
        SQLRelationExpr {
            relation_expr: RelationExpr::Get {
                name: name.to_owned(),
                typ: RelationType {
                    column_types: types.clone(),
                },
            },
            columns: types
                .into_iter()
                .map(|typ| {
                    (
                        Name {
                            table_name: Some(name.to_owned()),
                            column_name: typ.name.clone(),
                            func_hash: None,
                        },
                        typ,
                    )
                })
                .collect(),
        }
    }

    pub fn alias_table(mut self, table_name: &str) -> Self {
        for (name, _) in &mut self.columns {
            name.table_name = Some(table_name.to_owned());
        }
        self
    }

    pub fn resolve_column(
        &self,
        column_name: &str,
    ) -> Result<(usize, &Name, &ColumnType), failure::Error> {
        let mut results = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| name.column_name.as_deref() == Some(column_name));
        match (results.next(), results.next()) {
            (None, None) => bail!("no column named {} in scope", column_name),
            (Some((i, (name, typ))), None) => Ok((i, name, typ)),
            (Some(_), Some(_)) => bail!("column name {} is ambiguous", column_name),
            _ => unreachable!(),
        }
    }

    pub fn resolve_table_column(
        &self,
        table_name: &str,
        column_name: &str,
    ) -> Result<(usize, &Name, &ColumnType), failure::Error> {
        let mut results = self.columns.iter().enumerate().filter(|(_, (name, _))| {
            name.table_name.as_deref() == Some(table_name)
                && name.column_name.as_deref() == Some(column_name)
        });
        match (results.next(), results.next()) {
            (None, None) => bail!("no column named {}.{} in scope", table_name, column_name),
            (Some((i, (name, typ))), None) => Ok((i, name, typ)),
            (Some(_), Some(_)) => bail!("column name {}.{} is ambiguous", table_name, column_name),
            _ => unreachable!(),
        }
    }

    pub fn resolve_func<'a, 'b>(&'a self, func: &'b SQLFunction) -> (usize, &'a ColumnType) {
        let func_hash = ore::hash::hash(func);
        let mut results = self
            .columns
            .iter()
            .enumerate()
            .filter(|(_, (name, _))| name.func_hash == Some(func_hash));
        match (results.next(), results.next()) {
            (None, None) => panic!("no func hash {:?} in scope", func_hash),
            (Some((i, (_, typ))), None) => (i, typ),
            (Some(_), Some(_)) => panic!("func hash {:?} is ambiguous", func_hash),
            _ => unreachable!(),
        }
    }

    pub fn project(self, project_key: &[usize]) -> Self {
        let SQLRelationExpr {
            relation_expr,
            columns,
        } = self;
        SQLRelationExpr {
            relation_expr: RelationExpr::Project {
                outputs: project_key.to_vec(),
                input: Box::new(relation_expr),
            },
            columns: project_key.iter().map(|i| columns[*i].clone()).collect(),
        }
    }

    pub fn product(self, right: Self) -> Self {
        let SQLRelationExpr {
            relation_expr: left_relation_expr,
            columns: left_columns,
        } = self;
        let SQLRelationExpr {
            relation_expr: right_relation_expr,
            columns: right_columns,
        } = right;
        SQLRelationExpr {
            relation_expr: RelationExpr::Join {
                inputs: vec![left_relation_expr, right_relation_expr],
                variables: vec![],
            },
            columns: left_columns
                .into_iter()
                .chain(right_columns.into_iter())
                .collect(),
        }
    }

    pub fn filter(mut self, predicate: ScalarExpr) -> Self {
        let mut predicates = vec![];
        fn get_predicates(predicate: ScalarExpr, predicates: &mut Vec<ScalarExpr>) {
            match predicate {
                ScalarExpr::CallBinary {
                    func: BinaryFunc::And,
                    expr1,
                    expr2,
                } => {
                    get_predicates(*expr1, predicates);
                    get_predicates(*expr2, predicates);
                }
                _ => predicates.push(predicate),
            }
        }
        get_predicates(predicate, &mut predicates);
        self.relation_expr = RelationExpr::Filter {
            predicates,
            input: Box::new(self.relation_expr),
        };
        self
    }

    pub fn reduce(
        self,
        group_key: Vec<(ScalarExpr, ColumnType)>,
        group_names: Vec<Name>,
        aggregates: Vec<(&SQLFunction, AggregateExpr, ColumnType)>,
    ) -> Self {
        let key_columns = group_key
            .iter()
            .zip(group_names.into_iter())
            .map(|(key, name)| (name, key.1.clone()))
            .collect::<Vec<_>>();
        let (
            SQLRelationExpr {
                mut relation_expr, ..
            },
            group_key,
        ) = self.map(group_key);
        // Deduplicate by function hash.
        let mut aggregates: Vec<_> = aggregates
            .into_iter()
            .map(|(func, agg, typ)| (ore::hash::hash(func), agg, typ))
            .collect();
        aggregates.sort_by_key(|(func_hash, _agg, _typ)| *func_hash);
        aggregates.dedup_by_key(|(func_hash, _agg, _typ)| *func_hash);
        let mut agg_columns = Vec::new();
        let mut aggs = Vec::new();
        for (func_hash, agg, typ) in aggregates.iter() {
            agg_columns.push((
                Name {
                    table_name: None,
                    column_name: None,
                    func_hash: Some(*func_hash),
                },
                typ.clone(),
            ));
            aggs.push((agg.clone(), typ.clone()));
        }
        relation_expr = RelationExpr::Reduce {
            input: Box::new(relation_expr),
            group_key: group_key.clone(),
            aggregates: aggs,
        };
        if group_key.is_empty() {
            relation_expr = RelationExpr::OrDefault {
                input: Box::new(relation_expr),
                default: aggregates
                    .iter()
                    .map(|(_, agg, _)| agg.func.default())
                    .collect(),
            }
        }
        SQLRelationExpr {
            relation_expr,
            columns: key_columns.into_iter().chain(agg_columns).collect(),
        }
    }

    pub fn select(self, outputs: Vec<(ScalarExpr, ColumnType)>) -> Self {
        let input_arity = self.columns.len();
        let SQLRelationExpr { relation_expr, .. } = self;
        let mut map_scalars = vec![];
        let mut project_outputs = vec![];
        let mut columns = vec![];
        for (expr, typ) in outputs {
            if let ScalarExpr::Column(i) = expr {
                project_outputs.push(i);
            } else {
                project_outputs.push(input_arity + map_scalars.len());
                map_scalars.push((expr, typ.clone()));
            }
            columns.push((
                Name {
                    table_name: None,
                    column_name: typ.name.clone(),
                    func_hash: None,
                },
                typ.clone(),
            ));
        }
        SQLRelationExpr {
            relation_expr: RelationExpr::Project {
                outputs: project_outputs,
                input: Box::new(RelationExpr::Map {
                    scalars: map_scalars,
                    input: Box::new(relation_expr),
                }),
            },
            columns,
        }
    }

    pub fn map(self, outputs: Vec<(ScalarExpr, ColumnType)>) -> (Self, Vec<usize>) {
        let input_arity = self.columns.len();
        let SQLRelationExpr {
            relation_expr,
            mut columns,
        } = self;
        let mut map_scalars = vec![];
        let mut project_outputs = vec![];
        for (expr, typ) in outputs {
            if let ScalarExpr::Column(i) = expr {
                project_outputs.push(i);
            } else {
                project_outputs.push(input_arity + map_scalars.len());
                map_scalars.push((expr, typ.clone()));
                columns.push((
                    Name {
                        table_name: None,
                        column_name: typ.name.clone(),
                        func_hash: None,
                    },
                    typ.clone(),
                ));
            }
        }
        (
            SQLRelationExpr {
                relation_expr: RelationExpr::Map {
                    scalars: map_scalars,
                    input: Box::new(relation_expr),
                },
                columns,
            },
            project_outputs,
        )
    }

    pub fn distinct(mut self) -> Self {
        self.relation_expr = RelationExpr::Distinct {
            input: Box::new(self.relation_expr),
        };
        self
    }

    pub fn named_columns(&self) -> Vec<(String, String)> {
        self.columns
            .iter()
            .filter_map(|(name, _)| match (&name.table_name, &name.column_name) {
                (Some(table_name), Some(column_name)) => {
                    Some((table_name.clone(), column_name.clone()))
                }
                _ => None,
            })
            .collect()
    }

    pub fn finish(self) -> (RelationExpr, RelationType) {
        let SQLRelationExpr {
            relation_expr,
            columns,
        } = self;
        (
            relation_expr,
            RelationType {
                column_types: columns.into_iter().map(|(_, typ)| typ).collect(),
            },
        )
    }
}
