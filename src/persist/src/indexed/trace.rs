// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A persistent, compacting data structure of `(Key, Value, Time, Diff)`
//! updates, indexed by key.
//!
//! This is directly a persistent analog of [differential_dataflow::trace::Trace].

use std::marker::PhantomData;
use std::sync::Arc;

use differential_dataflow::trace::Description;
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;

use crate::error::Error;
use crate::indexed::cache::BlobCache;
use crate::indexed::encoding::BlobTraceMeta;
use crate::indexed::{BlobTraceBatch, Id, Snapshot};
use crate::storage::Blob;
use crate::Data;

/// A persistent, compacting data structure containing `(Key, Value, Time,
/// Diff)` entries indexed by `(key, value, time)`.
///
/// Invariants:
/// - All entries are before some time frontier.
/// - This acts as an append-only log. Data is added in advancing batches and
///   logically immutable after that (modulo compactions, which preserve it, but
///   the ability to read at old times is lost).
/// - TODO: Explain since and logical compactions.
/// - TODO: Space usage.
pub struct BlobTrace<K, V> {
    id: Id,
    // The next ID used to assign a Blob key for this trace.
    next_blob_id: u64,
    // NB: We may at some point need to break this up into separate logical and
    // physical compaction frontiers.
    since: Antichain<u64>,
    // NB: The Descriptions here are sorted and contiguous half-open intervals
    // `[lower, upper)`.
    batches: Vec<(Description<u64>, String)>,
    _phantom: PhantomData<(K, V)>,
}

impl<K: Data, V: Data> BlobTrace<K, V> {
    /// Returns a BlobTrace re-instantiated with the previously serialized
    /// state.
    pub fn new(meta: BlobTraceMeta) -> Self {
        BlobTrace {
            id: meta.id,
            next_blob_id: meta.next_blob_id,
            since: meta.since,
            batches: meta.batches,
            _phantom: PhantomData,
        }
    }

    fn new_blob_key(&mut self) -> String {
        let key = format!("{:?}-trace-{:?}", self.id, self.next_blob_id);
        self.next_blob_id += 1;

        key
    }

    /// Serializes the state of this BlobTrace for later re-instantiation.
    pub fn meta(&self) -> BlobTraceMeta {
        BlobTraceMeta {
            id: self.id,
            batches: self.batches.clone(),
            since: self.since.clone(),
            next_blob_id: self.next_blob_id,
        }
    }

    /// An upper bound on the times of contained updates.
    pub fn ts_upper(&self) -> Antichain<u64> {
        match self.batches.last() {
            Some((desc, _)) => desc.upper().clone(),
            None => Antichain::from_elem(Timestamp::minimum()),
        }
    }

    /// A lower bound on the time at which updates may have been logically
    /// compacted together.
    pub fn since(&self) -> Antichain<u64> {
        self.since.clone()
    }

    /// Writes the given batch to [Blob] storage and logically adds the contained
    /// updates to this trace.
    pub fn append<L: Blob>(
        &mut self,
        batch: BlobTraceBatch<K, V>,
        blob: &mut BlobCache<K, V, L>,
    ) -> Result<(), Error> {
        if &self.ts_upper() != batch.desc.lower() {
            return Err(Error::from(format!(
                "batch lower doesn't match trace upper {:?}: {:?}",
                self.ts_upper(),
                batch.desc
            )));
        }
        let desc = batch.desc.clone();
        let key = self.new_blob_key();
        blob.set_trace_batch(key.clone(), batch)?;
        self.batches.push((desc, key));
        Ok(())
    }

    /// Returns a consistent read of all the updates contained in this trace.
    pub fn snapshot<L: Blob>(
        &self,
        blob: &BlobCache<K, V, L>,
    ) -> Result<TraceSnapshot<K, V>, Error> {
        let ts_upper = self.ts_upper();
        let mut updates = Vec::with_capacity(self.batches.len());
        for (_, key) in self.batches.iter() {
            updates.push(blob.get_trace_batch(key)?);
        }
        Ok(TraceSnapshot { ts_upper, updates })
    }

    /// Update the compaction frontier to `since`.
    pub fn allow_compaction(&mut self, since: Antichain<u64>) -> Result<(), Error> {
        if PartialOrder::less_equal(&self.ts_upper(), &since) {
            return Err(Error::from(format!(
                "invalid compaction at or in advance of trace upper {:?}: {:?}",
                self.ts_upper(),
                since,
            )));
        }

        if PartialOrder::less_equal(&since, &self.since) {
            return Err(Error::from(format!(
                "invalid compaction less than or equal to trace since {:?}: {:?}",
                self.since, since
            )));
        }

        self.since = since;
        Ok(())
    }
}

/// A consistent snapshot of the data currently in a persistent [BlobTrace].
#[derive(Debug)]
pub struct TraceSnapshot<K, V> {
    /// An open upper bound on the times of contained updates.
    pub ts_upper: Antichain<u64>,
    updates: Vec<Arc<BlobTraceBatch<K, V>>>,
}

impl<K: Clone, V: Clone> Snapshot<K, V> for TraceSnapshot<K, V> {
    fn read<E: Extend<((K, V), u64, isize)>>(&mut self, buf: &mut E) -> bool {
        if let Some(batch) = self.updates.pop() {
            buf.extend(batch.updates.iter().cloned());
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allow_compaction() -> Result<(), Error> {
        let mut t: BlobTrace<String, String> = BlobTrace::new(BlobTraceMeta {
            id: Id(0),
            batches: vec![(
                Description::new(
                    Antichain::from_elem(0),
                    Antichain::from_elem(10),
                    Antichain::from_elem(5),
                ),
                "key1".to_string(),
            )],
            since: Antichain::from_elem(5),
            next_blob_id: 0,
        });

        // Normal case: advance since frontier.
        t.allow_compaction(Antichain::from_elem(6))?;

        // Allow compaction at the same since frontier.
        assert_eq!(t.allow_compaction(Antichain::from_elem(6)),
            Err(Error::from("invalid compaction less than or equal to trace since Antichain { elements: [6] }: Antichain { elements: [6] }")));

        // Regress since frontier.
        assert_eq!(t.allow_compaction(Antichain::from_elem(5)),
            Err(Error::from("invalid compaction less than or equal to trace since Antichain { elements: [6] }: Antichain { elements: [5] }")));

        // Advance since frontier to upper
        assert_eq!(t.allow_compaction(Antichain::from_elem(10)),
            Err(Error::from("invalid compaction at or in advance of trace upper Antichain { elements: [10] }: Antichain { elements: [10] }")));

        // Advance since frontier beyond upper
        assert_eq!(t.allow_compaction(Antichain::from_elem(11)),
            Err(Error::from("invalid compaction at or in advance of trace upper Antichain { elements: [10] }: Antichain { elements: [11] }")));

        Ok(())
    }
}
