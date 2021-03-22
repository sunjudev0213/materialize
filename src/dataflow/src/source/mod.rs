// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Types related to the creation of dataflow sources.

use mz_avro::types::Value;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryInto;
use std::fmt::{self, Debug};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use timely::dataflow::{
    channels::pact::{Exchange, ParallelizationContract},
    operators::Capability,
};

use dataflow_types::{Consistency, DataEncoding, ExternalSourceConnector, MzOffset, SourceError};
use expr::{GlobalId, PartitionId, SourceInstanceId};
use lazy_static::lazy_static;
use log::{debug, error, trace};
use prometheus::core::{AtomicI64, AtomicU64};
use prometheus::{
    register_int_counter, register_int_counter_vec, register_int_gauge_vec,
    register_uint_gauge_vec, DeleteOnDropCounter, DeleteOnDropGauge, IntCounter, IntCounterVec,
    IntGaugeVec, UIntGauge, UIntGaugeVec,
};
use repr::{CachedRecord, CachedRecordIter, Timestamp};
use timely::dataflow::channels::pushers::Tee;
use timely::dataflow::operators::generic::OutputHandle;
use timely::dataflow::Scope;
use timely::scheduling::activate::{Activator, SyncActivator};
use timely::Data;
use tokio::sync::mpsc;

use super::source::util::source;
use crate::logging::materialized::{Logger, MaterializedEvent};
use crate::operator::StreamExt;
use crate::server::{TimestampDataUpdate, TimestampDataUpdates};
use crate::source::cache::WorkerCacheData;
use crate::CacheMessage;

mod file;
mod kafka;
mod kinesis;
mod s3;
mod util;

pub mod cache;

use differential_dataflow::Hashable;
pub use file::read_file_task;
pub use file::FileReadStyle;
pub use file::FileSourceInfo;
pub use kafka::KafkaSourceInfo;
pub use kinesis::KinesisSourceInfo;
pub use s3::S3SourceInfo;

// Interval after which the source operator will yield control.
static YIELD_INTERVAL_MS: u128 = 10;

/// Shared configuration information for all source types.
pub struct SourceConfig<'a, G> {
    /// The name to attach to the underlying timely operator.
    pub name: String,
    /// The ID of this instantiation of this source.
    pub id: SourceInstanceId,
    /// The timely scope in which to build the source.
    pub scope: &'a G,
    /// The ID of the worker on which this operator is executing
    pub worker_id: usize,
    /// The total count of workers
    pub worker_count: usize,
    // Timestamping fields.
    /// Data-timestamping updates: information about (timestamp, source offset)
    pub timestamp_histories: TimestampDataUpdates,
    /// A source can use Real-Time consistency timestamping or BYO consistency information.
    pub consistency: Consistency,
    /// Source Type
    /// Timestamp Frequency: frequency at which timestamps should be closed (and capabilities
    /// downgraded)
    pub timestamp_frequency: Duration,
    /// Whether this worker has been chosen to actually receive data.
    pub active: bool,
    /// Data encoding
    pub encoding: DataEncoding,
    /// Channel to send source caching information to cacher thread
    pub caching_tx: Option<mpsc::UnboundedSender<CacheMessage>>,
    /// Timely worker logger for source events
    pub logger: Option<Logger>,
}

#[derive(Clone, Serialize, Debug, Deserialize)]
/// A record produced by a source
pub struct SourceOutput<K, V>
where
    K: Data,
    V: Data,
{
    /// The record's key (or some empty/default value for sources without the concept of key)
    pub key: K,
    /// The record's value
    pub value: V,
    /// The position in the source, if such a concept exists (e.g., Kafka offset, file line number)
    pub position: Option<i64>,
    /// The time the record was created in the upstream systsem, as milliseconds since the epoch
    pub upstream_time_millis: Option<i64>,
}

/// The data that we send from sources to the decode process
#[derive(Debug, Default, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub(crate) struct SourceData {
    /// The actual value
    pub(crate) value: Vec<u8>,
    /// The source's reported position for this record
    ///
    /// e.g. kafka offset or file location
    pub(crate) position: Option<i64>,

    /// The time that the upstream source believes that the message was created
    ///
    /// Currently only applies to Kafka
    pub(crate) upstream_time_millis: Option<i64>,
}

impl<K, V> SourceOutput<K, V>
where
    K: Data,
    V: Data,
{
    /// Build a new SourceOutput
    pub fn new(
        key: K,
        value: V,
        position: Option<i64>,
        upstream_time_millis: Option<i64>,
    ) -> SourceOutput<K, V> {
        SourceOutput {
            key,
            value,
            position,
            upstream_time_millis,
        }
    }
}
impl<K, V> SourceOutput<K, V>
where
    K: Data + Serialize + for<'a> Deserialize<'a> + Send + Sync,
    V: Data + Serialize + for<'a> Deserialize<'a> + Send + Sync,
{
    /// A parallelization contract that hashes by keys.
    ///
    /// It would not be ideal to use this in a case where the keys might be
    /// empty (or for any other reason too often the same),
    /// as that would lead to serial execution of whatever operator
    /// is being "parallelized" by this contract.
    ///
    /// One good reason to use this (and as of this writing, apparently the only reason)
    /// is for operators that need to do some dedpulication logic based on keys:
    /// For example, upserts, or Debezium dedpulication based on MySQL binlog coordinates.
    pub fn key_contract() -> impl ParallelizationContract<Timestamp, Self>
    where
        K: Hashable<Output = u64>,
    {
        Exchange::new(|x: &Self| x.key.hashed())
    }
    /// A parallelization contract that hashes by positions (if available)
    /// and otherwise falls back to hashing by value. Values can be just as
    /// skewed as keys, whereas positions are generally known to be unique or
    /// close to unique in a source. For example, Kafka offsets are unique per-partition.
    /// Most decode logic should use this instead of `key_contract`.
    pub fn position_value_contract() -> impl ParallelizationContract<Timestamp, Self>
    where
        V: Hashable<Output = u64>,
    {
        Exchange::new(|x: &Self| {
            if let Some(position) = x.position {
                position.hashed()
            } else {
                x.value.hashed()
            }
        })
    }
}

