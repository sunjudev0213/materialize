// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use failure::{bail, format_err};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::iter::FromIterator;

use crate::dataflow::{Dataflow, LocalSourceConnector, Source, SourceConnector};
use crate::repr::{ColumnType, RelationType, ScalarType};

#[derive(Debug)]
pub struct DataflowStore {
    inner: HashMap<String, DataflowAndMetadata>,
}

#[derive(Debug)]
struct DataflowAndMetadata {
    inner: Dataflow,
    used_by: Vec<String>,
}

impl DataflowStore {
    pub fn get(&self, name: &str) -> Result<&Dataflow, failure::Error> {
        match self.inner.get(name) {
            Some(metadata) => Ok(&metadata.inner),
            None => bail!("dataflow {} does not exist", name),
        }
    }

    pub fn get_type(&self, name: &str) -> Result<&RelationType, failure::Error> {
        Ok(self.get(name)?.typ())
    }

    pub fn insert(&mut self, dataflow: Dataflow) -> Result<(), failure::Error> {
        let name = dataflow.name().to_owned();
        match self.inner.entry(name.clone()) {
            Entry::Occupied(_) => bail!("dataflow {} already exists", name),
            Entry::Vacant(vacancy) => {
                vacancy.insert(DataflowAndMetadata {
                    inner: dataflow.clone(),
                    used_by: Vec::new(),
                });
            }
        }
        for u in dataflow.uses() {
            match self.inner.get_mut(u) {
                Some(entry) => entry.used_by.push(name.clone()),
                None => panic!(
                    "DataflowStore: missing dependent dataflow {} while installing {}",
                    u, name
                ),
            }
        }
        Ok(())
    }

    pub fn remove(
        &mut self,
        name: &str,
        mode: RemoveMode,
        removed: &mut Vec<Dataflow>,
    ) -> Result<(), failure::Error> {
        let metadata = match self.inner.get(name) {
            Some(metadata) => metadata,
            None => return Ok(()),
        };

        match mode {
            RemoveMode::Restrict => {
                if !metadata.used_by.is_empty() {
                    bail!(
                        "cannot delete {}: still depended upon by dataflow '{}'",
                        name,
                        metadata.used_by[0]
                    )
                }
            }
            RemoveMode::Cascade => {
                let used_by = metadata.used_by.clone();
                for u in used_by {
                    // We may have removed other dependent dataflows on a prior
                    // turn of the loop, so cascading removes must not fail, or
                    // we'll have violated atomicity. Therefore unwrap instead
                    // of propagating the error.
                    self.remove(&u, RemoveMode::Cascade, removed).unwrap();
                }
            }
        }

        // Safe to unwrap, because we already proved above that name exists in
        // self.inner.
        let metadata = self.inner.remove(name).unwrap();
        for u in metadata.inner.uses() {
            match self.inner.get_mut(u) {
                Some(entry) => entry.used_by.retain(|u| u != name),
                None => panic!(
                    "DataflowStore: missing dependent dataflow {} while removing {}",
                    u, name
                ),
            }
        }

        removed.push(metadata.inner);

        Ok(())
    }
}

impl Default for DataflowStore {
    fn default() -> DataflowStore {
        let mut store = DataflowStore {
            inner: HashMap::new(),
        };

        // https://en.wikipedia.org/wiki/DUAL_table
        let dual_dataflow = Dataflow::Source(Source {
            name: "dual".into(),
            connector: SourceConnector::Local(LocalSourceConnector {}),
            typ: RelationType {
                column_types: vec![ColumnType {
                    name: Some("x".into()),
                    nullable: false,
                    scalar_type: ScalarType::String,
                }],
            },
        });
        store.insert(dual_dataflow).unwrap();

        store
    }
}

impl FromIterator<Dataflow> for DataflowStore {
    fn from_iter<I: IntoIterator<Item = Dataflow>>(iter: I) -> Self {
        let mut store = DataflowStore::default();
        for dataflow in iter {
            store.insert(dataflow).unwrap();
        }
        store
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum RemoveMode {
    Cascade,
    Restrict,
}

impl RemoveMode {
    pub fn from_cascade(cascade: bool) -> RemoveMode {
        if cascade {
            RemoveMode::Cascade
        } else {
            RemoveMode::Restrict
        }
    }
}
