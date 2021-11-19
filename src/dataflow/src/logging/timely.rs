// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Logging dataflows for events generated by timely dataflow.

use std::collections::HashMap;
use std::time::Duration;

use differential_dataflow::collection::AsCollection;
use differential_dataflow::operators::arrange::arrangement::Arrange;
use timely::communication::Allocate;
use timely::dataflow::channels::pact::Exchange;
use timely::dataflow::operators::capture::EventLink;
use timely::logging::{ParkEvent, TimelyEvent, WorkerIdentifier};

use super::{LogVariant, TimelyLog};
use crate::activator::RcActivator;
use crate::arrangement::manager::RowSpine;
use crate::arrangement::KeysValsHandle;
use crate::logging::ConsolidateBuffer;
use crate::replay::MzReplay;
use dataflow_types::logging::LoggingConfig;
use dataflow_types::plan::Permutation;
use repr::{datum_list_size, datum_size, Datum, DatumVec, Row, Timestamp};

/// Constructs the logging dataflow for timely logs.
///
/// Params
/// * `worker`: The Timely worker hosting the log analysis dataflow.
/// * `config`: Logging configuration
/// * `linked`: The source to read log events from.
/// * `activator`: A handle to acknowledge activations.
///
/// Returns a map from log variant to a tuple of a trace handle and a permutation to reconstruct
/// the original rows.
pub fn construct<A: Allocate>(
    worker: &mut timely::worker::Worker<A>,
    config: &LoggingConfig,
    linked: std::rc::Rc<EventLink<Timestamp, (Duration, WorkerIdentifier, TimelyEvent)>>,
    activator: RcActivator,
) -> std::collections::HashMap<LogVariant, (KeysValsHandle, Permutation)> {
    let granularity_ms = std::cmp::max(1, config.granularity_ns / 1_000_000) as Timestamp;
    let peers = worker.peers();

    // A dataflow for multiple log-derived arrangements.
    let traces = worker.dataflow_named("Dataflow: timely logging", move |scope| {
        let logs = Some(linked).mz_replay(
            scope,
            "timely logs",
            Duration::from_nanos(config.granularity_ns as u64),
            activator,
        );

        use timely::dataflow::operators::generic::builder_rc::OperatorBuilder;

        let mut demux = OperatorBuilder::new("Timely Logging Demux".to_string(), scope.clone());

        use timely::dataflow::channels::pact::Pipeline;
        let mut input = demux.new_input(&logs, Pipeline);

        let (mut operates_out, operates) = demux.new_output();
        let (mut channels_out, channels) = demux.new_output();
        let (mut addresses_out, addresses) = demux.new_output();
        let (mut parks_out, parks) = demux.new_output();
        let (mut messages_sent_out, messages_sent) = demux.new_output();
        let (mut messages_received_out, messages_received) = demux.new_output();
        let (mut schedules_duration_out, schedules_duration) = demux.new_output();
        let (mut schedules_histogram_out, schedules_histogram) = demux.new_output();

        let mut demux_buffer = Vec::new();
        demux.build(move |_capability| {
            // These two maps track operator and channel information
            // so that they can be deleted when we observe the drop
            // events for the corresponding operators.
            let mut operates_data = HashMap::new();
            let mut channels_data = HashMap::new();
            let mut parks_data = HashMap::new();
            let mut schedules_stash = HashMap::new();
            let mut messages_sent_data: HashMap<_, Vec<isize>> = HashMap::new();
            let mut messages_received_data: HashMap<_, Vec<isize>> = HashMap::new();
            let mut schedules_data: HashMap<_, Vec<(isize, isize)>> = HashMap::new();
            move |_frontiers| {
                let operates = operates_out.activate();
                let channels = channels_out.activate();
                let addresses = addresses_out.activate();
                let parks = parks_out.activate();
                let messages_sent = messages_sent_out.activate();
                let messages_received = messages_received_out.activate();
                let schedules_duration = schedules_duration_out.activate();
                let schedules_histogram = schedules_histogram_out.activate();

                let mut operates_session = ConsolidateBuffer::new(operates, 0);
                let mut channels_session = ConsolidateBuffer::new(channels, 1);
                let mut addresses_session = ConsolidateBuffer::new(addresses, 2);
                let mut parks_sesssion = ConsolidateBuffer::new(parks, 3);
                let mut messages_sent_session = ConsolidateBuffer::new(messages_sent, 4);
                let mut messages_received_session = ConsolidateBuffer::new(messages_received, 5);
                let mut schedules_duration_session = ConsolidateBuffer::new(schedules_duration, 6);
                let mut schedules_histogram_session =
                    ConsolidateBuffer::new(schedules_histogram, 7);

                input.for_each(|cap, data| {
                    data.swap(&mut demux_buffer);

                    for (time, worker, datum) in demux_buffer.drain(..) {
                        let time_ns = time.as_nanos();
                        let time_ms = (((time.as_millis() as Timestamp / granularity_ms) + 1)
                            * granularity_ms) as Timestamp;

                        match datum {
                            TimelyEvent::Operates(event) => {
                                // Record operator information so that we can replay a negated
                                // version when the operator is dropped.
                                operates_data.insert((event.id, worker), event.clone());

                                operates_session
                                    .give(&cap, (((event.id, worker), event.name), time_ms, 1));

                                let address_row = (event.id as i64, worker as i64, event.addr);
                                addresses_session.give(&cap, (address_row, time_ms, 1));
                            }
                            TimelyEvent::Channels(event) => {
                                // Record channel information so that we can replay a negated
                                // version when the host dataflow is dropped.
                                channels_data
                                    .entry((event.scope_addr[0], worker))
                                    .or_insert_with(Vec::new)
                                    .push(event.clone());

                                // Present channel description.
                                let d = (
                                    (event.id, worker),
                                    event.source.0,
                                    event.source.1,
                                    event.target.0,
                                    event.target.1,
                                );
                                channels_session.give(&cap, (d, time_ms, 1));

                                let address_row =
                                    (event.id as i64, worker as i64, event.scope_addr);
                                addresses_session.give(&cap, (address_row, time_ms, 1));
                            }
                            TimelyEvent::Shutdown(event) => {
                                // Dropped operators should result in a negative record for
                                // the `operates` collection, cancelling out the initial
                                // operator announcement.
                                if let Some(event) = operates_data.remove(&(event.id, worker)) {
                                    operates_session.give(
                                        &cap,
                                        (((event.id, worker), event.name), time_ms, -1),
                                    );

                                    // Retract schedules information for the operator
                                    if let Some(schedules) =
                                        schedules_data.remove(&(event.id, worker))
                                    {
                                        for (index, (pow, elapsed_ns)) in schedules
                                            .into_iter()
                                            .enumerate()
                                            .filter(|(_, (pow, _))| *pow != 0)
                                        {
                                            schedules_duration_session.give(
                                                &cap,
                                                ((event.id, worker), time_ms, -elapsed_ns),
                                            );
                                            schedules_histogram_session.give(
                                                &cap,
                                                ((event.id, worker, 1 << index), time_ms, -pow),
                                            );
                                        }
                                    }

                                    // If we are observing a dataflow shutdown, we should also
                                    // issue a deletion for channels in the dataflow.
                                    if event.addr.len() == 1 {
                                        let dataflow_id = event.addr[0];
                                        if let Some(events) =
                                            channels_data.remove(&(dataflow_id, worker))
                                        {
                                            for event in events {
                                                // Retract channel description.
                                                let d = (
                                                    (event.id, worker),
                                                    event.source.0,
                                                    event.source.1,
                                                    event.target.0,
                                                    event.target.1,
                                                );
                                                channels_session.give(&cap, (d, time_ms, -1));

                                                if let Some(sent) =
                                                    messages_sent_data.remove(&(event.id, worker))
                                                {
                                                    for (index, count) in sent.iter().enumerate() {
                                                        let data = (
                                                            ((event.id, worker), index),
                                                            time_ms,
                                                            -count,
                                                        );
                                                        messages_sent_session.give(&cap, data);
                                                    }
                                                }
                                                if let Some(received) = messages_received_data
                                                    .remove(&(event.id, worker))
                                                {
                                                    for (index, count) in
                                                        received.iter().enumerate()
                                                    {
                                                        let data = (
                                                            ((event.id, worker), index),
                                                            time_ms,
                                                            -count,
                                                        );
                                                        messages_received_session.give(&cap, data);
                                                    }
                                                }

                                                let address_row = (
                                                    event.id as i64,
                                                    worker as i64,
                                                    event.scope_addr,
                                                );
                                                addresses_session
                                                    .give(&cap, (address_row, time_ms, -1));
                                            }
                                        }
                                    }

                                    let address_row = (event.id as i64, worker as i64, event.addr);
                                    addresses_session.give(&cap, (address_row, time_ms, -1));
                                }
                            }
                            TimelyEvent::Park(event) => match event {
                                ParkEvent::Park(duration) => {
                                    parks_data.insert(worker, (time_ns, duration));
                                }
                                ParkEvent::Unpark => {
                                    if let Some((start_ns, requested)) = parks_data.remove(&worker)
                                    {
                                        let duration_ns = time_ns - start_ns;
                                        let requested =
                                            requested.map(|r| r.as_nanos().next_power_of_two());
                                        let pow = duration_ns.next_power_of_two();
                                        parks_sesssion.give(
                                            &cap,
                                            ((worker, pow as i64, requested), time_ms, 1),
                                        );
                                    } else {
                                        panic!("Park data not found!");
                                    }
                                }
                            },

                            TimelyEvent::Messages(event) => {
                                if event.is_send {
                                    // Record messages sent per channel and source
                                    // We can send data to at most `peers` targets.
                                    messages_sent_data
                                        .entry((event.channel, event.source))
                                        .or_insert_with(|| vec![0; peers])[event.target] +=
                                        event.length as isize;
                                    let d = ((event.channel, event.source), event.target);
                                    messages_sent_session
                                        .give(&cap, (d, time_ms, event.length as isize));
                                } else {
                                    // Record messages received per channel and target
                                    // We can receive data from at most `peers` targets.
                                    messages_received_data
                                        .entry((event.channel, event.target))
                                        .or_insert_with(|| vec![0; peers])[event.source] +=
                                        event.length as isize;
                                    let d = ((event.channel, event.target), event.source);
                                    messages_received_session
                                        .give(&cap, (d, time_ms, event.length as isize));
                                }
                            }
                            TimelyEvent::Schedule(event) => {
                                let key = (worker, event.id);
                                match event.start_stop {
                                    timely::logging::StartStop::Start => {
                                        debug_assert!(!schedules_stash.contains_key(&key));
                                        schedules_stash.insert(key, time_ns);
                                    }
                                    timely::logging::StartStop::Stop => {
                                        debug_assert!(schedules_stash.contains_key(&key));
                                        let start = schedules_stash
                                            .remove(&key)
                                            .expect("start event absent");
                                        let elapsed_ns = time_ns - start;

                                        // Record count and elapsed for retraction
                                        // Note that we store the histogram for retraction with
                                        // 64 buckets, which should be enough to cover all scheduling
                                        // durations up to ~500 years. One bucket is an `(isize, isize`)
                                        // pair, which should consume 1KiB on 64-bit arch per entry.
                                        let mut schedule_entry = &mut schedules_data
                                            .entry((event.id, worker))
                                            .or_insert_with(|| vec![(0, 0); 64])
                                            [elapsed_ns.next_power_of_two().trailing_zeros()
                                                as usize];
                                        schedule_entry.0 += 1;
                                        schedule_entry.1 += elapsed_ns as isize;

                                        schedules_duration_session.give(
                                            &cap,
                                            ((key.1, worker), time_ms, elapsed_ns as isize),
                                        );
                                        let d = (key.1, worker, elapsed_ns.next_power_of_two());
                                        schedules_histogram_session.give(&cap, (d, time_ms, 1));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
        });

        // Accumulate the durations of each operator.
        let elapsed = schedules_duration
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(|(((_, w), ()), _, _)| *w as u64),
                "PreArrange Timely duration",
            )
            .as_collection(|(op, worker), _| {
                Row::pack_slice(&[Datum::Int64(*op as i64), Datum::Int64(*worker as i64)])
            });

        // Accumulate histograms of execution times for each operator.
        let histogram = schedules_histogram
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(|(((_, w, _), ()), _, _)| *w as u64),
                "PreArrange Timely histogram",
            )
            .as_collection(|(op, worker, pow), _| {
                let row = Row::pack_slice(&[
                    Datum::Int64(*op as i64),
                    Datum::Int64(*worker as i64),
                    Datum::Int64(*pow as i64),
                ]);
                row
            });

        let operates = operates
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(|((((_, w), _), ()), _, _)| *w as u64),
                "PreArrange Timely operates",
            )
            .as_collection(move |((id, worker), name), _| {
                Row::pack_slice(&[
                    Datum::Int64(*id as i64),
                    Datum::Int64(*worker as i64),
                    Datum::String(&name),
                ])
            });

        let addresses = addresses
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(|(((_, w, _), ()), _, _)| *w as u64),
                "PreArrange Timely addresses",
            )
            .as_collection(|(event_id, worker, addr), _| {
                create_address_row(*event_id as i64, *worker as i64, &addr)
            });

        let parks = parks
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(|(((w, _, _), ()), _, _)| *w as u64),
                "PreArrange Timely parks",
            )
            .as_collection(|(worker, duration_ns, requested), ()| {
                Row::pack_slice(&[
                    Datum::Int64(*worker as i64),
                    Datum::Int64(*duration_ns),
                    requested
                        .map(|requested| Datum::Int64(requested as i64))
                        .unwrap_or(Datum::Null),
                ])
            });

        let messages_received = messages_received
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(|((((_, w), _), ()), _, _)| *w as u64),
                "PreArrange Timely messages received",
            )
            .as_collection(move |((channel, source), target), ()| {
                Row::pack_slice(&[
                    Datum::Int64(*channel as i64),
                    Datum::Int64(*source as i64),
                    Datum::Int64(*target as i64),
                ])
            });

        let messages_sent = messages_sent
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(|((((_, w), _), ()), _, _)| *w as u64),
                "PreArrange Timely messages sent",
            )
            .as_collection(move |((channel, source), target), ()| {
                Row::pack_slice(&[
                    Datum::Int64(*channel as i64),
                    Datum::Int64(*source as i64),
                    Datum::Int64(*target as i64),
                ])
            });

        let channels = channels
            .as_collection()
            .arrange_core::<_, RowSpine<_, _, _, _>>(
                Exchange::new(|((((_, w), _, _, _, _), ()), _, _)| *w as u64),
                "PreArrange Timely operates",
            )
            .as_collection(
                move |((id, worker), source_node, source_port, target_node, target_port), ()| {
                    Row::pack_slice(&[
                        Datum::Int64(*id as i64),
                        Datum::Int64(*worker as i64),
                        Datum::Int64(*source_node as i64),
                        Datum::Int64(*source_port as i64),
                        Datum::Int64(*target_node as i64),
                        Datum::Int64(*target_port as i64),
                    ])
                },
            );

        // Restrict results by those logs that are meant to be active.
        let logs = vec![
            (LogVariant::Timely(TimelyLog::Operates), operates),
            (LogVariant::Timely(TimelyLog::Channels), channels),
            (LogVariant::Timely(TimelyLog::Elapsed), elapsed),
            (LogVariant::Timely(TimelyLog::Histogram), histogram),
            (LogVariant::Timely(TimelyLog::Addresses), addresses),
            (LogVariant::Timely(TimelyLog::Parks), parks),
            (LogVariant::Timely(TimelyLog::MessagesSent), messages_sent),
            (
                LogVariant::Timely(TimelyLog::MessagesReceived),
                messages_received,
            ),
        ];

        let mut result = std::collections::HashMap::new();
        for (variant, collection) in logs {
            if config.active_logs.contains_key(&variant) {
                let key = variant.index_by();
                let (permutation, value) =
                    Permutation::construct_from_columns(&key, variant.desc().arity());
                let trace = collection
                    .map({
                        let mut row_packer = Row::default();
                        let mut datums = DatumVec::new();
                        move |row| {
                            let datums = datums.borrow_with(&row);
                            row_packer.extend(key.iter().map(|k| datums[*k]));
                            let row_key = row_packer.finish_and_reuse();
                            row_packer.extend(value.iter().map(|k| datums[*k]));
                            (row_key, row_packer.finish_and_reuse())
                        }
                    })
                    .arrange_named::<RowSpine<_, _, _, _>>(&format!("ArrangeByKey {:?}", variant))
                    .trace;
                result.insert(variant, (trace, permutation));
            }
        }
        result
    });

    traces
}

fn create_address_row(id: i64, worker: i64, address: &[usize]) -> Row {
    let id_datum = Datum::Int64(id);
    let worker_datum = Datum::Int64(worker);
    // we're collecting into a Vec because we need to iterate over the Datums
    // twice: once for determining the size of the row, then again for pushing
    // them
    let address_datums: Vec<_> = address.iter().map(|i| Datum::Int64(*i as i64)).collect();

    let row_capacity =
        datum_size(&id_datum) + datum_size(&worker_datum) + datum_list_size(&address_datums);

    let mut address_row = Row::with_capacity(row_capacity);
    address_row.push(id_datum);
    address_row.push(worker_datum);
    address_row.push_list(address_datums);

    address_row
}