/// A `SourceToken` manages interest in a source.
///
/// When the `SourceToken` is dropped the associated source will be stopped.
pub struct SourceToken {
    capability: Rc<RefCell<Option<Capability<Timestamp>>>>,
    activator: Activator,
}

impl SourceToken {
    /// Re-activates the associated timely source operator.
    pub fn activate(&self) {
        self.activator.activate();
    }
}

impl Drop for SourceToken {
    fn drop(&mut self) {
        *self.capability.borrow_mut() = None;
        self.activator.activate();
    }
}

/// The status of a source.
pub enum SourceStatus {
    /// The source is still alive.
    Alive,
    /// The source is complete.
    Done,
}

// Global Prometheus metrics
lazy_static! {
    static ref BYTES_READ_COUNTER: IntCounter =
        register_int_counter!("mz_bytes_read_total", "Count of bytes read from sources").unwrap();
}

/// Creates a specific source, parameterised by 'Out'. Out denotes the encoding
/// of the ingested data (Vec<u8> or Value).
pub(crate) trait SourceConstructor<Out> {
    /// Constructor for source creation
    #[allow(clippy::too_many_arguments)]
    fn new(
        source_name: String,
        source_id: SourceInstanceId,
        active: bool,
        worker_id: usize,
        consumer_activator: SyncActivator,
        connector: ExternalSourceConnector,
        consistency_info: &mut ConsistencyInfo,
        encoding: DataEncoding,
    ) -> Result<Self, anyhow::Error>
    where
        Self: Sized + SourceInfo<Out>;
}

/// Types that implement this trait expose a length function
pub trait MaybeLength {
    /// Returns the size of the object
    fn len(&self) -> Option<usize>;
    /// Returns true if the object is empty
    fn is_empty(&self) -> bool;
}

impl MaybeLength for Vec<u8> {
    fn len(&self) -> Option<usize> {
        Some(self.len())
    }

    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

impl MaybeLength for Value {
    // Not possible to compute a size in bytes without recursively traversing the entire tree.
    fn len(&self) -> Option<usize> {
        None
    }

    fn is_empty(&self) -> bool {
        false
    }
}

/// Each source must implement this trait. Sources will then get created as part of the
/// [`create_source`] function.
pub(crate) trait SourceInfo<Out> {
    /// Ensures that the partition `pid` exists for this source. Once this function has been
    /// called, the source should be able to receive messages from this partition
    fn add_partition(&mut self, _pid: PartitionId) {
        panic!("add partiton not supported for source!");
    }

    /// Returns the next message read from the source
    fn get_next_message(&mut self) -> Result<NextMessage<Out>, anyhow::Error>;
}

/// This trait defines the interface to cache incoming messages.
///
/// Every output message type must implement this trait.
pub trait SourceCache {
    /// Cache a message
    fn cache_message(
        message: &SourceMessage<Self>,
        source_id: SourceInstanceId,
        caching_tx: &mut Option<mpsc::UnboundedSender<CacheMessage>>,
        timestamp: Timestamp,
        offset: Option<MzOffset>,
    ) where
        Self: Sized;

    /// Read back data from a previously cached file.
    /// Reads messages back from files in offset order, and returns None when there is
    /// no more data left to process
    /// TODO(rkhaitan): clean this up to return a proper type and potentially a iterator.
    fn read_file(path: PathBuf) -> Vec<SourceMessage<Self>>
    where
        Self: Sized;
}

impl SourceCache for Vec<u8> {
    fn cache_message(
        message: &SourceMessage<Vec<u8>>,
        source_id: SourceInstanceId,
        caching_tx: &mut Option<mpsc::UnboundedSender<CacheMessage>>,
        timestamp: Timestamp,
        predecessor: Option<MzOffset>,
    ) {
        // Send this record to be cached
        if let Some(caching_tx) = caching_tx {
            let partition_id = match message.partition {
                PartitionId::Kafka(p) => p,
                _ => unreachable!(),
            };

            // TODO(rkhaitan): let's experiment with wrapping these in a
            // Arc so we don't have to clone.
            let key = message.key.clone().unwrap_or_default();
            let value = message.payload.clone().unwrap_or_default();

            let cache_data = CacheMessage::Data(WorkerCacheData {
                source_id: source_id.source_id,
                partition_id,
                record: CachedRecord {
                    predecessor: predecessor.map(|p| p.offset),
                    offset: message.offset.offset,
                    timestamp,
                    key,
                    value,
                },
            });

            // TODO(benesch): the lack of backpressure here can result in
            // unbounded memory usage.
            caching_tx
                .send(cache_data)
                .expect("caching receiver should never drop first");
        }
    }

