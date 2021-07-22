// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A persistent, compacting data structure of `(Key, Value, Time, Diff)`
//! updates, indexed by time.

use std::marker::PhantomData;
use std::sync::Arc;

use timely::progress::Antichain;
use timely::PartialOrder;

use crate::error::Error;
use crate::indexed::cache::BlobCache;
use crate::indexed::encoding::{BlobFutureBatchMeta, BlobFutureMeta};
use crate::indexed::{BlobFutureBatch, Id, Snapshot};
use crate::storage::{Blob, SeqNo};
use crate::Data;

/// A persistent, compacting data structure containing `(Key, Value, Time,
/// Diff)` entries indexed by `(time, key, value)`.
///
/// Invariants:
/// - All entries are after or equal to some time frontier and less than some
///   SeqNo. Intuitively, this is the data that has been drained out of a
///   [crate::storage::Buffer] but not yet "seal"ed into a
///   [crate::indexed::trace::BlobTrace].
/// - TODO: Space usage.
pub struct BlobFuture<K, V> {
    id: Id,
    // The next id used to assign a Blob key for this future.
    next_blob_id: u64,
    // NB: This is a closed lower bound. When Indexed seals a time, only data
    // strictly before that time gets moved into the trace.
    ts_lower: Antichain<u64>,
    batches: Vec<BlobFutureBatchMeta>,
    _phantom: PhantomData<(K, V)>,
}

impl<K: Data, V: Data> BlobFuture<K, V> {
    /// Returns a BlobFuture re-instantiated with the previously serialized
    /// state.
    pub fn new(meta: BlobFutureMeta) -> Self {
        BlobFuture {
            id: meta.id,
            next_blob_id: meta.next_blob_id,
            ts_lower: meta.ts_lower,
            batches: meta.batches,
            _phantom: PhantomData,
        }
    }

    // Get a new key to write to the Blob store for this future.
    fn new_blob_key(&mut self) -> String {
        let key = format!("{:?}-future-{:?}", self.id, self.next_blob_id);
        self.next_blob_id += 1;

        key
    }

    /// Serializes the state of this BlobFuture for later re-instantiation.
    pub fn meta(&self) -> BlobFutureMeta {
        BlobFutureMeta {
            id: self.id,
            ts_lower: self.ts_lower.clone(),
            batches: self.batches.clone(),
            next_blob_id: self.next_blob_id,
        }
    }

    /// An open upper bound on the seqnos of contained updates.
    pub fn seqno_upper(&self) -> Antichain<SeqNo> {
        self.batches.last().map_or_else(
            || Antichain::from_elem(SeqNo(0)),
            |meta| meta.desc.upper().clone(),
        )
    }

    /// Writes the given batch to [Blob] storage and logically adds the contained
    /// updates to this future.
    pub fn append<L: Blob>(
        &mut self,
        batch: BlobFutureBatch<K, V>,
        blob: &mut BlobCache<K, V, L>,
    ) -> Result<(), Error> {
        if batch.desc.lower() != &self.seqno_upper() {
            return Err(Error::from(format!(
                "batch lower doesn't match seqno_upper {:?}: {:?}",
                self.seqno_upper(),
                batch.desc
            )));
        }
        if cfg!(any(debug, test)) {
            // Batches being appended to this future come from data being
            // drained out of the buffer. Indexed should have prevented this
            // write to the buffer, so this should never happen. Hopefully any
            // regressions in maintaining this invariant will be caught by this
            // debug/test check.
            for (_, ts, _) in batch.updates.iter() {
                if !self.ts_lower.less_equal(ts) {
                    return Err(Error::from(format!(
                        "batch contains timestamp {:?} before ts_lower: {:?}",
                        ts, self.ts_lower
                    )));
                }
            }
        }

        let desc = batch.desc.clone();
        let key = self.new_blob_key();
        blob.set_future_batch(key.clone(), batch)?;
        self.batches.push(BlobFutureBatchMeta { key, desc });
        Ok(())
    }

