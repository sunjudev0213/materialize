// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A client backed by multiple replicas.
//!
//! This client accepts commands and responds as would a correctly implemented client.
//! Its implementation is wrapped around clients that may fail at any point, and restart.
//! To accommodate this, it records the commands it accepts, and should a client restart
//! the commands are replayed at it, with some modification. As the clients respond, the
//! wrapper client tracks the responses and ensures that they are "logically deduplicated",
//! so that the receiver need not be aware of the replication and restarting.
//!
//! This tactic requires that dataflows be restartable, which they generally are not, due
//! to allowed compaction of their source data. This client must correctly observe commands
//! that allow for compaction of its assets, and only attempt to rebuild them as of those
//! compacted frontiers, as the underlying resources to rebuild them any earlier may not
//! exist any longer.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Duration;

use anyhow::bail;
use chrono::{DateTime, Utc};
use differential_dataflow::lattice::Lattice;
use futures::future;
use futures::stream::{FuturesUnordered, StreamExt};
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;
use tokio::select;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};

use mz_build_info::BuildInfo;
use mz_ore::retry::Retry;
use mz_ore::task::{AbortOnDropHandle, JoinHandleExt};
use mz_ore::tracing::OpenTelemetryContext;
use mz_repr::GlobalId;
use mz_service::client::GenericClient;
use mz_storage::controller::CollectionMetadata;

use crate::command::{ComputeCommand, ComputeCommandHistory, Peek, ReplicaId};
use crate::logging::LogVariant;
use crate::response::{ComputeResponse, PeekResponse, TailBatch, TailResponse};
use crate::service::{ComputeClient, ComputeGrpcClient};

/// Configuration for `replica_task`.
struct ReplicaTaskConfig<T> {
    /// The ID of the replica.
    replica_id: ReplicaId,
    /// The network addresses of the processes in the replica.
    addrs: Vec<String>,
    /// The build information for this process.
    build_info: &'static BuildInfo,
    /// A channel upon which commands intended for the replica are delivered.
    command_rx: UnboundedReceiver<ComputeCommand<T>>,
    /// A channel upon which responses from the replica are delivered.
    response_tx: UnboundedSender<ComputeResponse<T>>,
}

/// Asynchronously forwards commands to and responses from a single replica.
async fn replica_task<T>(config: ReplicaTaskConfig<T>)
where
    T: Timestamp + Lattice,
    ComputeGrpcClient: ComputeClient<T>,
{
    let replica_id = config.replica_id;
    info!("starting replica task for {replica_id}");
    match run_replica_core(config).await {
        Ok(()) => info!("gracefully stopping replica task for {replica_id}"),
        Err(e) => warn!("replica task for {replica_id} failed: {e}"),
    }
}

async fn run_replica_core<T>(
    ReplicaTaskConfig {
        replica_id,
        addrs,
        build_info,
        mut command_rx,
        response_tx,
    }: ReplicaTaskConfig<T>,
) -> Result<(), anyhow::Error>
where
    T: Timestamp + Lattice,
    ComputeGrpcClient: ComputeClient<T>,
{
    let mut client = Retry::default()
        .clamp_backoff(Duration::from_secs(32))
        .retry_async(|state| {
            let addrs = addrs.clone();
            let version = build_info.semver_version();
            async move {
                match ComputeGrpcClient::connect_partitioned(addrs, version).await {
                    Ok(client) => Ok(client),
                    Err(e) => {
                        warn!(
                            "error connecting to replica {replica_id}, retrying in {:?}: {e}",
                            state.next_backoff.unwrap()
                        );
                        Err(e)
                    }
                }
            }
        })
        .await
        .expect("retry retries forever");

    loop {
        select! {
            // Command from controller to forward to replica.
            command = command_rx.recv() => match command {
                None => {
                    // Controller is no longer interested in this replica. Shut
                    // down.
                    return Ok(())
                }
                Some(command) => client.send(command).await?,
            },
            // Response from replica to forward to controller.
            response = client.recv() => {
                let response = match response? {
                    None => bail!("replica unexpectedly gracefully terminated connection"),
                    Some(response) => response,
                };
                if response_tx.send(response).is_err() {
                    // Controller is no longer interested in this replica. Shut
                    // down.
                    return Ok(());
                }
            }
        }
    }
}

/// Additional information to store with pening peeks.
#[derive(Debug)]
struct PendingPeek {
    /// The OpenTelemetry context for this peek.
    otel_ctx: OpenTelemetryContext,
}

