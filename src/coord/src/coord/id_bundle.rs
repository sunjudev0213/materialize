// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeSet;

use mz_expr::GlobalId;

/// A bundle of storage and compute identifiers.
#[derive(Debug, Default, Clone)]
pub struct IdBundle {
    /// The identifiers for sources in the storage layer.
    pub storage_ids: BTreeSet<GlobalId>,
    /// The identifiers for indexes in the compute layer.
    pub compute_ids: BTreeSet<GlobalId>,
}

impl IdBundle {
    /// Reports whether the bundle contains any identifiers of any type.
    pub fn is_empty(&self) -> bool {
        self.storage_ids.is_empty() && self.compute_ids.is_empty()
    }

    /// Extends the bundle with the identifiers from `other`.
    pub fn extend(&mut self, other: &IdBundle) {
        self.storage_ids.extend(&other.storage_ids);
        self.compute_ids.extend(&other.compute_ids);
    }

    /// Returns a new bundle without the identifiers from `other`.
    pub fn difference(&self, other: &IdBundle) -> IdBundle {
        IdBundle {
            storage_ids: &self.storage_ids - &other.storage_ids,
            compute_ids: &self.compute_ids - &other.compute_ids,
        }
    }

    /// Returns an iterator over all IDs in the bundle.
    ///
    /// The IDs are iterated in an unspecified order.
    pub fn iter(&self) -> impl Iterator<Item = GlobalId> + '_ {
        self.storage_ids
            .iter()
            .copied()
            .chain(self.compute_ids.iter().copied())
    }
}