    fn read_file(path: PathBuf) -> Vec<SourceMessage<Vec<u8>>> {
        debug!("reading cached data from {}", path.display());
        let data = ::std::fs::read(&path).unwrap_or_else(|e| {
            error!("failed to read source cache file {}: {}", path.display(), e);
            vec![]
        });

        let partition =
            crate::source::cache::cached_file_partition(&path).expect("partition known to exist");
        CachedRecordIter::new(data)
            .map(move |r| SourceMessage {
                key: Some(r.key),
                payload: Some(r.value),
                offset: MzOffset { offset: r.offset },
                upstream_time_millis: None,
                partition: partition.clone(),
            })
            .collect()
    }
}

impl SourceCache for mz_avro::types::Value {
    fn cache_message(
        _message: &SourceMessage<Self>,
        _source_id: SourceInstanceId,
        _caching_tx: &mut Option<mpsc::UnboundedSender<CacheMessage>>,
        _timestamp: Timestamp,
        _offset: Option<MzOffset>,
    ) {
        // Just no-op for OCF sources
        trace!("source caching is not supported for Avro OCF sources");
    }

    fn read_file(_path: PathBuf) -> Vec<SourceMessage<Self>> {
        panic!("source caching is not supported for Avro OCF sources");
    }
}

#[derive(Debug)]
pub(crate) enum NextMessage<Out> {
    Ready(SourceMessage<Out>),
    Pending,
    TransientDelay,
    Finished,
}

/// Source-agnostic wrapper for messages. Each source must implement a
/// conversion to Message.
pub struct SourceMessage<Out> {
    /// Partition from which this message originates
    pub partition: PartitionId,
    /// Materialize offset of the message (1-indexed)
    pub offset: MzOffset,
    /// The time that an external system first observed the message
    ///
    /// Milliseconds since the unix epoch
    pub upstream_time_millis: Option<i64>,
    /// Optional key
    pub key: Option<Vec<u8>>,
    /// Optional payload
    pub payload: Option<Out>,
}

impl<Out> fmt::Debug for SourceMessage<Out> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceMessage")
            .field("partition", &self.partition)
            .field("offset", &self.offset)
            .field("upstream_time_millis", &self.upstream_time_millis)
            .field("key[present]", &self.key.is_some())
            .field("payload[present]", &self.payload.is_some())
            .finish()
    }
}

/// Consistency information. Each partition contains information about
/// 1) the last closed timestamp for this partition
/// 2) the last processed offset
#[derive(Copy, Clone)]
pub struct ConsInfo {
    /// the last closed timestamp for this partition
    pub ts: Timestamp,
    /// the last processed offset
    pub offset: MzOffset,
}

/// Contains all necessary information that relates to consistency and timestamping.
/// This information is (and should remain) source independent. This covers consistency
/// information for sources that follow RT consistency and BYO consistency.
pub struct ConsistencyInfo {
    /// Last closed timestamp
    last_closed_ts: u64,
    /// Time since last capability downgrade
    time_since_downgrade: Instant,
    /// Frequency at which we should downgrade capability (in milliseconds)
    downgrade_capability_frequency: u64,
    /// Per partition (a partition ID in Kafka is an i32), keep track of the last closed offset
    /// and the last closed timestamp.
    /// Note that we only keep track of partitions this worker is responsible for in this
    /// hashmap.
    pub partition_metadata: HashMap<PartitionId, ConsInfo>,
    /// Optional: Materialize Offset from which source should start reading (default is 0)
    start_offsets: HashMap<PartitionId, MzOffset>,
    /// Source Type (Real-time or BYO)
    source_type: Consistency,
    /// Per-source Prometheus metrics.
    source_metrics: SourceMetrics,
    /// source global id
    source_global_id: GlobalId,
    /// True if this worker is the active reader
    active: bool,
    /// id of worker
    worker_id: usize,
    /// total number of workers
    worker_count: usize,
}

impl ConsistencyInfo {
    fn new(
        active: bool,
        source_name: String,
        source_id: SourceInstanceId,
        worker_id: usize,
        worker_count: usize,
        consistency: Consistency,
        timestamp_frequency: Duration,
        connector: &ExternalSourceConnector,
        logger: Option<Logger>,
    ) -> ConsistencyInfo {
        let start_offsets = match connector {
            ExternalSourceConnector::Kafka(kc) => {
                let mut start_offsets = HashMap::new();

                for (partition, offset) in kc.start_offsets.iter() {
                    start_offsets
                        .insert(PartitionId::Kafka(*partition), MzOffset { offset: *offset });
                }

                start_offsets
            }
            _ => HashMap::new(),
        };

        ConsistencyInfo {
            last_closed_ts: 0,
            // Safe conversion: statement.rs checks that value specified fits in u64
            downgrade_capability_frequency: timestamp_frequency.as_millis().try_into().unwrap(),
            partition_metadata: HashMap::new(),
            start_offsets,
            source_type: consistency,
            source_metrics: SourceMetrics::new(
                &source_name,
                source_id,
                &worker_id.to_string(),
                logger,
            ),
            // we have never downgraded, so make sure the initial value is outside of our frequency
            time_since_downgrade: Instant::now() - timestamp_frequency - Duration::from_secs(1),
            source_global_id: source_id.source_id,
            active,
            worker_id,
            worker_count,
        }
    }

    /// Returns true if this worker is responsible for handling this partition
    fn responsible_for(&self, pid: &PartitionId) -> bool {
        match pid {
            PartitionId::File | PartitionId::S3 | PartitionId::Kinesis => self.active,
            PartitionId::Kafka(p) => {
                // We want to distribute partitions across workers evenly, such that
                // - different partitions for the same source are uniformly distributed across workers
                // - the same partition id across different sources are uniformly distributed across workers
                // - the same partition id across different instances of the same source is sent to
                //   the same worker.
                // We achieve this by taking a hash of the `source_id` (not the source instance id) and using
                // that to offset distributing partitions round robin across workers.

                // We keep only 32 bits of randomness from `hashed` to prevent 64 bit
                // overflow.
                let hash = (self.source_global_id.hashed() >> 32) + *p as u64;
                (hash % self.worker_count as u64) == self.worker_id as u64
            }
        }
    }

