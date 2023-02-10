// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logging dataflows for events generated by clusterd.

use std::any::Any;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use differential_dataflow::collection::AsCollection;
use differential_dataflow::operators::arrange::arrangement::Arrange;
use differential_dataflow::operators::arrange::Arranged;
use differential_dataflow::operators::count::CountTotal;
use differential_dataflow::trace::TraceReader;
use timely::communication::Allocate;
use timely::dataflow::operators::capture::EventLink;
use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;
use timely::dataflow::operators::{Filter, InspectCore};
use timely::dataflow::{Scope, StreamCore};
use timely::logging::WorkerIdentifier;
use timely::Container;
use tracing::error;
use uuid::Uuid;

use mz_expr::{permutation_for_arrangement, MirScalarExpr};
use mz_ore::cast::CastFrom;
use mz_repr::{Datum, DatumVec, GlobalId, Row, Timestamp};
use mz_timely_util::activator::RcActivator;
use mz_timely_util::replay::MzReplay;

use crate::compute_state::ComputeState;
use crate::logging::persist::persist_sink;
use crate::logging::{ComputeLog, LogVariant};
use crate::typedefs::{KeysValsHandle, RowSpine};

/// Type alias for logging of compute events.
pub type Logger = timely::logging_core::Logger<ComputeEvent, WorkerIdentifier>;

/// A logged compute event.
#[derive(Debug, Clone, PartialOrd, PartialEq)]
pub enum ComputeEvent {
    /// Dataflow command, true for create and false for drop.
    Dataflow(GlobalId, bool),
    /// Dataflow depends on a named source of data.
    DataflowDependency {
        /// Globally unique identifier for the dataflow.
        dataflow: GlobalId,
        /// Globally unique identifier for the source on which the dataflow depends.
        source: GlobalId,
    },
    /// Peek command, true for install and false for retire.
    Peek(Peek, bool),
    /// Available frontier information for dataflow exports.
    Frontier {
        id: GlobalId,
        time: Timestamp,
        diff: i8,
    },
    /// Available frontier information for dataflow imports.
    ImportFrontier {
        import_id: GlobalId,
        export_id: GlobalId,
        time: Timestamp,
        diff: i8,
    },
}

/// A logged peek event.
#[derive(
    Debug, Clone, Ord, PartialOrd, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Peek {
    /// The identifier of the view the peek targets.
    id: GlobalId,
    /// The logical timestamp requested.
    time: Timestamp,
    /// The ID of the peek.
    uuid: Uuid,
}

impl Peek {
    /// Create a new peek from its arguments.
    pub fn new(id: GlobalId, time: Timestamp, uuid: Uuid) -> Self {
        Self { id, time, uuid }
    }
}

