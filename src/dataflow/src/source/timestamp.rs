// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Types and methods for managing timestamp assignment and invention in sources

//! External users will interact primarily with instances of a `TimestampBindingRc` object
//! which lets various source instances reading on the same worker coordinate about the
//! underlying `TimestampBindingBox` and give readers that are lagging behind the ability
//! to delay compaction.

//! Besides that, the only other bit of complexity in this code is the `TimestampProposer` object
//! which manages the collaborative invention of timestamps by several source instances all reading
//! from the same worker. The key idea is that since all source readers are assigned to the same
//! worker, only one of them will be reading at a given time, and that reader can either consult
//! the timestamp bindings generated by its peers if it is not the furthest ahead, or if it is
//! the furthest ahead, it can propose a new assingment of `(partition, offset) -> timestamp` that
//! its peers will respect.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use log::{debug, error};
use timely::order::PartialOrder;
use timely::progress::frontier::{Antichain, AntichainRef, MutableAntichain};
use timely::progress::Timestamp as TimelyTimestamp;

use dataflow_types::MzOffset;
use expr::PartitionId;
use ore::now::NowFn;
use repr::Timestamp;

/// This struct holds state for proposed timestamps and
/// proposed bindings from offsets to timestamps.
#[derive(Debug)]
pub struct TimestampProposer {
    /// Working set of proposed offsets to assign to a new timestamp.
    bindings: HashMap<PartitionId, MzOffset>,
    /// Current timestamp we are assigning new data to.
    timestamp: Timestamp,
    /// Last time we updated the timestamp.
    last_update_time: Instant,
    /// Interval at which we are updating the timestamp.
    update_interval: u64,
    now: NowFn,
}

impl TimestampProposer {
    fn new(update_interval: u64, now: NowFn) -> Self {
        let timestamp = now();
        Self {
            bindings: HashMap::new(),
            timestamp,
            last_update_time: Instant::now(),
            update_interval,
            now,
        }
    }

    /// Attempt to propose that `(partition, offset)` be bound to `time`, which means
    /// that all offsets < `offset` get bound to `time` for `partition`.
    ///
    /// This proposal will be ignored if the `time` does not match the current `time`
    /// this proposer is operating at, and also if another reader has already proposed
    /// a binding for an offset greater than `offset`. The only exception here is if
    /// `time` is 0, which is accepted to bootstrap the timestamp proposal.
    fn propose_binding(&mut self, partition: PartitionId, time: Timestamp, offset: MzOffset) {
        if time != self.timestamp && time != 0 {
            error!("Invalid proposed time {} expected {}", time, self.timestamp);
            return;
        }

        if time == 0 && self.bindings.contains_key(&partition) {
            panic!(
                "Incorrectly trying to propose a new binding for partition: {:?}",
                partition
            );
        }

        let current_max = self.bindings.entry(partition).or_insert(offset);
        if offset > *current_max {
            *current_max = offset;
        }
    }

    /// Attempt to mint the currently proposed timestamp bindings, and open up for
    /// proposals on a new timestamp.
    ///
    /// This function needs to be called periodically in order for RT sources to
    /// make progress.
    fn update_timestamp(&mut self) -> Option<(Timestamp, Vec<(PartitionId, MzOffset)>)> {
        if self.last_update_time.elapsed().as_millis() < self.update_interval.into() {
            return None;
        }

        // We need to determine the new timestamp
        let mut new_ts = (self.now)();
        new_ts += self.update_interval - (new_ts % self.update_interval);

        if self.timestamp < new_ts {
            // Now we need to fetch all of the existing bindings
            let bindings: Vec<_> = self.bindings.iter().map(|(p, o)| (p.clone(), *o)).collect();
            let old_timestamp = self.timestamp;

            self.timestamp = new_ts;
            self.last_update_time = Instant::now();
            Some((old_timestamp, bindings))
        } else {
            // We could not determine a suitable new timestamp, and so we
            // cannot finalize any current proposals.
            None
        }
    }