/// The internal state of the client.
///
/// This lives in a separate struct from the handles to the individual replica
/// tasks, so that we can call methods on it
/// while holding mutable borrows to those.
#[derive(Debug)]
struct ActiveReplicationState<T> {
    /// Outstanding peek identifiers, to guide responses (and which to suppress).
    peeks: HashMap<uuid::Uuid, PendingPeek>,
    /// Reported frontier of each in-progress tail.
    tails: HashMap<GlobalId, Antichain<T>>,
    /// Frontier information, unioned across all replicas.
    uppers: HashMap<GlobalId, Antichain<T>>,
    /// The command history, used when introducing new replicas or restarting existing replicas.
    history: ComputeCommandHistory<T>,
    /// Responses that should be emitted on the next `recv` call.
    ///
    /// This is introduced to produce peek cancelation responses eagerly, without awaiting a replica
    /// responding with the response itself, which allows us to compact away the peek in `self.history`.
    pending_response: VecDeque<ActiveReplicationResponse<T>>,
}

impl<T> ActiveReplicationState<T>
where
    T: Timestamp + Lattice,
{
    #[tracing::instrument(level = "debug", skip(self))]
    fn handle_command(&mut self, cmd: &ComputeCommand<T>) {
        // Update our tracking of peek commands.
        match &cmd {
            ComputeCommand::Peek(Peek { uuid, otel_ctx, .. }) => {
                self.peeks.insert(
                    *uuid,
                    PendingPeek {
                        // TODO(guswynn): can we just hold the `tracing::Span`
                        // here instead?
                        otel_ctx: otel_ctx.clone(),
                    },
                );
            }
            ComputeCommand::CancelPeeks { uuids } => {
                // Enqueue the response to the cancelation.
                self.pending_response.extend(uuids.iter().map(|uuid| {
                    // Canceled peeks should not be further responded to.
                    let otel_ctx = self
                        .peeks
                        .remove(uuid)
                        .map(|pending| pending.otel_ctx)
                        .unwrap_or_else(|| {
                            tracing::warn!("did not find pending peek for {}", uuid);
                            OpenTelemetryContext::empty()
                        });
                    ActiveReplicationResponse::ComputeResponse(ComputeResponse::PeekResponse(
                        *uuid,
                        PeekResponse::Canceled,
                        otel_ctx,
                    ))
                }));
            }
            _ => {}
        }

        // Initialize any necessary frontier tracking.
        let mut start = Vec::new();
        let mut cease = Vec::new();
        cmd.frontier_tracking(&mut start, &mut cease);
        for id in start.into_iter() {
            let frontier = timely::progress::Antichain::from_elem(T::minimum());

            let previous = self.uppers.insert(id, frontier);
            assert!(previous.is_none());
        }
        for id in cease.into_iter() {
            let previous = self.uppers.remove(&id);
            assert!(previous.is_some());
        }

        // Record the command so that new replicas can be brought up to speed.
        self.history.push(cmd.clone());
    }

    fn handle_response(
        &mut self,
        message: ComputeResponse<T>,
        replica_id: ReplicaId,
    ) -> Option<ActiveReplicationResponse<T>> {
        self.pending_response
            .push_front(ActiveReplicationResponse::ReplicaHeartbeat(
                replica_id,
                Utc::now(),
            ));
        match message {
            ComputeResponse::PeekResponse(uuid, response, otel_ctx) => {
                // If this is the first response, forward it; otherwise do not.
                // TODO: we could collect the other responses to assert equivalence?
                // Trades resources (memory) for reassurances; idk which is best.
                //
                // NOTE: we use the `otel_ctx` from the response, not the
                // pending peek, because we currently want the parent
                // to be whatever the compute worker did with this peek.
                //
                // Additionally, we just use the `otel_ctx` from the first worker to
                // respond.
                self.peeks.remove(&uuid).map(|_| {
                    ActiveReplicationResponse::ComputeResponse(ComputeResponse::PeekResponse(
                        uuid, response, otel_ctx,
                    ))
                })
            }
            ComputeResponse::FrontierUppers(list) => {
                let mut new_uppers = Vec::new();

                for (id, new_upper) in list {
                    if let Some(reported) = self.uppers.get_mut(&id) {
                        if PartialOrder::less_than(reported, &new_upper) {
                            reported.clone_from(&new_upper);
                            new_uppers.push((id, new_upper));
                        }
                    }
                }
                if !new_uppers.is_empty() {
                    Some(ActiveReplicationResponse::ComputeResponse(
                        ComputeResponse::FrontierUppers(new_uppers),
                    ))
                } else {
                    None
                }
            }
            ComputeResponse::TailResponse(id, response) => {
                match response {
                    TailResponse::Batch(TailBatch {
                        lower: _,
                        upper,
                        mut updates,
                    }) => {
                        // It is sufficient to compare `upper` against the last reported frontier for `id`,
                        // and if `upper` is not less or equal to that frontier, some progress has happened.
                        // If so, we retain only the updates greater or equal to that last reported frontier,
                        // and announce a batch from that frontier to its join with `upper`.

                        // Ensure that we have a recorded frontier ready to go.
                        let entry = self
                            .tails
                            .entry(id)
                            .or_insert_with(|| Antichain::from_elem(T::minimum()));
                        // If the upper frontier has changed, we have a statement to make.
                        // This happens if there is any element of `entry` not greater or
                        // equal to some element of `upper`.
                        let new_upper = entry.join(&upper);
                        if &new_upper != entry {
                            let new_lower = entry.clone();
                            entry.clone_from(&new_upper);
                            updates.retain(|(time, _data, _diff)| new_lower.less_equal(time));
                            Some(ActiveReplicationResponse::ComputeResponse(
                                ComputeResponse::TailResponse(
                                    id,
                                    TailResponse::Batch(TailBatch {
                                        lower: new_lower,
                                        upper: new_upper,
                                        updates,
                                    }),
                                ),
                            ))
                        } else {
                            None
                        }
                    }
                    TailResponse::DroppedAt(frontier) => {
                        // Introduce a new terminal frontier to suppress all future responses.
                        // We cannot simply remove the entry, as we currently create new entries in response
                        // to observed responses; if we pre-load the entries in response to commands we can
                        // clean up the state here.
                        self.tails.insert(id, Antichain::new());
                        Some(ActiveReplicationResponse::ComputeResponse(
                            ComputeResponse::TailResponse(id, TailResponse::DroppedAt(frontier)),
                        ))
                    }
                }
            }
        }
    }
}

