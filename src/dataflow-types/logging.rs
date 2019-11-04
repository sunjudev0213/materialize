// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use repr::{LiteralName, QualName, RelationDesc, ScalarType};
use std::collections::HashSet;
use std::time::Duration;

/// Logging configuration.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    granularity_ns: u128,
    active_logs: HashSet<LogVariant>,
}

impl LoggingConfig {
    pub fn new(granularity: Duration) -> LoggingConfig {
        Self {
            granularity_ns: granularity.as_nanos(),
            active_logs: LogVariant::default_logs().into_iter().collect(),
        }
    }

    pub fn granularity_ns(&self) -> u128 {
        self.granularity_ns
    }

    pub fn active_logs(&self) -> &HashSet<LogVariant> {
        &self.active_logs
    }
}

#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub enum LogVariant {
    Timely(TimelyLog),
    Differential(DifferentialLog),
    Materialized(MaterializedLog),
}

#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub enum TimelyLog {
    Operates,
    Channels,
    Elapsed,
    Histogram,
    Addresses,
    Parks,
}

#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub enum DifferentialLog {
    Arrangement,
    Sharing,
}

#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub enum MaterializedLog {
    DataflowCurrent,
    DataflowDependency,
    FrontierCurrent,
    PeekCurrent,
    PeekDuration,
}

impl LogVariant {
    pub fn default_logs() -> Vec<LogVariant> {
        vec![
            LogVariant::Timely(TimelyLog::Operates),
            LogVariant::Timely(TimelyLog::Channels),
            LogVariant::Timely(TimelyLog::Elapsed),
            LogVariant::Timely(TimelyLog::Histogram),
            LogVariant::Timely(TimelyLog::Addresses),
            LogVariant::Timely(TimelyLog::Parks),
            LogVariant::Differential(DifferentialLog::Arrangement),
            LogVariant::Differential(DifferentialLog::Sharing),
            LogVariant::Materialized(MaterializedLog::DataflowCurrent),
            LogVariant::Materialized(MaterializedLog::DataflowDependency),
            LogVariant::Materialized(MaterializedLog::FrontierCurrent),
            LogVariant::Materialized(MaterializedLog::PeekCurrent),
            LogVariant::Materialized(MaterializedLog::PeekDuration),
        ]
    }

    pub fn name(&self) -> QualName {
        // Bind all names in one place to avoid accidental clashes.
        match self {
            LogVariant::Timely(TimelyLog::Operates) => "logs_operates".lit(),
            LogVariant::Timely(TimelyLog::Channels) => "logs_channels".lit(),
            LogVariant::Timely(TimelyLog::Elapsed) => "logs_elapsed".lit(),
            LogVariant::Timely(TimelyLog::Histogram) => "logs_histogram".lit(),
            LogVariant::Timely(TimelyLog::Addresses) => "logs_addresses".lit(),
            LogVariant::Timely(TimelyLog::Parks) => "logs_parks".lit(),
            LogVariant::Differential(DifferentialLog::Arrangement) => "logs_arrangement".lit(),
            LogVariant::Differential(DifferentialLog::Sharing) => "logs_sharing".lit(),
            LogVariant::Materialized(MaterializedLog::DataflowCurrent) => "logs_dataflows".lit(),
            LogVariant::Materialized(MaterializedLog::DataflowDependency) => {
                "logs_dataflow_dependency".lit()
            }
            LogVariant::Materialized(MaterializedLog::FrontierCurrent) => "logs_frontiers".lit(),
            LogVariant::Materialized(MaterializedLog::PeekCurrent) => "logs_peeks".lit(),
            LogVariant::Materialized(MaterializedLog::PeekDuration) => "logs_peek_durations".lit(),
        }
    }

    /// By which columns should the logs be indexed.
    ///
    /// This is distinct from the `keys` property of the type, which indicates uniqueness.
    /// When keys exist these are good choices for indexing, but when they do not we still
    /// require index guidance.
    pub fn index_by(&self) -> Vec<usize> {
        let typ = self.schema().typ().clone();
        typ.keys
            .get(0)
            .cloned()
            .unwrap_or_else(|| (0..typ.column_types.len()).collect::<Vec<_>>())
    }