    /// Returns a consistent read of the updates contained in this future
    /// matching the given filters (in practice, everything not in Trace).
    pub fn snapshot<L: Blob>(
        &self,
        ts_lower: Antichain<u64>,
        ts_upper: Option<Antichain<u64>>,
        blob: &BlobCache<K, V, L>,
    ) -> Result<FutureSnapshot<K, V>, Error> {
        let mut updates = Vec::with_capacity(self.batches.len());
        for meta in self.batches.iter() {
            updates.push(blob.get_future_batch(&meta.key)?);
        }
        Ok(FutureSnapshot {
            seqno_upper: self.seqno_upper(),
            ts_lower,
            ts_upper,
            updates,
        })
    }

    /// Removes all updates contained in this future before the given bound.
    pub fn truncate(&mut self, new_ts_lower: Antichain<u64>) -> Result<(), Error> {
        if PartialOrder::less_than(&new_ts_lower, &self.ts_lower) {
            return Err(format!(
                "cannot regress ts_lower from {:?} to {:?}",
                self.ts_lower, new_ts_lower
            )
            .into());
        }
        self.ts_lower = new_ts_lower;
        // TODO: Actually delete the data.
        Ok(())
    }
}

/// A consistent snapshot of the data currently in a persistent [BlobFuture].
#[derive(Debug)]
pub struct FutureSnapshot<K, V> {
    /// An open upper bound on the seqnos of contained updates.
    pub seqno_upper: Antichain<SeqNo>,
    /// A closed lower bound on the times of contained updates.
    pub ts_lower: Antichain<u64>,
    /// An optional open upper bound on the times of the contained updates.
    pub ts_upper: Option<Antichain<u64>>,
    updates: Vec<Arc<BlobFutureBatch<K, V>>>,
}

impl<K: Clone, V: Clone> Snapshot<K, V> for FutureSnapshot<K, V> {
    fn read<E: Extend<((K, V), u64, isize)>>(&mut self, buf: &mut E) -> bool {
        if let Some(batch) = self.updates.pop() {
            let updates = batch
                .updates
                .iter()
                .filter(|(_, ts, _)| {
                    self.ts_lower.less_equal(ts)
                        && self.ts_upper.as_ref().map_or(true, |u| !u.less_equal(ts))
                })
                .map(|((key, val), ts, diff)| ((key.clone(), val.clone()), *ts, *diff));
            buf.extend(updates);
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use differential_dataflow::trace::Description;

    use crate::mem::MemBlob;

    use super::*;

    #[test]
    fn append_ts_lower_invariant() {
        let mut blob = BlobCache::new(MemBlob::new("append_ts_lower_invariant"));
        let mut f = BlobFuture::new(BlobFutureMeta {
            id: Id(0),
            ts_lower: Antichain::from_elem(2),
            batches: vec![],
            next_blob_id: 0,
        });

        // ts < ts_lower.data()[0] is disallowed
        let batch = BlobFutureBatch {
            desc: Description::new(
                Antichain::from_elem(SeqNo(0)),
                Antichain::from_elem(SeqNo(1)),
                Antichain::from_elem(SeqNo(0)),
            ),
            updates: vec![(("k".to_string(), "v".to_string()), 1, 1)],
        };
        assert_eq!(
            f.append(batch, &mut blob),
            Err(Error::from(
                "batch contains timestamp 1 before ts_lower: Antichain { elements: [2] }"
            ))
        );

        // ts == ts_lower.data()[0] is allowed
        let batch = BlobFutureBatch {
            desc: Description::new(
                Antichain::from_elem(SeqNo(0)),
                Antichain::from_elem(SeqNo(1)),
                Antichain::from_elem(SeqNo(0)),
            ),
            updates: vec![(("k".to_string(), "v".to_string()), 2, 1)],
        };
        assert_eq!(f.append(batch, &mut blob), Ok(()));
    }

    #[test]
    fn truncate_regress() {
        let mut f: BlobFuture<String, String> = BlobFuture::new(BlobFutureMeta {
            id: Id(0),
            ts_lower: Antichain::from_elem(2),
            batches: vec![],
            next_blob_id: 0,
        });
        assert_eq!(f.truncate(Antichain::from_elem(2)), Ok(()));
        assert_eq!(
            f.truncate(Antichain::from_elem(1)),
            Err(Error::from(
                "cannot regress ts_lower from Antichain { elements: [2] } to Antichain { elements: [1] }"
            ))
        );
    }
}