    /// Returns true if we currently know of particular partition. We know (and have updated the
    /// metadata for this partition) if there is an entry for it
    pub fn knows_of(&self, pid: &PartitionId) -> bool {
        self.partition_metadata.contains_key(pid)
    }

    /// Updates the underlying partition metadata structure to include the current partition.
    /// New partitions must always be added with a minimum closed offset of (last_closed_ts)
    /// They are guaranteed to only receive timestamp update greater than last_closed_ts (this
    /// is enforced in `coord::timestamp::is_ts_valid`.
    pub fn update_partition_metadata(&mut self, pid: &PartitionId) {
        let cons_info = ConsInfo {
            offset: self.get_partition_start_offset(&pid),
            ts: self.last_closed_ts,
        };
        self.partition_metadata.insert(pid.clone(), cons_info);
    }

    fn get_partition_start_offset(&self, pid: &PartitionId) -> MzOffset {
        self.start_offsets
            .get(pid)
            .unwrap_or(&MzOffset { offset: 0 })
            .clone()
    }

    /// Generates a timestamp that is guaranteed to be monotonically increasing.
    /// This may require multiple calls to the underlying now() system method, which is not
    /// guaranteed to increase monotonically
    fn generate_next_timestamp(&mut self) -> Option<u64> {
        let mut new_ts = 0;
        while new_ts <= self.last_closed_ts {
            let start = SystemTime::now();
            new_ts = start
                .duration_since(UNIX_EPOCH)
                .expect("Time went backwards")
                .as_millis() as u64;
            if new_ts < self.last_closed_ts && self.last_closed_ts - new_ts > 1000 {
                // If someone resets their system clock to be too far in the past, this could cause
                // Materialize to block and not give control to other operators. We thus give up
                // on timestamping this message
                error!("The new timestamp is more than 1 second behind the last assigned timestamp. To \
                avoid unnecessary blocking, Materialize will not attempt to downgrade the capability. Please \
                consider resetting your system time.");
                return None;
            }
        }
        assert!(new_ts > self.last_closed_ts);
        // Round new_ts to the next greatest self.downgrade_capability_frequency increment.
        // This is to guarantee that different workers downgrade (without coordination) to the
        // "same" next time
        new_ts = new_ts
            + (self.downgrade_capability_frequency
                - (new_ts % self.downgrade_capability_frequency));
        self.last_closed_ts = new_ts;
        Some(self.last_closed_ts)
    }