/// A client backed by multiple replicas.
#[derive(Debug)]
pub(super) struct ActiveReplication<T> {
    /// The build information for this process.
    build_info: &'static BuildInfo,
    /// State for each replica.
    replicas: HashMap<ReplicaId, ReplicaState<T>>,
    /// All other internal state of the client
    state: ActiveReplicationState<T>,
}

impl<T> ActiveReplication<T> {
    pub(super) fn new(build_info: &'static BuildInfo) -> Self {
        Self {
            build_info,
            replicas: Default::default(),
            state: ActiveReplicationState {
                peeks: Default::default(),
                tails: Default::default(),
                uppers: Default::default(),
                history: Default::default(),
                pending_response: Default::default(),
            },
        }
    }
}

/// State for a single replica.
#[derive(Debug)]
struct ReplicaState<T> {
    /// A sender for commands for the replica.
    ///
    /// If sending to this channel fails, the replica has failed and requires
    /// rehydration.
    command_tx: UnboundedSender<ComputeCommand<T>>,
    /// A receiver for responses from the replica.
    ///
    /// If receiving from the channel returns `None`, the replica has failed
    /// and requires rehydration.
    response_rx: UnboundedReceiver<ComputeResponse<T>>,
    /// A handle to the task that aborts it when the replica is dropped.
    _task: AbortOnDropHandle<()>,
    /// The network addresses of the processes that make up the replica.
    addrs: Vec<String>,
    /// Where to persist introspection sources
    persisted_logs: BTreeMap<LogVariant, (GlobalId, CollectionMetadata)>,
}

impl<T> ReplicaState<T> {
    /// Specialize a command for the given `Replica` and `ReplicaId`.
    ///
    /// Most `ComputeCommand`s are independent of the target replica, but some
    /// contain replica-specific fields that must be adjusted before sending.
    fn specialize_command(&self, command: &mut ComputeCommand<T>, replica_id: ReplicaId) {
        // Set new replica ID and obtain set the sinked logs specific to this replica
        if let ComputeCommand::CreateInstance(config) = command {
            // Set sink_logs
            if let Some(logging) = &mut config.logging {
                logging.sink_logs = self.persisted_logs.clone();
                tracing::debug!(
                    "Enabling sink_logs at replica {:?}: {:?}",
                    replica_id,
                    &logging.sink_logs
                );
            };

            // Set replica id
            config.replica_id = replica_id;
        }
    }
}