    /// Returns the current upper frontier (timestamp at which all future updates
    /// will occur).
    fn upper(&self) -> Timestamp {
        self.timestamp
    }
}

/// This struct holds per partition timestamp binding state, as a ordered list of bindings (time, offset).
/// Each binding indicates "all offsets < offset must be bound to time", and adjacent pairs of bindings
/// (time1, offset1), (time2, offset2) denote that offsets in [offset1, offset2) should get bound
/// to time1.
#[derive(Debug)]
pub struct PartitionTimestamps {
    id: PartitionId,
    bindings: Vec<(Timestamp, MzOffset)>,
}

impl PartitionTimestamps {
    fn new(id: PartitionId) -> Self {
        Self {
            id,
            bindings: Vec::new(),
        }
    }

    /// Advance all timestamp bindings to the frontier, and then
    /// combine overlapping offset ranges bound to the same timestamp.
    fn compact(&mut self, frontier: AntichainRef<Timestamp>) {
        if self.bindings.is_empty() {
            return;
        }

        // First, let's advance all times not in advance of the frontier to the frontier
        for (time, _) in self.bindings.iter_mut() {
            if !frontier.less_equal(time) {
                *time = *frontier.first().expect("known to exist");
            }
        }

        let mut new_bindings = Vec::with_capacity(self.bindings.len());
        // Now let's only keep the largest binding for each timestamp, ie lets merge bindings
        // of the form (timestamp1, offset1), (timestamp1, offset2), (timestamp1, offset3) =>
        // (timestamp1, offset3)
        for i in 0..(self.bindings.len() - 1) {
            if self.bindings[i].0 != self.bindings[i + 1].0 {
                new_bindings.push(self.bindings[i]);
            }
        }

        // We always keep the last binding around.
        new_bindings.push(*self.bindings.last().expect("known to exist"));
        self.bindings = new_bindings;
    }

    fn add_binding(&mut self, timestamp: Timestamp, offset: MzOffset) {
        let (last_ts, last_offset) = self.bindings.last().unwrap_or(&(0, MzOffset { offset: 0 }));
        assert!(
            offset >= *last_offset,
            "offset should not go backwards, but {} < {}",
            offset,
            last_offset
        );
        assert!(
            timestamp >= *last_ts,
            "timestamp should not go backwards, but {} < {}",
            timestamp,
            last_ts
        );
        self.bindings.push((timestamp, offset));
    }

    /// Gets the minimal timestamp binding (time, offset) for offset (the minimal time
    /// with offset > requested offset.
    ///
    /// Returns None if no such binding exists.
    fn get_binding(&self, offset: MzOffset) -> Option<(Timestamp, MzOffset)> {
        // Rust's binary search is inconvenient so let's roll our own.
        // Maintain the invariants that the offset at lo (entries[lo].1) is always <=
        // than the requested offset, and n is > 1. Check for violations of that before we
        // start the main loop.
        if self.bindings.is_empty() {
            return None;
        }

        let mut n = self.bindings.len();
        let mut lo = 0;
        if self.bindings[lo].1 > offset {
            return Some(self.bindings[lo]);
        }

        while n > 1 {
            let half = n / 2;

            // Advance lo if a later element has an offset less than / equal to the one requested.
            if self.bindings[lo + half].1 <= offset {
                lo += half;
            }

            n -= half;
        }

        if lo + 1 < self.bindings.len() {
            Some(self.bindings[lo + 1])
        } else {
            None
        }
    }

    // Returns the frontier at which all future updates will occur.
    fn upper(&self) -> Option<Timestamp> {
        self.bindings.last().map(|(time, _)| *time + 1)
    }

