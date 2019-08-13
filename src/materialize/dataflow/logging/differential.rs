// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use super::{BatchLogger, DifferentialLog, LogVariant};
use crate::dataflow::arrangement::KeysOnlyHandle;
use crate::dataflow::types::Timestamp;
use crate::repr::Datum;
use differential_dataflow::logging::DifferentialEvent;
use std::time::Duration;
use timely::communication::Allocate;
use timely::dataflow::operators::capture::EventLink;
use timely::dataflow::operators::probe::Probe;
use timely::dataflow::ProbeHandle;
use timely::logging::WorkerIdentifier;

pub fn construct<A: Allocate>(
    worker: &mut timely::worker::Worker<A>,
    probe: &mut ProbeHandle<Timestamp>,
    config: &super::LoggingConfiguration,
) -> (
    BatchLogger<
        DifferentialEvent,
        WorkerIdentifier,
        std::rc::Rc<EventLink<Timestamp, (Duration, WorkerIdentifier, DifferentialEvent)>>,
    >,
    std::collections::HashMap<LogVariant, KeysOnlyHandle>,
) {
    // Create timely dataflow logger based on shared linked lists.
    let writer = EventLink::<Timestamp, (Duration, WorkerIdentifier, DifferentialEvent)>::new();
    let writer = std::rc::Rc::new(writer);
    let reader = writer.clone();

    // let granularity_ns = config.granularity_ns;
    let granularity_ms = std::cmp::max(1, config.granularity_ns / 1_000_000) as Timestamp;

    // The two return values.
    let logger = BatchLogger::new(writer);

    let traces = worker.dataflow(move |scope| {
        use differential_dataflow::collection::AsCollection;
        use differential_dataflow::operators::arrange::arrangement::ArrangeBySelf;
        use differential_dataflow::operators::reduce::Count;
        use timely::dataflow::operators::capture::Replay;
        use timely::dataflow::operators::Map;

        // TODO: Rewrite as one operator with multiple outputs.
        let logs = Some(reader).replay_into(scope);

        // Duration statistics derive from the non-rounded event times.
        let arrangements = logs
            .flat_map(move |(ts, worker, event)| {
                let time_ms = ((ts.as_millis() as Timestamp / granularity_ms) + 1) * granularity_ms;
                match event {
                    DifferentialEvent::Batch(event) => {
                        let difference = differential_dataflow::difference::DiffVector::new(vec![
                            event.length as isize,
                            1,
                        ]);
                        Some(((event.operator, worker), time_ms, difference))
                    }
                    DifferentialEvent::Merge(event) => {
                        if let Some(done) = event.complete {
                            Some((
                                (event.operator, worker),
                                time_ms,
                                differential_dataflow::difference::DiffVector::new(vec![
                                    (done as isize) - ((event.length1 + event.length2) as isize),
                                    -1,
                                ]),
                            ))
                        } else {
                            None
                        }
                    }
                    DifferentialEvent::MergeShortfall(_) => None,
                }
            })
            .as_collection()
            .count()
            .map(|((op, worker), count)| {
                vec![
                    Datum::Int64(op as i64),
                    Datum::Int64(worker as i64),
                    Datum::Int64(count[0] as i64),
                    Datum::Int64(count[1] as i64),
                ]
            })
            .arrange_by_self();

        arrangements.stream.probe_with(probe);

        vec![(
            LogVariant::Differential(DifferentialLog::Arrangement),
            arrangements.trace,
        )]
        .into_iter()
        .filter(|(name, _trace)| config.active_logs.contains(name))
        .collect()
    });

    (logger, traces)
}
