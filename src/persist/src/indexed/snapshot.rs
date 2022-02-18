// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Isolated, consistent reads of previously written (Key, Value, Time, Diff)
//! updates.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;

use differential_dataflow::lattice::Lattice;
use timely::progress::Antichain;

use crate::error::Error;
use crate::indexed::columnar::ColumnarRecords;
use crate::indexed::encoding::BlobTraceBatchPart;
use crate::indexed::BlobUnsealedBatch;
use crate::pfuture::PFuture;
use crate::storage::SeqNo;

/// An isolated, consistent read of previously written (Key, Value, Time, Diff)
/// updates.
//
// TODO: This <K, V> allows Snapshot to be generic over both IndexedSnapshot
// (and friends) and DecodedSnapshot, but does that get us anything?
pub trait Snapshot<K, V>: Sized {
    /// The kind of iterator we are turning this into.
    type Iter: Iterator<Item = Result<((K, V), u64, i64), Error>>;

    /// Returns a set of `num_iters` [Iterator]s that each output roughly
    /// `1/num_iters` of the data represented by this snapshot.
    fn into_iters(self, num_iters: NonZeroUsize) -> Vec<Self::Iter>;

    /// Returns a single [Iterator] that outputs the data represented by this
    /// snapshot.
    fn into_iter(self) -> Self::Iter {
        let mut iters = self.into_iters(NonZeroUsize::new(1).unwrap());
        assert_eq!(iters.len(), 1);
        iters.remove(0)
    }
}

/// Extension methods on `Snapshot<K, V>` for use in tests.
#[cfg(test)]
pub trait SnapshotExt<K: Ord, V: Ord>: Snapshot<K, V> + Sized {
    /// A full read of the data in the snapshot.
    fn read_to_end(self) -> Result<Vec<((K, V), u64, i64)>, Error> {
        let iter = self.into_iter();
        let mut buf = iter.collect::<Result<Vec<_>, Error>>()?;
        buf.sort();
        Ok(buf)
    }
}

#[cfg(test)]
impl<K: Ord, V: Ord, S: Snapshot<K, V> + Sized> SnapshotExt<K, V> for S {}

/// A consistent snapshot of the data that is currently _physically_ in the
/// unsealed bucket of a persistent [crate::indexed::arrangement::Arrangement].
#[derive(Debug)]
pub struct UnsealedSnapshot {
    /// A closed lower bound on the times of contained updates.
    pub ts_lower: Antichain<u64>,
    /// An open upper bound on the times of the contained updates.
    pub ts_upper: Antichain<u64>,
    pub(crate) batches: Vec<PFuture<Arc<BlobUnsealedBatch>>>,
}

impl Snapshot<Vec<u8>, Vec<u8>> for UnsealedSnapshot {
    type Iter = UnsealedSnapshotIter;

    fn into_iters(self, num_iters: NonZeroUsize) -> Vec<Self::Iter> {
        let mut iters = Vec::with_capacity(num_iters.get());
        iters.resize_with(num_iters.get(), || UnsealedSnapshotIter {
            ts_lower: self.ts_lower.clone(),
            ts_upper: self.ts_upper.clone(),
            iter: BatchesIter::default(),
        });
        // TODO: This should probably distribute batches based on size, but for
        // now it's simpler to round-robin them.
        for (i, batch) in self.batches.into_iter().enumerate() {
            let iter_idx = i % num_iters;
            iters[iter_idx].iter.batches.push_back(batch);
        }
        iters
    }
}

/// An [Iterator] representing one part of the data in a [UnsealedSnapshot].
#[derive(Debug)]
pub struct UnsealedSnapshotIter {
    /// A closed lower bound on the times of contained updates.
    ts_lower: Antichain<u64>,
    /// An open upper bound on the times of the contained updates.
    ts_upper: Antichain<u64>,
    iter: BatchesIter<BlobUnsealedBatch>,
}

impl Iterator for UnsealedSnapshotIter {
    type Item = Result<((Vec<u8>, Vec<u8>), u64, i64), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let next = match self.iter.next() {
                Some(x) => x,
                None => return None,
            };
            let (kv, t, d) = match next {
                Ok(x) => x,
                Err(err) => return Some(Err(err)),
            };
            if self.ts_lower.less_equal(&t) && !self.ts_upper.less_equal(&t) {
                return Some(Ok((kv, t, d)));
            }
        }
    }
}

/// A consistent snapshot of the data that is currently _physically_ in the
/// trace bucket of a persistent [crate::indexed::arrangement::Arrangement].
#[derive(Debug)]
pub struct TraceSnapshot {
    /// An open upper bound on the times of contained updates.
    pub ts_upper: Antichain<u64>,
    /// Since frontier of the given updates.
    ///
    /// All updates not at times greater than this frontier must be advanced
    /// to a time that is equivalent to this frontier.
    pub since: Antichain<u64>,
    pub(crate) batches: Vec<PFuture<Arc<BlobTraceBatchPart>>>,
}

impl Snapshot<Vec<u8>, Vec<u8>> for TraceSnapshot {
    type Iter = TraceSnapshotIter;

    fn into_iters(self, num_iters: NonZeroUsize) -> Vec<Self::Iter> {
        let mut iters = Vec::with_capacity(num_iters.get());
        iters.resize_with(num_iters.get(), || TraceSnapshotIter {
            iter: BatchesIter::default(),
        });
        // TODO: This should probably distribute batches based on size, but for
        // now it's simpler to round-robin them.
        for (i, batch) in self.batches.into_iter().enumerate() {
            let iter_idx = i % num_iters;
            iters[iter_idx].iter.batches.push_back(batch);
        }
        iters
    }
}