    fn get_bindings_in_range(
        &self,
        lower: AntichainRef<Timestamp>,
        upper: AntichainRef<Timestamp>,
        bindings: &mut Vec<(PartitionId, Timestamp, MzOffset)>,
    ) {
        for (time, offset) in self.bindings.iter() {
            if lower.less_equal(time) && !upper.less_equal(time) {
                bindings.push((self.id.clone(), *time, *offset));
            }
        }
    }
}

/// This struct holds per-source timestamp state in a way that can be shared across
/// different source instances and allow different source instances to indicate
/// how far they have read up to.
///
/// This type is almost never meant to be used directly, and you probably want to
/// use `TimestampBindingRc` instead.
#[derive(Debug)]
pub struct TimestampBindingBox {
    /// List of timestamp bindings per independent partition. This vector is sorted
    /// by timestamp and offset and each `(time, offset)` entry indicates that offsets <=
    /// `offset` should be assigned `time` as their timestamp. Consecutive entries form
    /// an interval of offsets.
    partitions: HashMap<PartitionId, PartitionTimestamps>,
    /// Indicates the lowest timestamp across all partitions that we retain bindings for.
    /// This frontier can be held back by other entities holding the shared
    /// `TimestampBindingRc`.
    compaction_frontier: MutableAntichain<Timestamp>,
    /// Indicates the lowest timestamp across all partititions and across all workers that has
    /// been durably persisted.
    durability_frontier: Antichain<Timestamp>,
    /// Generates new timestamps for RT sources
    proposer: Option<TimestampProposer>,
    /// Never persist these bindings. This is used for BYO, where the bindings
    /// are stored externally already.
    never_requires_persistence: bool,
}

impl TimestampBindingBox {
    fn new(
        timestamp_update_interval: Option<u64>,
        now: NowFn,
        never_requires_persistence: bool,
    ) -> Self {
        Self {
            partitions: HashMap::new(),
            compaction_frontier: MutableAntichain::new_bottom(TimelyTimestamp::minimum()),
            durability_frontier: Antichain::from_elem(TimelyTimestamp::minimum()),
            proposer: timestamp_update_interval.map(|i| TimestampProposer::new(i, now)),
            never_requires_persistence,
        }
    }

    fn adjust_compaction_frontier(
        &mut self,
        remove: AntichainRef<Timestamp>,
        add: AntichainRef<Timestamp>,
    ) {
        self.compaction_frontier
            .update_iter(remove.iter().map(|t| (*t, -1)));
        self.compaction_frontier
            .update_iter(add.iter().map(|t| (*t, 1)));
    }

    fn set_durability_frontier(&mut self, new_frontier: AntichainRef<Timestamp>) {
        <_ as PartialOrder>::less_equal(&self.durability_frontier.borrow(), &new_frontier);
        self.durability_frontier = new_frontier.to_owned();
    }

    fn compact(&mut self) {
        let frontier = self.compaction_frontier.frontier();

        // Don't compact up to the empty frontier as it would mean there were no
        // timestamp bindings available
        // TODO(rkhaitan): is there a more sensible approach here?
        if frontier.is_empty() {
            return;
        }

        for (_, partition) in self.partitions.iter_mut() {
            partition.compact(frontier);
        }
    }

    fn add_partition(&mut self, partition: PartitionId) {
        if self.partitions.contains_key(&partition) {
            debug!("already inserted partition {:?}, ignoring", partition);
            return;
        }

        self.partitions
            .insert(partition.clone(), PartitionTimestamps::new(partition));
    }

    fn add_binding(
        &mut self,
        partition: PartitionId,
        timestamp: Timestamp,
        offset: MzOffset,
        proposed: bool,
    ) {
        if !self.partitions.contains_key(&partition) {
            panic!("missing partition {:?} when adding binding", partition);
        }

        if proposed {
            if let Some(proposer) = &mut self.proposer {
                proposer.propose_binding(partition, timestamp, offset);
            } else {
                panic!(
                    "attempting to propose a timestamp binding on a source that isn't real-time."
                );
            }
        } else {
            let partition = self.partitions.get_mut(&partition).expect("known to exist");
            partition.add_binding(timestamp, offset);
        }
    }