impl<T> ActiveReplication<T>
where
    T: Timestamp + Lattice,
    ComputeGrpcClient: ComputeClient<T>,
{
    /// Introduce a new replica, and catch it up to the commands of other replicas.
    ///
    /// It is not yet clear under which circumstances a replica can be removed.
    pub(super) fn add_replica(
        &mut self,
        id: ReplicaId,
        addrs: Vec<String>,
        persisted_logs: BTreeMap<LogVariant, (GlobalId, CollectionMetadata)>,
    ) {
        // Launch a task to handle communication with the replica
        // asynchronously. This isolates the main controller thread from
        // the replica.
        let (command_tx, command_rx) = unbounded_channel();
        let (response_tx, response_rx) = unbounded_channel();
        let task = mz_ore::task::spawn(
            || format!("active-replication-replica-{id}"),
            replica_task(ReplicaTaskConfig {
                replica_id: id,
                build_info: self.build_info,
                addrs: addrs.clone(),
                command_rx,
                response_tx,
            }),
        );

        // Take this opportunity to clean up the history we should present.
        self.state.history.retain_peeks(&self.state.peeks);
        self.state.history.reduce();

        let replica_state = ReplicaState {
            command_tx,
            response_rx,
            _task: task.abort_on_drop(),
            addrs,
            persisted_logs,
        };

        // Replay the commands at the client, creating new dataflow identifiers.
        for command in self.state.history.iter() {
            let mut command = command.clone();
            replica_state.specialize_command(&mut command, id);
            replica_state
                .command_tx
                .send(command)
                .expect("Channel to client has gone away!")
        }

        // Start tracking frontiers of persisted_logs collections.
        for (id, _) in replica_state.persisted_logs.values() {
            let frontier = Antichain::from_elem(Timestamp::minimum());
            let previous = self.state.uppers.insert(*id, frontier);
            assert!(previous.is_none());
        }

        // Add replica to tracked state.
        self.replicas.insert(id, replica_state);
    }

    /// Returns an iterator over the IDs of the replicas.
    pub(super) fn get_replica_ids(&self) -> impl Iterator<Item = ReplicaId> + '_ {
        self.replicas.keys().copied()
    }

    /// Remove a replica by its identifier.
    pub(super) fn remove_replica(&mut self, id: ReplicaId) {
        let replica_state = self.replicas.remove(&id).expect("replica not found");

        // Cease tracking frontiers of persisted_logs collections.
        for (id, _) in replica_state.persisted_logs.values() {
            let previous = self.state.uppers.remove(id);
            assert!(previous.is_some());
        }
    }

    fn rehydrate_replica(&mut self, id: ReplicaId) {
        let addrs = self.replicas[&id].addrs.clone();
        let persisted_logs = self.replicas[&id].persisted_logs.clone();
        self.remove_replica(id);
        self.add_replica(id, addrs, persisted_logs);
    }

    // We avoid implementing `GenericClient` here, because the protocol between
    // the compute controller and this client is subtly but meaningfully different:
    // this client is expected to handle errors, rather than propagate them, and therefore
    // it returns infallible values.

    /// Sends a command to all replicas.
    #[tracing::instrument(level = "debug", skip(self))]
    pub(super) fn send(&mut self, cmd: ComputeCommand<T>) {
        self.state.handle_command(&cmd);

        // Clone the command for each active replica.
        let mut failed_replicas = vec![];
        for (id, replica) in self.replicas.iter_mut() {
            let mut command = cmd.clone();
            replica.specialize_command(&mut command, *id);
            // If sending the command fails, the replica requires rehydration.
            if replica.command_tx.send(command).is_err() {
                failed_replicas.push(*id);
            }
        }
        for id in failed_replicas {
            self.rehydrate_replica(id);
        }
    }

    /// Receives the next response from any replica.
    ///
    /// This method is cancellation safe.
    pub(super) async fn recv(&mut self) -> ActiveReplicationResponse<T> {
        // If we have a pending response, we should send it immediately.
        if let Some(response) = self.state.pending_response.pop_front() {
            return response;
        }

        // Receive responses from any of the replicas, and take appropriate
        // action.
        loop {
            let mut responses = self
                .replicas
                .iter_mut()
                .map(|(id, replica)| async { (*id, replica.response_rx.recv().await) })
                .collect::<FuturesUnordered<_>>();
            match responses.next().await {
                None => {
                    // There were no replicas in the set. Block forever to
                    // communicate that no response is ready.
                    future::pending().await
                }
                Some((replica_id, None)) => {
                    // A replica has failed and requires rehydration.
                    drop(responses);
                    self.rehydrate_replica(replica_id)
                }
                Some((replica_id, Some(response))) => {
                    // A replica has produced a response. Absorb it, possibly
                    // returning a response up the stack.
                    match self.state.handle_response(response, replica_id) {
                        Some(response) => return response,
                        None => { /* continue */ }
                    }
                }
            }
        }
    }
}

/// A response from the ActiveReplication client:
/// either a deduplicated compute response, or a notification
/// that we heard from a given replica and should update its recency status.
#[derive(Debug, Clone)]
pub(super) enum ActiveReplicationResponse<T = mz_repr::Timestamp> {
    /// A response from the underlying compute replica.
    ComputeResponse(ComputeResponse<T>),
    /// A notification that we heard a response from the given replica at the
    /// given time.
    ReplicaHeartbeat(ReplicaId, DateTime<Utc>),
}