    /// Timestamp history map is of format [pid1: (p_ct, ts1, offset1), (p_ct, ts2, offset2), pid2: (p_ct, ts1, offset)...].
    /// For a given partition pid, messages in interval [0,offset1] get assigned ts1, all messages in interval [offset1+1,offset2]
    /// get assigned ts2, etc.
    /// When receive message with offset1, it is safe to downgrade the capability to the next
    /// timestamp, which is either
    /// 1) the timestamp associated with the next highest offset if it exists
    /// 2) max(timestamp, offset1) + 1. The timestamp_history map can contain multiple timestamps for
    /// the same offset. We pick the greatest one + 1
    /// (the next message we generate will necessarily have timestamp timestamp + 1)
    ///
    /// This method assumes that timestamps are inserted in increasing order in the hashmap
    /// (even across partitions). This means that once we see a timestamp with ts x, no entry with
    /// ts (x-1) will ever be inserted. Entries with timestamp x might still be inserted in different
    /// partitions. This is guaranteed by the `coord::timestamp::is_ts_valid` method.
    ///
    fn downgrade_capability<Out: Send + Clone + 'static>(
        &mut self,
        id: &SourceInstanceId,
        cap: &mut Capability<Timestamp>,
        source: &mut dyn SourceInfo<Out>,
        timestamp_histories: &TimestampDataUpdates,
    ) {
        let mut min_closed_timestamp_unowned_partitions: Option<Timestamp> = None;

        if let Consistency::BringYourOwn(_) = self.source_type {
            // Determine which timestamps have been closed. A timestamp is closed once we have processed
            // all messages that we are going to process for this timestamp across all partitions that the
            // worker knows about (i.e. the ones the worker has been assigned to read from).
            // In practice, the following happens:
            // Per partition, we iterate over the data structure to remove (ts,offset) mappings for which
            // we have seen all records <= offset. We keep track of the last "closed" timestamp in that partition
            // in next_partition_ts
            if let Some(entries) = timestamp_histories.borrow().get(&id.source_id) {
                match entries {
                    TimestampDataUpdate::BringYourOwn(entries) => {
                        // Iterate over each partition that we know about
                        for (pid, entries) in entries {
                            if !self.knows_of(pid) && self.responsible_for(pid) {
                                source.add_partition(pid.clone());
                                self.update_partition_metadata(pid);
                                self.source_metrics.add_partition(pid);
                            }

                            if !self.partition_metadata.contains_key(pid) {
                                // We need to grab the min closed (max) timestamp from partitions we
                                // don't own.

                                if let Some((timestamp, _)) = entries.back() {
                                    min_closed_timestamp_unowned_partitions =
                                        match min_closed_timestamp_unowned_partitions.as_mut() {
                                            Some(min) => Some(std::cmp::min(*timestamp, *min)),
                                            None => Some(*timestamp),
                                        };
                                }

                                continue;
                            }

                            let existing_ts = self.partition_metadata.get(pid).unwrap().ts;
                            // Check whether timestamps can be closed on this partition

                            // TODO(rkhaitan): this code performs a linear scan through the full
                            // set of timestamp bindings to find a match every time. This is
                            // clearly suboptimal and can be improved upon with a better
                            // API for sources to interact with timestamp binding information.
                            for (ts, offset) in entries {
                                if existing_ts >= *ts {
                                    //skip old records
                                    continue;
                                }
                                let partition_start_offset = self.get_partition_start_offset(pid);
                                assert!(
                                    *offset >= partition_start_offset,
                                    "Internal error! Timestamping offset went below start: {} < {}. Materialize will now crash.",
                                    offset, partition_start_offset
                                );

                                assert!(
                                    *ts > 0,
                                    "Internal error! Received a zero-timestamp. Materialize will crash now."
                                );

                                if let Some(pmetrics) =
                                    self.source_metrics.partition_metrics.get_mut(pid)
                                {
                                    pmetrics
                                        .closed_ts
                                        .set(self.partition_metadata.get(pid).unwrap().ts);
                                }

                                let cons_info = self.partition_metadata.get_mut(pid).unwrap();
                                if cons_info.offset >= *offset {
                                    cons_info.ts = *ts;
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    _ => panic!("Unexpected timestamp message format. Expected BYO update."),
                }
            }

            //  Next, we determine the maximum timestamp that is fully closed. This corresponds to the minimum of
            //  * closed timestamps across all partitions we own
            //  * maximum bound timestamps across all partitions we don't own.
            let min_closed_timestamp_owned_partitions = self
                .partition_metadata
                .iter()
                .map(|(_, cons_info)| cons_info.ts)
                .min();

            let min = match (
                min_closed_timestamp_owned_partitions,
                min_closed_timestamp_unowned_partitions,
            ) {
                (None, None) => 0,
                (Some(owned), None) => owned,
                (None, Some(unowned)) => unowned,
                (Some(owned), Some(unowned)) => std::cmp::min(owned, unowned),
            };

            // Downgrade capability to new minimum open timestamp (which corresponds to min + 1).
            if min > self.last_closed_ts {
                self.source_metrics.capability.set(min);
                cap.downgrade(&(&min + 1));
                self.last_closed_ts = min;
            }
        } else {
            // This a RT source. It is always possible to close the timestamp and downgrade the
            // capability
            if let Some(entries) = timestamp_histories.borrow().get(&id.source_id) {
                match entries {
                    TimestampDataUpdate::RealTime(partitions) => {
                        for pid in partitions {
                            if !self.knows_of(pid) && self.responsible_for(pid) {
                                source.add_partition(pid.clone());
                                self.update_partition_metadata(pid);
                                self.source_metrics.add_partition(pid);
                            }
                        }
                    }
                    _ => panic!("Unexpected timestamp message. Expected RT update."),
                }
            }

            if self.time_since_downgrade.elapsed().as_millis()
                > self.downgrade_capability_frequency.try_into().unwrap()
            {
                let ts = self.generate_next_timestamp();
                if let Some(ts) = ts {
                    self.source_metrics.capability.set(ts);
                    cap.downgrade(&(&ts + 1));
                }
                self.time_since_downgrade = Instant::now();
            }
        }
    }

    /// For a given offset, returns an option type returning the matching timestamp or None
    /// if no timestamp can be assigned.
    ///
    /// The timestamp history contains a sequence of
    /// (partition_count, timestamp, offset) tuples. A message with offset x will be assigned the first timestamp
    /// for which offset>=x.
    fn find_matching_timestamp(
        &self,
        id: &SourceInstanceId,
        partition: &PartitionId,
        offset: MzOffset,
        timestamp_histories: &TimestampDataUpdates,
    ) -> Option<Timestamp> {
        if let Consistency::RealTime = self.source_type {
            // Simply assign to this message the next timestamp that is not closed
            Some(self.find_matching_rt_timestamp())
        } else {
            // The source is a BYO source. Must check the list of timestamp updates for the given partition
            match timestamp_histories.borrow().get(&id.source_id) {
                None => None,
                Some(TimestampDataUpdate::BringYourOwn(entries)) => match entries.get(partition) {
                    Some(entries) => {
                        for (ts, max_offset) in entries {
                            if offset <= *max_offset {
                                return Some(ts.clone());
                            }
                        }
                        None
                    }
                    None => None,
                },
                _ => panic!("Unexpected entry format in TimestampDataUpdates for BYO source"),
            }
        }
    }

    /// For a given record from a RT source, find the timestamp that is not closed
    fn find_matching_rt_timestamp(&self) -> Timestamp {
        self.last_closed_ts + 1
    }
}

/// Source-specific Prometheus metrics
pub struct SourceMetrics {
    /// Number of times an operator gets scheduled
    operator_scheduled_counter: IntCounter,
    /// Value of the capability associated with this source
    capability: UIntGauge,
    /// Per-partition Prometheus metrics.
    pub partition_metrics: HashMap<PartitionId, PartitionMetrics>,
    logger: Option<Logger>,
    source_name: String,
    source_id: SourceInstanceId,
}

impl SourceMetrics {
    /// Initialises source metrics for a given (source_id, worker_id)
    pub fn new(
        source_name: &str,
        source_id: SourceInstanceId,
        worker_id: &str,
        logger: Option<Logger>,
    ) -> SourceMetrics {
        let source_id_string = source_id.to_string();
        lazy_static! {
            static ref OPERATOR_SCHEDULED_COUNTER: IntCounterVec = register_int_counter_vec!(
                "mz_operator_scheduled_total",
                "The number of times the kafka client got invoked for this source",
                &["topic", "source_id", "worker_id"]
            )
            .unwrap();
            static ref CAPABILITY: UIntGaugeVec = register_uint_gauge_vec!(
                "mz_capability",
                "The current capability for this dataflow. This corresponds to min(mz_partition_closed_ts)",
                &["topic", "source_id", "worker_id"]
            )
            .unwrap();
        }
        let labels = &[source_name, &source_id_string, worker_id];
        SourceMetrics {
            operator_scheduled_counter: OPERATOR_SCHEDULED_COUNTER.with_label_values(labels),
            capability: CAPABILITY.with_label_values(labels),
            partition_metrics: Default::default(),
            logger,
            source_name: source_name.to_string(),
            source_id,
        }
    }

    /// Add metrics for `partition_id`
    pub fn add_partition(&mut self, partition_id: &PartitionId) {
        if self.partition_metrics.contains_key(partition_id) {
            error!(
                "incorrectly adding a duplicate partition metric in source: {} partition: {}",
                self.source_id, partition_id
            );
            return;
        }

        let metric = PartitionMetrics::new(&self.source_name, self.source_id, partition_id);
        self.partition_metrics.insert(partition_id.clone(), metric);
    }

    /// Log updates to which offsets / timestamps read up to.
    pub fn record_partition_offsets(&mut self, offsets: HashMap<PartitionId, (MzOffset, u64)>) {
        if self.logger.is_none() {
            return;
        }

        for (partition, (offset, timestamp)) in offsets {
            if let Some(metric) = self.partition_metrics.get_mut(&partition) {
                metric.record_offset(
                    &mut self.logger.as_mut().unwrap(),
                    &self.source_name,
                    self.source_id,
                    &partition,
                    offset.offset,
                    timestamp as i64,
                );
            }
        }
    }
}

impl Drop for SourceMetrics {
    fn drop(&mut self) {
        // retract our partition from logging
        if let Some(logger) = self.logger.as_mut() {
            for (partition, metric) in self.partition_metrics.iter() {
                logger.log(MaterializedEvent::SourceInfo {
                    source_name: self.source_name.clone(),
                    source_id: self.source_id,
                    partition_id: partition.to_string(),
                    offset: -metric.last_offset,
                    timestamp: -metric.last_timestamp,
                });
            }
        }
    }
}

/// Partition-specific metrics, recorded to both Prometheus and a system table
pub struct PartitionMetrics {
    /// Highest offset that has been received by the source and timestamped
    offset_ingested: DeleteOnDropGauge<'static, AtomicI64>,
    /// Highest offset that has been received by the source
    offset_received: DeleteOnDropGauge<'static, AtomicI64>,
    /// Value of the highest timestamp that is closed (for which all messages have been ingested)
    closed_ts: DeleteOnDropGauge<'static, AtomicU64>,
    /// Total number of messages that have been received by the source and timestamped
    messages_ingested: DeleteOnDropCounter<'static, AtomicI64>,
    last_offset: i64,
    last_timestamp: i64,
}

impl PartitionMetrics {
    /// Record the latest offset ingested high-water mark
    fn record_offset(
        &mut self,
        logger: &mut Logger,
        source_name: &str,
        source_id: SourceInstanceId,
        partition_id: &PartitionId,
        offset: i64,
        timestamp: i64,
    ) {
        logger.log(MaterializedEvent::SourceInfo {
            source_name: source_name.to_string(),
            source_id,
            partition_id: partition_id.to_string(),
            offset: offset - self.last_offset,
            timestamp: timestamp - self.last_timestamp,
        });
        self.last_offset = offset;
        self.last_timestamp = timestamp;
    }

    /// Initialises partition metrics for a given (source_id, partition_id)
    pub fn new(
        source_name: &str,
        source_id: SourceInstanceId,
        partition_id: &PartitionId,
    ) -> PartitionMetrics {
        const LABELS: &[&str] = &["topic", "source_id", "source_instance", "partition_id"];
        lazy_static! {
            static ref OFFSET_INGESTED: IntGaugeVec = register_int_gauge_vec!(
                "mz_partition_offset_ingested",
                "The most recent offset that we have ingested into a dataflow. This correspond to \
                data that we have 1)ingested 2) assigned a timestamp",
                LABELS
            )
            .unwrap();
            static ref OFFSET_RECEIVED: IntGaugeVec = register_int_gauge_vec!(
                "mz_partition_offset_received",
                "The most recent offset that we have been received by this source.",
                LABELS
            )
            .unwrap();
            static ref CLOSED_TS: UIntGaugeVec = register_uint_gauge_vec!(
                "mz_partition_closed_ts",
                "The highest closed timestamp for each partition in this dataflow",
                LABELS
            )
            .unwrap();
            static ref MESSAGES_INGESTED: IntCounterVec = register_int_counter_vec!(
                "mz_messages_ingested",
                "The number of messages ingested per partition.",
                LABELS
            )
            .unwrap();
        }
        let labels = &[
            source_name,
            &source_id.source_id.to_string(),
            &source_id.dataflow_id.to_string(),
            &partition_id.to_string(),
        ];
        PartitionMetrics {
            offset_ingested: DeleteOnDropGauge::new_with_error_handler(
                OFFSET_INGESTED.with_label_values(labels),
                &OFFSET_INGESTED,
                |e, v| log::debug!("unable to delete metric {}: {}", v.fq_name(), e),
            ),
            offset_received: DeleteOnDropGauge::new_with_error_handler(
                OFFSET_RECEIVED.with_label_values(labels),
                &OFFSET_RECEIVED,
                |e, v| log::debug!("unable to delete metric {}: {}", v.fq_name(), e),
            ),
            closed_ts: DeleteOnDropGauge::new_with_error_handler(
                CLOSED_TS.with_label_values(labels),
                &CLOSED_TS,
                |e, v| log::debug!("unable to delete metric {}: {}", v.fq_name(), e),
            ),
            messages_ingested: DeleteOnDropCounter::new_with_error_handler(
                MESSAGES_INGESTED.with_label_values(labels),
                &MESSAGES_INGESTED,
                |e, v| log::debug!("unable to delete metric {}: {}", v.fq_name(), e),
            ),
            last_offset: 0,
            last_timestamp: 0,
        }
    }
}

enum MessageProcessing {
    Stopped,
    Active,
    Yielded,
    YieldedWithDelay,
}

/// Creates a source dataflow operator. The type of ExternalSourceConnector determines the
/// type of source that should be created
pub(crate) fn create_source<G, S: 'static, Out>(
    config: SourceConfig<G>,
    source_connector: ExternalSourceConnector,
) -> (
    (
        timely::dataflow::Stream<G, SourceOutput<Vec<u8>, Out>>,
        timely::dataflow::Stream<G, SourceError>,
    ),
    Option<SourceToken>,
)
where
    G: Scope<Timestamp = Timestamp>,
    S: SourceInfo<Out> + SourceConstructor<Out>,
    Out: Debug + Clone + Send + Default + MaybeLength + SourceCache + 'static,
{
    let SourceConfig {
        name,
        id,
        scope,
        timestamp_histories,
        worker_id,
        worker_count,
        consistency,
        timestamp_frequency,
        active,
        encoding,
        mut caching_tx,
        logger,
        ..
    } = config;
    let (stream, capability) = source(scope, name.clone(), move |info| {
        // Create activator for source
        let activator = scope.activator_for(&info.address[..]);

        // Create control plane information (Consistency-related information)
        let mut consistency_info = ConsistencyInfo::new(
            active,
            name.clone(),
            id,
            worker_id,
            worker_count,
            consistency,
            timestamp_frequency,
            &source_connector,
            logger.clone(),
        );

        // Create source information (this function is specific to a specific
        // source
        let mut source_info: Result<S, anyhow::Error> = SourceConstructor::<Out>::new(
            name.clone(),
            id,
            active,
            worker_id,
            scope.sync_activator_for(&info.address[..]),
            source_connector.clone(),
            &mut consistency_info,
            encoding,
        );

        let cached_files = dataflow_types::cached_files(&source_connector);
        let mut cached_files = crate::source::cache::cached_files_for_worker(
            id.source_id,
            cached_files,
            &consistency_info,
        );

        let mut predecessor = None;
        // Stash messages we cannot yet timestamp here.
        let mut buffer = None;

        move |cap, output| {
            // First check that the source was successfully created
            let source_info = match &mut source_info {
                Ok(source_info) => source_info,
                Err(e) => {
                    error!("Failed to create source: {}", e);
                    return SourceStatus::Done;
                }
            };

            if !active {
                return SourceStatus::Done;
            }

            // Downgrade capability (if possible)
            consistency_info.downgrade_capability(&id, cap, source_info, &timestamp_histories);

            if let Some(file) = cached_files.pop() {
                // TODO(rkhaitan) change this to properly re-use old timestamps.
                // Currently this is hard to do because there can be arbitrary delays between
                // different workers being scheduled, and this means that all cached state
                // can potentially get pulled into memory without being able to close timestamps
                // which causes the system to go out of memory.
                // For now, constrain we constrain caching to RT sources, and we re-assign
                // timestamps to cached messages on startup.
                let ts = consistency_info.find_matching_rt_timestamp();
                let ts_cap = cap.delayed(&ts);

                let messages = SourceCache::read_file(file);

                for message in messages {
                    let key = message.key.unwrap_or_default();
                    let out = message.payload.unwrap_or_default();
                    output.session(&ts_cap).give(Ok(SourceOutput::new(
                        key,
                        out,
                        Some(message.offset.offset),
                        message.upstream_time_millis,
                    )));
                }
                // Yield to give downstream operators time to handle this data.
                activator.activate_after(Duration::from_millis(10));
                return SourceStatus::Alive;
            }

            // Bound execution of operator to prevent a single operator from hogging
            // the CPU if there are many messages to process
            let timer = Instant::now();
            // Accumulate updates to BYTES_READ_COUNTER for Prometheus metrics collection
            let mut bytes_read = 0;
            // Accumulate updates to offsets for system table metrics collection
            let mut metric_updates = HashMap::new();

            // Record operator has been scheduled
            consistency_info
                .source_metrics
                .operator_scheduled_counter
                .inc();

            let mut source_state = (SourceStatus::Alive, MessageProcessing::Active);
            while let (_, MessageProcessing::Active) = source_state {
                // If we previously buffered something, try to ingest that first.
                // Otherwise, try to pull a new message from the source.
                source_state = if buffer.is_some() {
                    let message = buffer.take().unwrap();
                    handle_message(
                        message,
                        &mut predecessor,
                        &mut consistency_info,
                        &id,
                        &timestamp_histories,
                        &mut bytes_read,
                        &mut caching_tx,
                        &cap,
                        output,
                        &mut metric_updates,
                        &timer,
                        &mut buffer,
                    )
                } else {
                    match source_info.get_next_message() {
                        Ok(NextMessage::Ready(message)) => handle_message(
                            message,
                            &mut predecessor,
                            &mut consistency_info,
                            &id,
                            &timestamp_histories,
                            &mut bytes_read,
                            &mut caching_tx,
                            &cap,
                            output,
                            &mut metric_updates,
                            &timer,
                            &mut buffer,
                        ),
                        Ok(NextMessage::TransientDelay) => {
                            // There was a temporary hiccup in getting messages, check again asap.
                            (SourceStatus::Alive, MessageProcessing::Yielded)
                        }
                        Ok(NextMessage::Pending) => {
                            // There were no new messages, check again after a delay
                            (SourceStatus::Alive, MessageProcessing::YieldedWithDelay)
                        }
                        Ok(NextMessage::Finished) => {
                            if let Consistency::RealTime = consistency_info.source_type {
                                (SourceStatus::Done, MessageProcessing::Stopped)
                            } else {
                                // The coord drives Doneness decisions for BYO, so we must still return Alive
                                // on EOF
                                (SourceStatus::Alive, MessageProcessing::YieldedWithDelay)
                            }
                        }
                        Err(e) => {
                            output.session(&cap).give(Err(e.to_string()));
                            (SourceStatus::Done, MessageProcessing::Stopped)
                        }
                    }
                }
            }

            BYTES_READ_COUNTER.inc_by(bytes_read as i64);
            consistency_info
                .source_metrics
                .record_partition_offsets(metric_updates);

            // Downgrade capability (if possible) before exiting
            consistency_info.downgrade_capability(&id, cap, source_info, &timestamp_histories);

            let (source_status, processing_status) = source_state;
            // Schedule our next activation
            match processing_status {
                MessageProcessing::Yielded => activator.activate(),
                MessageProcessing::YieldedWithDelay => activator.activate_after(
                    Duration::from_millis(consistency_info.downgrade_capability_frequency),
                ),
                _ => (),
            }

            source_status
        }
    });

    let (ok_stream, err_stream) = stream.map_fallible(|r| r.map_err(SourceError::FileIO));

    if active {
        ((ok_stream, err_stream), Some(capability))
    } else {
        // Immediately drop the capability if worker is not an active reader for source
        ((ok_stream, err_stream), None)
    }
}

/// Take `message` and assign it the appropriate timestamps and push it into the
/// dataflow layer, if possible.
///
/// TODO: This function is a bit of a mess rn but hopefully this function makes the
/// existing mess more obvious and points towards ways to improve it.
fn handle_message<Out>(
    message: SourceMessage<Out>,
    predecessor: &mut Option<MzOffset>,
    consistency_info: &mut ConsistencyInfo,
    id: &SourceInstanceId,
    timestamp_histories: &TimestampDataUpdates,
    bytes_read: &mut usize,
    caching_tx: &mut Option<mpsc::UnboundedSender<CacheMessage>>,
    cap: &Capability<Timestamp>,
    output: &mut OutputHandle<
        Timestamp,
        Result<SourceOutput<Vec<u8>, Out>, String>,
        Tee<Timestamp, Result<SourceOutput<Vec<u8>, Out>, String>>,
    >,
    metric_updates: &mut HashMap<PartitionId, (MzOffset, Timestamp)>,
    timer: &std::time::Instant,
    buffer: &mut Option<SourceMessage<Out>>,
) -> (SourceStatus, MessageProcessing)
where
    Out: Debug + Clone + Send + Default + MaybeLength + SourceCache + 'static,
{
    let partition = message.partition.clone();
    let offset = message.offset;
    let msg_predecessor = *predecessor;
    *predecessor = Some(offset);

    // Update ingestion metrics. Guaranteed to exist as the appropriate
    // entry gets created in SourceConstructor or when a new partition
    // is discovered
    consistency_info
        .source_metrics
        .partition_metrics
        .get_mut(&partition)
        .unwrap_or_else(|| panic!("partition metrics do not exist for partition {}", partition))
        .offset_received
        .set(offset.offset);

    // Determine the timestamp to which we need to assign this message
    let ts =
        consistency_info.find_matching_timestamp(&id, &partition, offset, &timestamp_histories);
    match ts {
        None => {
            // We have not yet decided on a timestamp for this message,
            // we need to buffer the message
            *buffer = Some(message);
            (SourceStatus::Alive, MessageProcessing::Yielded)
        }
        Some(ts) => {
            SourceCache::cache_message(&message, *id, caching_tx, ts, msg_predecessor);
            // Note: empty and null payload/keys are currently
            // treated as the same thing.
            let key = message.key.unwrap_or_default();
            let out = message.payload.unwrap_or_default();
            // Entry for partition_metadata is guaranteed to exist as messages
            // are only processed after we have updated the partition_metadata for a
            // partition and created a partition queue for it.
            consistency_info
                .partition_metadata
                .get_mut(&partition)
                .unwrap()
                .offset = offset;
            *bytes_read += key.len();
            *bytes_read += out.len().unwrap_or(0);
            let ts_cap = cap.delayed(&ts);

            output.session(&ts_cap).give(Ok(SourceOutput::new(
                key,
                out,
                Some(offset.offset),
                message.upstream_time_millis,
            )));

            // Update ingestion metrics
            // Entry is guaranteed to exist as it gets created when we initialise the partition
            let partition_metrics = consistency_info
                .source_metrics
                .partition_metrics
                .get_mut(&partition)
                .unwrap();
            partition_metrics.offset_ingested.set(offset.offset);
            partition_metrics.messages_ingested.inc();

            metric_updates.insert(partition, (offset, ts));

            if timer.elapsed().as_millis() > YIELD_INTERVAL_MS {
                // We didn't drain the entire queue, so indicate that we
                // should run again but yield the CPU to other operators.
                (SourceStatus::Alive, MessageProcessing::Yielded)
            } else {
                (SourceStatus::Alive, MessageProcessing::Active)
            }
        }
    }
}