    fn get_binding(
        &self,
        partition: &PartitionId,
        offset: MzOffset,
    ) -> Option<(Timestamp, Option<MzOffset>)> {
        if !self.partitions.contains_key(partition) {
            return None;
        }

        let partition = self.partitions.get(partition).expect("known to exist");
        if let Some((time, offset)) = partition.get_binding(offset) {
            Some((time, Some(offset)))
        } else if let Some(proposer) = &self.proposer {
            Some((proposer.upper(), None))
        } else {
            None
        }
    }

    fn get_bindings_in_range(
        &self,
        lower: AntichainRef<Timestamp>,
        upper: AntichainRef<Timestamp>,
    ) -> Vec<(PartitionId, Timestamp, MzOffset)> {
        let mut ret = Vec::new();

        for (_, partition) in self.partitions.iter() {
            partition.get_bindings_in_range(lower, upper, &mut ret);
        }

        ret
    }

    fn read_upper(&self, target: &mut Antichain<Timestamp>) {
        target.clear();

        if let Some(proposer) = &self.proposer {
            target.insert(proposer.upper());
        } else {
            for (_, partition) in self.partitions.iter() {
                if let Some(timestamp) = partition.upper() {
                    target.insert(timestamp);
                }
            }
        }

        use timely::progress::Timestamp;
        if target.elements().is_empty() {
            target.insert(Timestamp::minimum());
        }
    }

    fn partitions(&self) -> Vec<PartitionId> {
        self.partitions
            .iter()
            .map(|(pid, _)| pid)
            .cloned()
            .collect()
    }

    fn update_timestamp(&mut self) {
        if let Some(proposer) = &mut self.proposer {
            let result = proposer.update_timestamp();
            if let Some((time, bindings)) = result {
                for (partition, offset) in bindings {
                    self.add_binding(partition, time, offset, false);
                }
            }
        }
    }
}

/// A wrapper that allows multiple source instances to share a `TimestampBindingBox`
/// and hold back its compaction.
#[derive(Debug)]
pub struct TimestampBindingRc {
    wrapper: Rc<RefCell<TimestampBindingBox>>,
    compaction_frontier: Antichain<Timestamp>,
}

impl TimestampBindingRc {
    /// Create a new instance of `TimestampBindingRc`.
    pub fn new(
        timestamp_update_interval: Option<u64>,
        now: NowFn,
        never_requires_persistence: bool,
    ) -> Self {
        let wrapper = Rc::new(RefCell::new(TimestampBindingBox::new(
            timestamp_update_interval,
            now,
            never_requires_persistence,
        )));

        let ret = Self {
            wrapper: wrapper.clone(),
            compaction_frontier: wrapper.borrow().compaction_frontier.frontier().to_owned(),
        };

        ret
    }

    /// Set the compaction frontier to `new_frontier` and compact all timestamp bindings at
    /// timestamps less than the compaction frontier.
    ///
    /// Note that `new_frontier` must be in advance of the current compaction
    /// frontier. The source can be correctly replayed from any `as_of` in advance of
    /// the compaction frontier after this operation.
    pub fn set_compaction_frontier(&mut self, new_frontier: AntichainRef<Timestamp>) {
        assert!(
            self.compaction_frontier.borrow().is_empty()
                || <_ as PartialOrder>::less_equal(
                    &self.compaction_frontier.borrow(),
                    &new_frontier
                )
        );
        self.wrapper
            .borrow_mut()
            .adjust_compaction_frontier(self.compaction_frontier.borrow(), new_frontier);
        self.compaction_frontier = new_frontier.to_owned();
        self.wrapper.borrow_mut().compact();
    }

    /// Sets the durability frontier, aka, the frontier before which all updates can be
    /// replayed across restarts.
    pub fn set_durability_frontier(&self, new_frontier: AntichainRef<Timestamp>) {
        self.wrapper
            .borrow_mut()
            .set_durability_frontier(new_frontier);
    }