    pub fn schema(&self) -> RelationDesc {
        match self {
            LogVariant::Timely(TimelyLog::Operates) => RelationDesc::empty()
                .add_column("id".lit(), ScalarType::Int64)
                .add_column("worker".lit(), ScalarType::Int64)
                .add_column("name".lit(), ScalarType::String)
                .add_keys(vec![0, 1]),

            LogVariant::Timely(TimelyLog::Channels) => RelationDesc::empty()
                .add_column("id".lit(), ScalarType::Int64)
                .add_column("worker".lit(), ScalarType::Int64)
                .add_column("source_node".lit(), ScalarType::Int64)
                .add_column("source_port".lit(), ScalarType::Int64)
                .add_column("target_node".lit(), ScalarType::Int64)
                .add_column("target_port".lit(), ScalarType::Int64)
                .add_keys(vec![0, 1]),

            LogVariant::Timely(TimelyLog::Elapsed) => RelationDesc::empty()
                .add_column("id".lit(), ScalarType::Int64)
                .add_column("elapsed_ns".lit(), ScalarType::Int64)
                .add_keys(vec![0]),

            LogVariant::Timely(TimelyLog::Histogram) => RelationDesc::empty()
                .add_column("id".lit(), ScalarType::Int64)
                .add_column("duration_ns".lit(), ScalarType::Int64)
                .add_column("count".lit(), ScalarType::Int64)
                .add_keys(vec![0]),

            LogVariant::Timely(TimelyLog::Addresses) => RelationDesc::empty()
                .add_column("id".lit(), ScalarType::Int64)
                .add_column("worker".lit(), ScalarType::Int64)
                .add_column("slot".lit(), ScalarType::Int64)
                .add_column("value".lit(), ScalarType::Int64)
                .add_keys(vec![0, 1]),

            LogVariant::Timely(TimelyLog::Parks) => RelationDesc::empty()
                .add_column("worker".lit(), ScalarType::Int64)
                .add_column("slept_for".lit(), ScalarType::Int64)
                .add_column("requested".lit(), ScalarType::Int64)
                .add_column("count".lit(), ScalarType::Int64)
                .add_keys(vec![0, 1, 2]),

            LogVariant::Differential(DifferentialLog::Arrangement) => RelationDesc::empty()
                .add_column("operator".lit(), ScalarType::Int64)
                .add_column("worker".lit(), ScalarType::Int64)
                .add_column("records".lit(), ScalarType::Int64)
                .add_column("batches".lit(), ScalarType::Int64)
                .add_keys(vec![0, 1]),

            LogVariant::Differential(DifferentialLog::Sharing) => RelationDesc::empty()
                .add_column("operator".lit(), ScalarType::Int64)
                .add_column("worker".lit(), ScalarType::Int64)
                .add_column("count".lit(), ScalarType::Int64)
                .add_keys(vec![0, 1]),

            LogVariant::Materialized(MaterializedLog::DataflowCurrent) => RelationDesc::empty()
                .add_column("name".lit(), ScalarType::String)
                .add_column("worker".lit(), ScalarType::Int64)
                .add_keys(vec![0, 1]),

            LogVariant::Materialized(MaterializedLog::DataflowDependency) => RelationDesc::empty()
                .add_column("dataflow".lit(), ScalarType::String)
                .add_column("source".lit(), ScalarType::String)
                .add_column("worker".lit(), ScalarType::Int64),

            LogVariant::Materialized(MaterializedLog::FrontierCurrent) => RelationDesc::empty()
                .add_column("name".lit(), ScalarType::String)
                .add_column("time".lit(), ScalarType::Int64),

            LogVariant::Materialized(MaterializedLog::PeekCurrent) => RelationDesc::empty()
                .add_column("uuid".lit(), ScalarType::String)
                .add_column("worker".lit(), ScalarType::Int64)
                .add_column("name".lit(), ScalarType::String)
                .add_column("time".lit(), ScalarType::Int64)
                .add_keys(vec![0, 1]),

            LogVariant::Materialized(MaterializedLog::PeekDuration) => RelationDesc::empty()
                .add_column("worker".lit(), ScalarType::Int64)
                .add_column("duration_ns".lit(), ScalarType::Int64)
                .add_column("count".lit(), ScalarType::Int64)
                .add_keys(vec![0, 1]),
        }
    }
}