/// Constructs the logging dataflow for compute logs.
///
/// Params
/// * `worker`: The Timely worker hosting the log analysis dataflow.
/// * `config`: Logging configuration
/// * `compute`: The source to read compute log events from.
/// * `activator`: A handle to acknowledge activations.
///
/// Returns a map from log variant to a tuple of a trace handle and a permutation to reconstruct
/// the original rows.
pub fn construct<A: Allocate>(
    worker: &mut timely::worker::Worker<A>,
    config: &mz_compute_client::logging::LoggingConfig,
    compute_state: &mut ComputeState,
    compute: std::rc::Rc<EventLink<Timestamp, (Duration, WorkerIdentifier, ComputeEvent)>>,
    activator: RcActivator,
) -> BTreeMap<LogVariant, (KeysValsHandle, Rc<dyn Any>)> {
    let interval_ms = std::cmp::max(1, config.interval.as_millis());

    let traces = worker.dataflow_named("Dataflow: compute logging", move |scope| {
        let (mut compute_logs, token) =
            Some(compute).mz_replay(scope, "compute logs", config.interval, activator.clone());

        // If logging is disabled, we still need to install the indexes, but we can leave them
        // empty. We do so by immediately filtering all logs events.
        // TODO(teskje): Remove this once we remove the arranged introspection sources.
        if !config.enable_logging {
            compute_logs = compute_logs.filter(|_| false);
        }

        let mut demux = OperatorBuilder::new("Compute Logging Demux".to_string(), scope.clone());
        use timely::dataflow::channels::pact::Pipeline;
        let mut input = demux.new_input(&compute_logs, Pipeline);
        let (mut dataflow_out, dataflow) = demux.new_output();
        let (mut dependency_out, dependency) = demux.new_output();
        let (mut frontier_out, frontier) = demux.new_output();
        let (mut source_frontier_out, source_frontier) = demux.new_output();
        let (mut frontier_delay_out, frontier_delay) = demux.new_output();
        let (mut peek_out, peek) = demux.new_output();
        let (mut peek_duration_out, peek_duration) = demux.new_output();

        let mut demux_buffer = Vec::new();
        demux.build(move |_capability| {
            let mut active_dataflows = BTreeMap::new();
            let mut peek_stash = BTreeMap::new();
            /// State for tracking dataflow frontier lag
            struct ImportDelayData {
                /// A list of input timestamps that have appeared on the input
                /// frontier, but that the output frontier has not yet advanced beyond,
                /// and the time at which we were informed of their availability
                time_deque: VecDeque<(mz_repr::Timestamp, u128)>,
                /// A histogram of emitted delays (bucket size to (bucket_sum, bucket_count))
                delay_map: BTreeMap<u128, (i128, i32)>,
            }
            let mut dataflow_imports =
                BTreeMap::<(GlobalId, usize), BTreeMap<GlobalId, ImportDelayData>>::new();
            move |_frontiers| {
                let mut dataflow = dataflow_out.activate();
                let mut dependency = dependency_out.activate();
                let mut frontier = frontier_out.activate();
                let mut source_frontier = source_frontier_out.activate();
                let mut frontier_delay = frontier_delay_out.activate();
                let mut peek = peek_out.activate();
                let mut peek_duration = peek_duration_out.activate();

                input.for_each(|time, data| {
                    data.swap(&mut demux_buffer);

                    let mut dataflow_session = dataflow.session(&time);
                    let mut dependency_session = dependency.session(&time);
                    let mut frontier_session = frontier.session(&time);
                    let mut source_frontier_session = source_frontier.session(&time);
                    let mut frontier_delay_session = frontier_delay.session(&time);
                    let mut peek_session = peek.session(&time);
                    let mut peek_duration_session = peek_duration.session(&time);

                    for (time, worker, datum) in demux_buffer.drain(..) {
                        let time_ms = (((time.as_millis() / interval_ms) + 1) * interval_ms)
                            .try_into()
                            .expect("must fit");

                        match datum {
                            ComputeEvent::Dataflow(id, is_create) => {
                                let diff = if is_create { 1 } else { -1 };
                                dataflow_session.give(((id, worker), time_ms, diff));

                                // For now we know that these always happen in
                                // the correct order, but it may be necessary
                                // down the line to have dataflows keep a
                                // reference to their own sources and a logger
                                // that is called on them in a `with_drop` handler
                                if is_create {
                                    active_dataflows.insert((id, worker), vec![]);
                                } else {
                                    let key = &(id, worker);
                                    match active_dataflows.remove(key) {
                                        Some(sources) => {
                                            for (source, worker) in sources {
                                                let n = key.0;
                                                dependency_session.give((
                                                    (n, source, worker),
                                                    time_ms,
                                                    -1,
                                                ));
                                            }
                                        }
                                        None => error!(
                                            "no active dataflow exists at time of drop. \
                                             name={} worker={}",
                                            key.0, worker
                                        ),
                                    }
                                    // Remove import frontier delay logging for this dataflow
                                    if let Some(import_map) = dataflow_imports.remove(key) {
                                        for (import_id, ImportDelayData { delay_map, .. }) in
                                            import_map
                                        {
                                            for (delay_pow, (delay_sum, delay_count)) in delay_map {
                                                frontier_delay_session.give((
                                                    (
                                                        id,
                                                        import_id,
                                                        worker,
                                                        delay_pow,
                                                        delay_sum,
                                                        delay_count,
                                                    ),
                                                    time_ms,
                                                    -1,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            ComputeEvent::DataflowDependency { dataflow, source } => {
                                dependency_session.give(((dataflow, source, worker), time_ms, 1));
                                let key = (dataflow, worker);
                                match active_dataflows.get_mut(&key) {
                                    Some(existing_sources) => {
                                        existing_sources.push((source, worker))
                                    }
                                    None => error!(
                                        "tried to create source for dataflow that doesn't exist: \
                                         dataflow={} source={} worker={}",
                                        key.0, source, worker,
                                    ),
                                }
                            }
                            ComputeEvent::Frontier {
                                id,
                                time: logical,
                                diff,
                            } => {
                                // report dataflow frontier advancement
                                frontier_session.give((
                                    Row::pack_slice(&[
                                        Datum::String(&id.to_string()),
                                        Datum::UInt64(u64::cast_from(worker)),
                                        Datum::MzTimestamp(logical),
                                    ]),
                                    time_ms,
                                    i64::from(diff),
                                ));
                                if diff > 0 {
                                    // check if we have imports associated to this dataflow
                                    // and report frontier advancement delays
                                    let dataflow_key = (id, worker);
                                    if let Some(import_map) =
                                        dataflow_imports.get_mut(&dataflow_key)
                                    {
                                        for (
                                            import_id,
                                            ImportDelayData {
                                                time_deque,
                                                delay_map,
                                            },
                                        ) in import_map
                                        {
                                            while let Some(current_front) = time_deque.pop_front() {
                                                let import_logical = current_front.0;
                                                if logical >= import_logical {
                                                    let elapsed_ns =
                                                        time.as_nanos() - current_front.1;
                                                    let elapsed_pow =
                                                        elapsed_ns.next_power_of_two();
                                                    let elapsed_ns: i128 = elapsed_ns
                                                        .try_into()
                                                        .expect("elapsed_ns too big");
                                                    let (new_delay_sum, new_delay_count) =
                                                        match delay_map.entry(elapsed_pow) {
                                                            Entry::Vacant(v) => v.insert((0, 0)),
                                                            Entry::Occupied(o) => {
                                                                let (
                                                                    old_delay_sum,
                                                                    old_delay_count,
                                                                ) = o.get().clone();
                                                                frontier_delay_session.give((
                                                                    (
                                                                        id,
                                                                        *import_id,
                                                                        worker,
                                                                        elapsed_pow,
                                                                        old_delay_sum,
                                                                        old_delay_count,
                                                                    ),
                                                                    time_ms,
                                                                    -1,
                                                                ));
                                                                o.into_mut()
                                                            }
                                                        };
                                                    *new_delay_sum += elapsed_ns;
                                                    *new_delay_count += 1;
                                                    frontier_delay_session.give((
                                                        (
                                                            id,
                                                            *import_id,
                                                            worker,
                                                            elapsed_pow,
                                                            *new_delay_sum,
                                                            *new_delay_count,
                                                        ),
                                                        time_ms,
                                                        1,
                                                    ));
                                                } else {
                                                    time_deque.push_front(current_front);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            ComputeEvent::ImportFrontier {
                                import_id,
                                export_id,
                                time: logical,
                                diff,
                            } => {
                                // report import frontier advancement
                                source_frontier_session.give((
                                    Row::pack_slice(&[
                                        Datum::String(&export_id.to_string()),
                                        Datum::String(&import_id.to_string()),
                                        Datum::UInt64(u64::cast_from(worker)),
                                        Datum::MzTimestamp(logical),
                                    ]),
                                    time_ms,
                                    i64::from(diff),
                                ));
                                if diff > 0 {
                                    // We should record the import frontier here only if
                                    // there is a corresponding active dataflow. This behavior
                                    // arises because `ImportFrontier` events are generated by a
                                    // dataflow `inspect_container` operator, which may outlive
                                    // the corresponding trace or sink recording in the
                                    // current `ComputeState` until Timely eventually drops it.
                                    let dataflow_key = (export_id, worker);
                                    if let Some(_) = active_dataflows.get(&dataflow_key) {
                                        let import_map = dataflow_imports
                                            .entry(dataflow_key)
                                            .or_insert_with(BTreeMap::new);
                                        let time_entry = import_map.entry(import_id).or_insert(
                                            ImportDelayData {
                                                time_deque: VecDeque::new(),
                                                delay_map: BTreeMap::new(),
                                            },
                                        );
                                        time_entry.time_deque.push_back((logical, time.as_nanos()));
                                    }
                                }
                            }
                            ComputeEvent::Peek(peek, is_install) => {
                                let key = (worker, peek.uuid);
                                if is_install {
                                    peek_session.give(((peek, worker), time_ms, 1));
                                    if peek_stash.contains_key(&key) {
                                        error!(
                                            "peek already registered: \
                                             worker={}, uuid: {}",
                                            worker, key.1,
                                        );
                                    }
                                    peek_stash.insert(key, time.as_nanos());
                                } else {
                                    peek_session.give(((peek, worker), time_ms, -1));
                                    if let Some(start) = peek_stash.remove(&key) {
                                        let elapsed_ns = time.as_nanos() - start;
                                        let elapsed_pow = elapsed_ns.next_power_of_two();
                                        let elapsed_ns: i128 =
                                            elapsed_ns.try_into().expect("elapsed_ns too big");
                                        peek_duration_session.give((
                                            (key.0, elapsed_pow),
                                            time_ms,
                                            (elapsed_ns, 1u64),
                                        ));
                                    } else {
                                        error!(
                                            "peek not yet registered: \
                                             worker={}, uuid: {}",
                                            worker, key.1,
                                        );
                                    }
                                }
                            }
                        }
                    }
                });
            }
        });

        let dataflow_current = dataflow.as_collection().map({
            move |(name, worker)| {
                Row::pack_slice(&[
                    Datum::String(&name.to_string()),
                    Datum::UInt64(u64::cast_from(worker)),
                ])
            }
        });

        let dependency_current = dependency.as_collection().map({
            move |(dataflow, source, worker)| {
                Row::pack_slice(&[
                    Datum::String(&dataflow.to_string()),
                    Datum::String(&source.to_string()),
                    Datum::UInt64(u64::cast_from(worker)),
                ])
            }
        });

        let frontier_current = frontier.as_collection();

        let source_frontier_current = source_frontier.as_collection();

        let frontier_delay = frontier_delay.as_collection().map({
            move |(dataflow, source_id, worker, delay_pow, delay_sum, delay_count)| {
                Row::pack_slice(&[
                    Datum::String(&dataflow.to_string()),
                    Datum::String(&source_id.to_string()),
                    Datum::UInt64(u64::cast_from(worker)),
                    Datum::UInt64(delay_pow.try_into().expect("pow too big")),
                    Datum::Int64(delay_count.into()),
                    // [btv] This is nullable so that we can fill
                    // in `NULL` if it overflows. That would be a
                    // bit far-fetched, but not impossible to
                    // imagine. See discussion
                    // [here](https://github.com/MaterializeInc/materialize/pull/17302#discussion_r1086373740)
                    // for more details, and think about this
                    // again if we ever decide to stabilize it.
                    u64::try_from(delay_sum).ok().into(),
                ])
            }
        });

        let peek_current = peek.as_collection().map({
            move |(peek, worker)| {
                Row::pack_slice(&[
                    Datum::Uuid(peek.uuid),
                    Datum::UInt64(u64::cast_from(worker)),
                    Datum::String(&peek.id.to_string()),
                    Datum::MzTimestamp(peek.time),
                ])
            }
        });

        // Duration statistics derive from the non-rounded event times.
        let peek_duration = peek_duration
            .as_collection()
            .arrange_named::<RowSpine<_, _, _, _>>("Arranged timely peek_duration")
            .count_total_core()
            .map(|((worker, bucket), (sum, count))| {
                Row::pack_slice(&[
                    Datum::UInt64(u64::cast_from(worker)),
                    Datum::UInt64(bucket.try_into().expect("pow too big")),
                    Datum::UInt64(count),
                    // [btv] This is nullable so that we can fill
                    // in `NULL` if it overflows. That would be a
                    // bit far-fetched, but not impossible to
                    // imagine. See discussion
                    // [here](https://github.com/MaterializeInc/materialize/pull/17302#discussion_r1086373740)
                    // for more details, and think about this
                    // again if we ever decide to stabilize it.
                    u64::try_from(sum).ok().into(),
                ])
            });

        let logs = vec![
            (
                LogVariant::Compute(ComputeLog::DataflowCurrent),
                dataflow_current,
            ),
            (
                LogVariant::Compute(ComputeLog::DataflowDependency),
                dependency_current,
            ),
            (
                LogVariant::Compute(ComputeLog::FrontierCurrent),
                frontier_current,
            ),
            (
                LogVariant::Compute(ComputeLog::ImportFrontierCurrent),
                source_frontier_current,
            ),
            (
                LogVariant::Compute(ComputeLog::FrontierDelay),
                frontier_delay,
            ),
            (LogVariant::Compute(ComputeLog::PeekCurrent), peek_current),
            (LogVariant::Compute(ComputeLog::PeekDuration), peek_duration),
        ];

        let mut result = BTreeMap::new();
        for (variant, collection) in logs {
            if config.index_logs.contains_key(&variant) {
                let key = variant.index_by();
                let (_, value) = permutation_for_arrangement(
                    &key.iter()
                        .cloned()
                        .map(MirScalarExpr::Column)
                        .collect::<Vec<_>>(),
                    variant.desc().arity(),
                );
                let trace = collection
                    .map({
                        let mut row_buf = Row::default();
                        let mut datums = DatumVec::new();
                        move |row| {
                            let datums = datums.borrow_with(&row);
                            row_buf.packer().extend(key.iter().map(|k| datums[*k]));
                            let row_key = row_buf.clone();
                            row_buf.packer().extend(value.iter().map(|k| datums[*k]));
                            let row_val = row_buf.clone();
                            (row_key, row_val)
                        }
                    })
                    .arrange_named::<RowSpine<_, _, _, _>>(&format!("ArrangeByKey {:?}", variant))
                    .trace;
                result.insert(variant.clone(), (trace, Rc::clone(&token)));
            }

            if let Some((id, meta)) = config.sink_logs.get(&variant) {
                tracing::debug!("Persisting {:?} to {:?}", &variant, meta);
                persist_sink(*id, meta, compute_state, collection);
            }
        }
        result
    });

    traces
}

pub(crate) trait LogImportFrontiers {
    fn log_import_frontiers(
        self,
        logger: Logger,
        import_id: GlobalId,
        export_ids: Vec<GlobalId>,
    ) -> Self;
}

impl<G, C> LogImportFrontiers for StreamCore<G, C>
where
    G: Scope<Timestamp = Timestamp>,
    C: Container,
{
    fn log_import_frontiers(
        self,
        logger: Logger,
        import_id: GlobalId,
        export_ids: Vec<GlobalId>,
    ) -> Self {
        let mut previous_time = None;
        self.inspect_container(move |event| {
            if let Err(frontier) = event {
                if let Some(previous) = previous_time {
                    for &export_id in export_ids.iter() {
                        logger.log(ComputeEvent::ImportFrontier {
                            import_id,
                            export_id,
                            time: previous,
                            diff: -1,
                        });
                    }
                }
                if let Some(time) = frontier.get(0) {
                    for &export_id in export_ids.iter() {
                        logger.log(ComputeEvent::ImportFrontier {
                            import_id,
                            export_id,
                            time: *time,
                            diff: 1,
                        });
                    }
                    previous_time = Some(*time);
                } else {
                    previous_time = None;
                }
            }
        })
    }
}

impl<G, Tr> LogImportFrontiers for Arranged<G, Tr>
where
    G: Scope<Timestamp = Timestamp>,
    Tr: TraceReader + Clone,
{
    fn log_import_frontiers(
        mut self,
        logger: Logger,
        import_id: GlobalId,
        export_ids: Vec<GlobalId>,
    ) -> Self {
        self.stream = self
            .stream
            .log_import_frontiers(logger, import_id, export_ids);
        self
    }
}