    /// Add a new mapping from `(partition, offset) -> timestamp`.
    ///
    /// Note that the `timestamp` has to be greater than the largest previously bound
    /// timestamp for that partition, and `offset` has to be greater than or equal to
    /// the largest previously bound offset for that partition. If `proposed` is true,
    /// the binding is treated as tentative and may be overwritten by other, overlapping
    /// bindings
    pub fn add_binding(
        &self,
        partition: PartitionId,
        timestamp: Timestamp,
        offset: MzOffset,
        proposed: bool,
    ) {
        self.wrapper
            .borrow_mut()
            .add_binding(partition, timestamp, offset, proposed);
    }

    /// Tell timestamping machinery to look out for `partition`
    pub fn add_partition(&self, partition: PartitionId) {
        self.wrapper.borrow_mut().add_partition(partition);
    }

    /// Get the timestamp assignment for `(partition, offset)` if it is known.
    ///
    /// This function returns the timestamp and the maximum offset for which it is
    /// valid.
    pub fn get_binding(
        &self,
        partition: &PartitionId,
        offset: MzOffset,
    ) -> Option<(Timestamp, Option<MzOffset>)> {
        self.wrapper.borrow().get_binding(partition, offset)
    }

    /// Returns the frontier of timestamps that have not been bound to any
    /// incoming data, or in other words, all data has been assigned timestamps
    /// less than some element in the returned frontier.
    ///
    /// All subsequent updates will either be at or in advance of this frontier.
    pub fn read_upper(&self, target: &mut Antichain<Timestamp>) {
        self.wrapper.borrow().read_upper(target)
    }

    /// Returns the list of partitions this source knows about.
    ///
    /// TODO(rkhaitan): this function feels like a hack, both in the API of having
    /// the source instances ask for the list of known partitions and in allocating
    /// a vector to answer that question.
    pub fn partitions(&self) -> Vec<PartitionId> {
        self.wrapper.borrow().partitions()
    }

    /// Instructs RT sources to try and move forward to the next timestamp if
    /// possible
    pub fn update_timestamp(&self) {
        self.wrapper.borrow_mut().update_timestamp()
    }

    /// Return all timestamp bindings at or in advance of lower and not at or in advance of upper
    pub fn get_bindings_in_range(
        &self,
        lower: AntichainRef<Timestamp>,
        upper: AntichainRef<Timestamp>,
    ) -> Vec<(PartitionId, Timestamp, MzOffset)> {
        self.wrapper.borrow().get_bindings_in_range(lower, upper)
    }

    /// Returns the current durability frontier
    pub fn durability_frontier(&self) -> Antichain<Timestamp> {
        self.wrapper.borrow().durability_frontier.clone()
    }

    /// Whether or not these timestamp bindings must be persisted.
    pub fn requires_persistence(&self) -> bool {
        !self.wrapper.borrow().never_requires_persistence
    }
}

impl Clone for TimestampBindingRc {
    fn clone(&self) -> Self {
        // Bump the reference count for the current shared frontier
        let frontier = self
            .wrapper
            .borrow()
            .compaction_frontier
            .frontier()
            .to_owned();
        self.wrapper
            .borrow_mut()
            .adjust_compaction_frontier(Antichain::new().borrow(), frontier.borrow());
        self.wrapper.borrow_mut().compact();

        Self {
            wrapper: self.wrapper.clone(),
            compaction_frontier: frontier,
        }
    }
}

impl Drop for TimestampBindingRc {
    fn drop(&mut self) {
        // Decrement the reference count for the current frontier
        self.wrapper.borrow_mut().adjust_compaction_frontier(
            self.compaction_frontier.borrow(),
            Antichain::new().borrow(),
        );
        self.wrapper.borrow_mut().compact();

        self.compaction_frontier = Antichain::new();
    }
}