/// An [Iterator] representing one part of the data in a [TraceSnapshot].
#[derive(Debug)]
pub struct TraceSnapshotIter {
    iter: BatchesIter<BlobTraceBatchPart>,
}

impl Iterator for TraceSnapshotIter {
    type Item = Result<((Vec<u8>, Vec<u8>), u64, i64), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }
}

/// A consistent snapshot of all data currently stored for an id.
#[derive(Debug)]
pub struct ArrangementSnapshot(
    pub(crate) UnsealedSnapshot,
    pub(crate) TraceSnapshot,
    pub(crate) SeqNo,
    pub(crate) Antichain<u64>,
);

impl ArrangementSnapshot {
    /// Returns the SeqNo at which this snapshot was run.
    ///
    /// All writes assigned a seqno < this are included.
    pub fn seqno(&self) -> SeqNo {
        self.2
    }

    /// Returns the since frontier of this snapshot.
    ///
    /// All updates at times less than this frontier must be forwarded
    /// to some time in this frontier.
    pub fn since(&self) -> Antichain<u64> {
        self.1.since.clone()
    }

    /// A logical upper bound on the times that had been added to the collection
    /// when this snapshot was taken
    pub(crate) fn get_seal(&self) -> Antichain<u64> {
        self.3.clone()
    }
}

impl Snapshot<Vec<u8>, Vec<u8>> for ArrangementSnapshot {
    type Iter = ArrangementSnapshotIter;

    fn into_iters(self, num_iters: NonZeroUsize) -> Vec<ArrangementSnapshotIter> {
        let since = self.since();
        let ArrangementSnapshot(unsealed, trace, _, _) = self;
        let unsealed_iters = unsealed.into_iters(num_iters);
        let trace_iters = trace.into_iters(num_iters);
        // I don't love the non-debug asserts, but it doesn't seem worth it to
        // plumb an error around here.
        assert_eq!(unsealed_iters.len(), num_iters.get());
        assert_eq!(trace_iters.len(), num_iters.get());
        unsealed_iters
            .into_iter()
            .zip(trace_iters.into_iter())
            .map(|(unsealed_iter, trace_iter)| ArrangementSnapshotIter {
                since: since.clone(),
                iter: trace_iter.chain(unsealed_iter),
            })
            .collect()
    }
}

/// An [Iterator] representing one part of the data in an [ArrangementSnapshot].
//
// This intentionally chains trace before unsealed so we get the data in roughly
// increasing timestamp order, but it's unclear if this is in any way important.
#[derive(Debug)]
pub struct ArrangementSnapshotIter {
    since: Antichain<u64>,
    iter: std::iter::Chain<TraceSnapshotIter, UnsealedSnapshotIter>,
}

impl Iterator for ArrangementSnapshotIter {
    type Item = Result<((Vec<u8>, Vec<u8>), u64, i64), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|x| {
            x.map(|(kv, mut ts, diff)| {
                // When reading a snapshot, the contract of since is that all
                // update timestamps will be advanced to it. We do this
                // physically during compaction, but don't have hard guarantees
                // about how long that takes, so we have to account for
                // un-advanced batches on reads.
                ts.advance_by(self.since.borrow());
                (kv, ts, diff)
            })
        })
    }
}

/// A type that [BatchesIter] can iterate over.
trait BatchesIterBatch {
    fn chunks(&self) -> &[ColumnarRecords];
}

impl BatchesIterBatch for BlobUnsealedBatch {
    fn chunks(&self) -> &[ColumnarRecords] {
        &self.updates
    }
}

impl BatchesIterBatch for BlobTraceBatchPart {
    fn chunks(&self) -> &[ColumnarRecords] {
        &self.updates
    }
}

/// An internal helper for iterating over the result of a set of Futures
/// (representing fetches from storage), each of which resolves to something
/// that has a slice of [ColumnarRecords].
//
// This intentionally stores the batches as a VecDeque so we can return the data
// in roughly increasing timestamp order, but it's unclear if this is in any way
// important.
#[derive(Debug)]
struct BatchesIter<B: BatchesIterBatch> {
    record_idx: usize,
    chunk_idx: usize,
    current: Option<Arc<B>>,
    batches: VecDeque<PFuture<Arc<B>>>,
}

impl<B: BatchesIterBatch> Default for BatchesIter<B> {
    fn default() -> Self {
        Self {
            record_idx: Default::default(),
            chunk_idx: Default::default(),
            current: Default::default(),
            batches: Default::default(),
        }
    }
}

impl<B: BatchesIterBatch> Iterator for BatchesIter<B> {
    type Item = Result<((Vec<u8>, Vec<u8>), u64, i64), Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let current = match self.current.as_ref() {
                Some(x) => x,
                None => {
                    let new_current = match self.batches.pop_front() {
                        Some(x) => x,
                        None => return None,
                    };
                    let new_current = match new_current.recv() {
                        Ok(x) => x,
                        Err(err) => return Some(Err(err)),
                    };
                    self.current = Some(new_current);
                    self.record_idx = 0;
                    self.chunk_idx = 0;
                    continue;
                }
            };
            let chunk = match current.chunks().get(self.chunk_idx) {
                Some(x) => x,
                None => {
                    self.current.take();
                    continue;
                }
            };
            let ((k, v), t, d) = match chunk.get(self.record_idx) {
                Some(x) => {
                    self.record_idx += 1;
                    x
                }
                None => {
                    self.record_idx = 0;
                    self.chunk_idx += 1;
                    continue;
                }
            };
            return Some(Ok(((k.to_owned(), v.to_owned()), t, d)));
        }
    }
}
