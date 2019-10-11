// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use failure::bail;
use serde::{Deserialize, Serialize};

use ore::option::OptionExt;

use crate::ScalarType;

/// The type of a [`Datum`].
///
/// [`ColumnType`] bundles information about the scalar type of a datum (e.g.,
/// Int32 or String) with additional attributes, likeits nullability.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct ColumnType {
    /// Whether this datum can be null.
    pub nullable: bool,
    /// The underlying scalar type (e.g., Int32 or String) of this column.
    pub scalar_type: ScalarType,
}

impl ColumnType {
    /// Constructs a new `ColumnType` with the specified [`ScalarType`] as its
    /// underlying type. If desired, the `nullable` property can be set with the
    /// methods of the same name.
    pub fn new(scalar_type: ScalarType) -> Self {
        ColumnType {
            nullable: if let ScalarType::Null = scalar_type {
                true
            } else {
                false
            },
            scalar_type,
        }
    }

    pub fn union(&self, other: &Self) -> Result<Self, failure::Error> {
        let scalar_type = match (&self.scalar_type, &other.scalar_type) {
            (ScalarType::Null, s) | (s, ScalarType::Null) => s,
            (s1, s2) if s1 == s2 => s1,
            (s1, s2) => bail!("Can't union types: {:?} and {:?}", s1, s2),
        };
        Ok(ColumnType {
            scalar_type: scalar_type.clone(),
            nullable: self.nullable
                || other.nullable
                || self.scalar_type == ScalarType::Null
                || other.scalar_type == ScalarType::Null,
        })
    }

    /// Consumes this `ColumnType` and returns a new `ColumnType` with its
    /// nullability set to the specified boolean.
    pub fn nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }

    /// Required outside of builder contexts.
    pub fn set_nullable(&mut self, nullable: bool) {
        self.nullable = nullable
    }
}

/// The type for a relation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct RelationType {
    /// The type for each column, in order.
    pub column_types: Vec<ColumnType>,
    /// Sets of indices that are "keys" for the collection.
    ///
    /// Each element in this list is a set of column indices, each with the property
    /// that the collection contains at most one record with each distinct set of values
    /// for each column. Alternately, for a specific set of values assigned to the these
    /// columns there is at most one record.
    ///
    /// A collection can contain multiple sets of keys, although it is common to have
    /// either zero or one sets of key indices.
    pub keys: Vec<Vec<usize>>,
}

impl RelationType {
    /// Creates a new instance from specified column types.
    pub fn new(column_types: Vec<ColumnType>) -> Self {
        RelationType {
            column_types,
            keys: Vec::new(),
        }
    }

    /// Adds a set of indices as keys for the colleciton.
    pub fn add_keys(mut self, mut indices: Vec<usize>) -> Self {
        indices.sort();
        if !self.keys.contains(&indices) {
            self.keys.push(indices);
        }
        self
    }
}

/// A complete relation description. Bundles together a `RelationType` with
/// additional metadata, like the names of each column.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
pub struct RelationDesc {
    typ: RelationType,
    names: Vec<Option<String>>,
}

impl RelationDesc {
    /// Constructs a new `RelationDesc` that represents a relation with no
    /// columns and no keys.
    pub fn empty() -> RelationDesc {
        RelationDesc {
            typ: RelationType::new(vec![]),
            names: vec![],
        }
    }

    /// Constructs a new `RelationDesc` from a `RelationType` and a list of
    /// column names.
    pub fn new<I, S>(typ: RelationType, names: I) -> RelationDesc
    where
        I: IntoIterator<Item = Option<S>>,
        S: Into<String>,
    {
        let names: Vec<_> = names.into_iter().map(|n| n.map(Into::into)).collect();
        assert_eq!(typ.column_types.len(), names.len());
        RelationDesc { typ, names }
    }

    /// Adds a new named, nonnullable column with the specified type.
    pub fn add_column(mut self, name: impl Into<String>, scalar_type: ScalarType) -> Self {
        self.typ.column_types.push(ColumnType::new(scalar_type));
        self.names.push(Some(name.into()));
        self
    }

    /// Adds a set of indices as keys for the relation.
    pub fn add_keys(mut self, mut indices: Vec<usize>) -> Self {
        indices.sort();
        if !self.typ.keys.contains(&indices) {
            self.typ.keys.push(indices);
        }
        self
    }

    pub fn typ(&self) -> &RelationType {
        &self.typ
    }

    pub fn iter(&self) -> impl Iterator<Item = (Option<&str>, &ColumnType)> {
        self.iter_names().zip(self.iter_types())
    }

    pub fn iter_types(&self) -> impl Iterator<Item = &ColumnType> {
        self.typ.column_types.iter()
    }

    pub fn iter_names(&self) -> impl Iterator<Item = Option<&str>> {
        self.names.iter().map(|n| n.mz_as_deref())
    }

    pub fn get_by_name(&self, name: &str) -> Option<(usize, &ColumnType)> {
        self.iter_names()
            .position(|n| n == Some(name))
            .map(|i| (i, &self.typ.column_types[i]))
    }

    pub fn set_name(&mut self, i: usize, name: Option<String>) {
        self.names[i] = name
    }
}
