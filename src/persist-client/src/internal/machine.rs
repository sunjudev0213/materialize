// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Implementation of the persist state machine.

use std::convert::Infallible;
use std::fmt::Debug;
use std::ops::{ControlFlow, ControlFlow::Break, ControlFlow::Continue};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use differential_dataflow::difference::Semigroup;
use differential_dataflow::lattice::Lattice;
use mz_ore::cast::CastFrom;
use mz_ore::task::spawn;
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

#[allow(unused_imports)] // False positive.
use mz_ore::fmt::FormatBuffer;
use mz_persist::location::{ExternalError, Indeterminate, SeqNo};
use mz_persist::retry::Retry;
use mz_persist_types::{Codec, Codec64, Opaque};

use crate::critical::CriticalReaderId;
use crate::error::{CodecMismatch, InvalidUsage};
use crate::internal::compact::CompactReq;
use crate::internal::maintenance::{LeaseExpiration, RoutineMaintenance, WriterMaintenance};
use crate::internal::metrics::{
    CmdMetrics, Metrics, MetricsRetryStream, RetryMetrics, ShardMetrics,
};
use crate::internal::paths::{PartialRollupKey, RollupId};
use crate::internal::state::{
    CompareAndAppendBreak, CriticalReaderState, HollowBatch, IdempotencyToken, LeasedReaderState,
    Since, State, StateCollections, Upper, WriterState,
};
use crate::internal::state_diff::StateDiff;
use crate::internal::state_versions::StateVersions;
use crate::internal::trace::{ApplyMergeResult, FueledMergeRes};
use crate::read::LeasedReaderId;
use crate::write::WriterId;
use crate::{PersistConfig, ShardId};

#[derive(Debug)]
pub struct Machine<K, V, T, D> {
    pub(crate) cfg: PersistConfig,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) shard_metrics: Arc<ShardMetrics>,
    pub(crate) state_versions: Arc<StateVersions>,

    state: State<K, V, T, D>,
}

// Impl Clone regardless of the type params.
impl<K, V, T: Clone, D> Clone for Machine<K, V, T, D> {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            metrics: Arc::clone(&self.metrics),
            shard_metrics: Arc::clone(&self.shard_metrics),
            state_versions: Arc::clone(&self.state_versions),
            state: self.state.clone(
                self.state.applier_version.clone(),
                self.state.hostname.clone(),
            ),
        }
    }
}

impl<K, V, T, D> Machine<K, V, T, D>
where
    K: Debug + Codec,
    V: Debug + Codec,
    T: Timestamp + Lattice + Codec64,
    D: Semigroup + Codec64,
{
    pub async fn new(
        cfg: PersistConfig,
        shard_id: ShardId,
        metrics: Arc<Metrics>,
        state_versions: Arc<StateVersions>,
    ) -> Result<Self, Box<CodecMismatch>> {
        let shard_metrics = metrics.shards.shard(&shard_id);
        let state = metrics
            .cmds
            .init_state
            .run_cmd(|_cas_mismatch_metric| {
                // No cas_mismatch retries because we just use the returned
                // state on a mismatch.
                state_versions.maybe_init_shard(&shard_metrics)
            })
            .await?;
        Ok(Machine {
            cfg,
            metrics,
            shard_metrics,
            state_versions,
            state,
        })
    }

    pub fn shard_id(&self) -> ShardId {
        self.state.shard_id()
    }

    pub async fn fetch_upper(&mut self) -> &Antichain<T> {
        self.fetch_and_update_state().await;
        self.state.upper()
    }

    pub fn upper(&self) -> &Antichain<T> {
        self.state.upper()
    }

    pub fn seqno(&self) -> SeqNo {
        self.state.seqno()
    }

    pub(crate) fn state(&self) -> &State<K, V, T, D> {
        &self.state
    }

    #[cfg(test)]
    pub fn seqno_since(&self) -> SeqNo {
        self.state.seqno_since()
    }

    pub async fn add_rollup_for_current_seqno(&mut self) {
        let rollup_seqno = self.state.seqno;
        let rollup_key = PartialRollupKey::new(rollup_seqno, &RollupId::new());
        let () = self
            .state_versions
            .write_rollup_blob(&self.shard_metrics, &self.state, &rollup_key)
            .await;
        let applied = self
            .add_and_remove_rollups((rollup_seqno, &rollup_key), &[])
            .await;
        if !applied {
            // Someone else already wrote a rollup at this seqno, so ours didn't
            // get added. Delete it.
            self.state_versions
                .delete_rollup(&self.state.shard_id, &rollup_key)
                .await;
        }
    }

    pub async fn add_and_remove_rollups(
        &mut self,
        add_rollup: (SeqNo, &PartialRollupKey),
        remove_rollups: &[(SeqNo, PartialRollupKey)],
    ) -> bool {
        // See the big SUBTLE comment in [Self::merge_res] for what's going on
        // here.
        let mut applied_ever_true = false;
        let metrics = Arc::clone(&self.metrics);
        let (_seqno, _applied, _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.add_and_remove_rollups, |_, _, state| {
                let ret = state.add_and_remove_rollups(add_rollup, remove_rollups);
                if let Continue(applied) = ret {
                    applied_ever_true = applied_ever_true || applied;
                }
                ret
            })
            .await;
        applied_ever_true
    }

    pub async fn register_leased_reader(
        &mut self,
        reader_id: &LeasedReaderId,
        purpose: &str,
        lease_duration: Duration,
        heartbeat_timestamp_ms: u64,
    ) -> (Upper<T>, LeasedReaderState<T>) {
        let metrics = Arc::clone(&self.metrics);
        let (seqno, (shard_upper, read_cap), _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.register, |seqno, cfg, state| {
                state.register_leased_reader(
                    &cfg.hostname,
                    reader_id,
                    purpose,
                    seqno,
                    lease_duration,
                    heartbeat_timestamp_ms,
                )
            })
            .await;
        debug_assert_eq!(seqno, read_cap.seqno);
        (shard_upper, read_cap)
    }

    pub async fn register_critical_reader<O: Opaque + Codec64>(
        &mut self,
        reader_id: &CriticalReaderId,
        purpose: &str,
    ) -> CriticalReaderState<T> {
        let metrics = Arc::clone(&self.metrics);
        let (_seqno, state, _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.register, |_seqno, cfg, state| {
                state.register_critical_reader::<O>(&cfg.hostname, reader_id, purpose)
            })
            .await;
        state
    }

    pub async fn register_writer(
        &mut self,
        writer_id: &WriterId,
        purpose: &str,
        lease_duration: Duration,
        heartbeat_timestamp_ms: u64,
    ) -> (Upper<T>, WriterState<T>) {
        let metrics = Arc::clone(&self.metrics);
        let (_seqno, (shard_upper, writer_state), _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.register, |_seqno, cfg, state| {
                state.register_writer(
                    &cfg.hostname,
                    writer_id,
                    purpose,
                    lease_duration,
                    heartbeat_timestamp_ms,
                )
            })
            .await;
        (shard_upper, writer_state)
    }

    pub async fn clone_reader(
        &mut self,
        new_reader_id: &LeasedReaderId,
        purpose: &str,
        lease_duration: Duration,
        heartbeat_timestamp_ms: u64,
    ) -> LeasedReaderState<T> {
        let metrics = Arc::clone(&self.metrics);
        let (seqno, read_cap, _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.clone_reader, |seqno, cfg, state| {
                state.clone_reader(
                    &cfg.hostname,
                    new_reader_id,
                    purpose,
                    seqno,
                    lease_duration,
                    heartbeat_timestamp_ms,
                )
            })
            .await;
        debug_assert_eq!(seqno, read_cap.seqno);
        read_cap
    }

    pub async fn compare_and_append(
        &mut self,
        batch: &HollowBatch<T>,
        writer_id: &WriterId,
        heartbeat_timestamp_ms: u64,
    ) -> Result<Result<(SeqNo, WriterMaintenance<T>), InvalidUsage<T>>, Upper<T>> {
        let idempotency_token = IdempotencyToken::new();
        loop {
            let res = self
                .compare_and_append_idempotent(
                    batch,
                    writer_id,
                    heartbeat_timestamp_ms,
                    &idempotency_token,
                    None,
                )
                .await;
            match res {
                Ok(x) => return Ok(x),
                Err(_current_upper) => {
                    // If the state machine thinks that the shard upper is not
                    // far enough along, it could be because the caller of this
                    // method has found out that it advanced via some some
                    // side-channel that didn't update our local cache of the
                    // machine state. So, fetch the latest state and try again
                    // if we indeed get something different.
                    self.fetch_and_update_state().await;
                    let current_upper = self.upper();

                    // We tried to to a compare_and_append with the wrong
                    // expected upper, that won't work.
                    if current_upper != batch.desc.lower() {
                        return Err(Upper(current_upper.clone()));
                    } else {
                        // The upper stored in state was outdated. Retry after
                        // updating.
                    }
                }
            }
        }
    }

    async fn compare_and_append_idempotent(
        &mut self,
        batch: &HollowBatch<T>,
        writer_id: &WriterId,
        heartbeat_timestamp_ms: u64,
        idempotency_token: &IdempotencyToken,
        // Only exposed for testing. In prod, this always starts as None, but
        // making it a parameter allows us to simulate hitting an indeterminate
        // error on the first attempt in tests.
        mut indeterminate: Option<Indeterminate>,
    ) -> Result<Result<(SeqNo, WriterMaintenance<T>), InvalidUsage<T>>, Upper<T>> {
        let metrics = Arc::clone(&self.metrics);
        // SUBTLE: Retries of compare_and_append with Indeterminate errors are
        // tricky (more discussion of this in #12797):
        //
        // - (1) We compare_and_append and get an Indeterminate error back from
        //   CRDB/Consensus. This means we don't know if it committed or not.
        // - (2) We retry it.
        // - (3) We get back an upper mismatch. The tricky bit is deciding if we
        //   conflicted with some other writer OR if the write in (1) actually
        //   went through and we're "conflicting" with ourself.
        //
        // A number of scenarios can be distinguished with per-writer
        // idempotency tokens, so I'll jump straight to the hardest one:
        //
        // - (1) A compare_and_append is issued for e.g. `[5,7)`, the consensus
        //   call makes it onto the network before the operation is cancelled
        //   (by dropping the future).
        // - (2) A compare_and_append is issued from the same WriteHandle for
        //   `[3,5)`, it uses a different conn from the consensus pool and gets
        //   an Indeterminate error.
        // - (3) The call in (1) is received by consensus and commits.
        // - (4) The retry of (2) receives an upper mismatch with an upper of 7.
        //
        // At this point, how do we determine whether (2) committed or not and
        // thus whether we should return success or upper mismatch? Getting this
        // right is very important for correctness (imagine this is a table
        // write and we either return success or failure to the client).
        //
        // - If we use per-writer IdempotencyTokens but only store the latest
        //   one in state, then the `[5,7)` one will have clobbered whatever our
        //   `[3,5)` one was.
        // - We could store every IdempotencyToken that ever committed, but that
        //   would require unbounded storage in state (non-starter).
        // - We could require that IdempotencyTokens are comparable and that
        //   each call issued by a WriteHandle uses one that is strictly greater
        //   than every call before it. A previous version of this PR tried this
        //   and it's remarkably subtle. As a result, I (Dan) have developed
        //   strong feels that our correctness protocol _should not depend on
        //   WriteHandle, only Machine_.
        // - We could require a new WriterId if a request is ever cancelled by
        //   making `compare_and_append` take ownership of `self` and then
        //   handing it back for any call polled to completion. The ergonomics
        //   of this are quite awkward and, like the previous idea, it depends
        //   on the WriteHandle impl for correctness.
        // - Any ideas that involve reading back the data are foiled by a step
        //   `(0) set the since to 100` (plus the latency and memory usage would
        //   be too unpredictable).
        //
        // The technique used here derives from the following observations:
        //
        // - In practice, we don't use compare_and_append with the sort of
        //   "regressing frontiers" described above.
        // - In practice, Indeterminate errors are rare-ish. They happen enough
        //   that we don't want to always panic on them, but this is still a
        //   useful property to build on.
        //
        // At a high level, we do just enough to be able to distinguish the
        // cases that we think will happen in practice and then leave the rest
        // for a panic! that we think we'll never see. Concretely:
        //
        // - Run compare_and_append in a loop, retrying on Indeterminate errors
        //   but noting if we've ever done that.
        // - If we haven't seen an Indeterminate error (i.e. this is the first
        //   time though the loop) then the result we got is guaranteed to be
        //   correct, so pass it up.
        // - Otherwise, any result other than an expected upper mismatch is
        //   guaranteed to be correct, so just pass it up.
        // - Otherwise examine the writer's most recent upper and break it into
        //   two cases:
        // - Case 1 `expected_upper.less_than(writer_most_recent_upper)`: it's
        //   impossible that we committed on a previous iteration because the
        //   overall upper of the shard is less_than what this call would have
        //   advanced it to. Pass up the expectation mismatch.
        // - Case 2 `!Case1`: First note that this means our IdempotencyToken
        //   didn't match, otherwise we would have gotten `AlreadyCommitted`. It
        //   also means some previous write from _this writer_ has committed an
        //   upper that is beyond the one in this call, which is a weird usage
        //   (NB can't be a future write because that would mean someone is
        //   still polling us, but `&mut self` prevents that).
        //
        // TODO: If this technique works in practice (leads to zero panics),
        // then commit to it and remove the Indeterminate from
        // [WriteHandle::compare_and_append_batch].
        let mut retry = self
            .metrics
            .retries
            .compare_and_append_idempotent
            .stream(Retry::persist_defaults(SystemTime::now()).into_retry_stream());
        loop {
            let cmd_res = self
                .apply_unbatched_cmd(&metrics.cmds.compare_and_append, |_, _, state| {
                    state.compare_and_append(
                        batch,
                        writer_id,
                        heartbeat_timestamp_ms,
                        idempotency_token,
                    )
                })
                .await;
            let (seqno, res, routine) = match cmd_res {
                Ok(x) => x,
                Err(err) => {
                    // These are rare and interesting enough that we always log
                    // them at info!.
                    info!(
                        "compare_and_append received an indeterminate error, retrying in {:?}: {}",
                        retry.next_sleep(),
                        err
                    );
                    if indeterminate.is_none() {
                        indeterminate = Some(err);
                    }
                    retry = retry.sleep().await;
                    continue;
                }
            };
            match res {
                Ok(merge_reqs) => {
                    // We got explicit confirmation that we succeeded, so
                    // anything that happened in a previous retry is irrelevant.
                    let mut compact_reqs = Vec::with_capacity(merge_reqs.len());
                    for req in merge_reqs {
                        let req = CompactReq {
                            shard_id: self.shard_id(),
                            desc: req.desc,
                            inputs: req.inputs,
                        };
                        compact_reqs.push(req);
                    }
                    let writer_maintenance = WriterMaintenance {
                        routine,
                        compaction: compact_reqs,
                    };
                    return Ok(Ok((seqno, writer_maintenance)));
                }
                Err(CompareAndAppendBreak::AlreadyCommitted) => {
                    // A previous iteration through this loop got an
                    // Indeterminate error but was successful. Sanity check this
                    // and pass along the good news.
                    assert!(indeterminate.is_some());
                    self.metrics.cmds.compare_and_append_noop.inc();
                    return Ok(Ok((seqno, WriterMaintenance::default())));
                }
                Err(CompareAndAppendBreak::InvalidUsage(err)) => {
                    // InvalidUsage is (or should be) a deterministic function
                    // of the inputs and independent of anything in persist
                    // state. It's handed back via a Break, so we never even try
                    // to commit it. No network, no Indeterminate.
                    assert!(indeterminate.is_none());
                    return Ok(Err(err));
                }
                Err(CompareAndAppendBreak::Upper {
                    shard_upper,
                    writer_upper,
                }) => {
                    // NB the below intentionally compares to writer_upper
                    // (because it gives a tighter bound on the bad case), but
                    // returns shard_upper (what the persist caller cares
                    // about).
                    assert!(
                        PartialOrder::less_equal(&writer_upper, &shard_upper),
                        "{:?} vs {:?}",
                        &writer_upper,
                        &shard_upper
                    );
                    if PartialOrder::less_than(&writer_upper, batch.desc.upper()) {
                        // No way this could have committed in some previous
                        // attempt of this loop: the upper of the writer is
                        // strictly less than the proposed new upper.
                        return Err(Upper(shard_upper));
                    }
                    if indeterminate.is_none() {
                        // No way this could have committed in some previous
                        // attempt of this loop: we never saw an indeterminate
                        // error (thus there was no previous iteration of the
                        // loop).
                        return Err(Upper(shard_upper));
                    }
                    // This is the bad case. We can't distinguish if some
                    // previous attempt that got an Indeterminate error
                    // succeeded or not. This should be sufficiently rare in
                    // practice (hopefully ~never) that we give up and let
                    // process restart fix things. See the big comment above for
                    // more context.
                    //
                    // NB: This is intentionally not a halt! because it's quite
                    // unexpected.
                    panic!(concat!(
                        "cannot distinguish compare_and_append success or failure ",
                        "caa_lower={:?} caa_upper={:?} writer_upper={:?} shard_upper={:?} err={:?}"),
                        batch.desc.lower().elements(), batch.desc.upper().elements(),
                        writer_upper.elements(), shard_upper.elements(), indeterminate,
                    );
                }
            };
        }
    }

    pub async fn merge_res(&mut self, res: &FueledMergeRes<T>) -> ApplyMergeResult {
        let metrics = Arc::clone(&self.metrics);

        // SUBTLE! If Machine::merge_res returns false, the blobs referenced in
        // compaction output are deleted so we don't leak them. Naively passing
        // back the value returned by State::apply_merge_res might give a false
        // negative in the presence of retries and Indeterminate errors.
        // Specifically, something like the following:
        //
        // - We try to apply_merge_res, it matches.
        // - When apply_unbatched_cmd goes to commit the new state, the
        //   Consensus::compare_and_set returns an Indeterminate error (but
        //   actually succeeds). The committed State now contains references to
        //   the compaction output blobs.
        // - Machine::apply_unbatched_idempotent_cmd retries the Indeterminate
        //   error. For whatever reason, this time though it doesn't match
        //   (maybe the batches simply get grouped difference when deserialized
        //   from state, or more unavoidably perhaps another compaction
        //   happens).
        // - This now bubbles up applied=false to the caller, which uses it as a
        //   signal that the blobs in the compaction output should be deleted so
        //   that we don't leak them.
        // - We now contain references in committed State to blobs that don't
        //   exist.
        //
        // The fix is to keep track of whether applied ever was true, even for a
        // compare_and_set that returned an Indeterminate error. This has the
        // chance of false positive (leaking a blob) but that's better than a
        // false negative (a blob we can never recover referenced by state). We
        // anyway need a mechanism to clean up leaked blobs because of process
        // crashes.
        let mut merge_result_ever_applied = ApplyMergeResult::NotAppliedNoMatch;
        let (_seqno, _apply_merge_result, _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.merge_res, |_, _, state| {
                let ret = state.apply_merge_res(res);
                if let Continue(result) = ret {
                    // record if we've ever applied the merge
                    if result.applied() {
                        merge_result_ever_applied = result;
                    }
                    // otherwise record the most granular reason for _not_
                    // applying the merge when there was a matching batch
                    if result.matched() && !result.applied() && !merge_result_ever_applied.applied()
                    {
                        merge_result_ever_applied = result;
                    }
                }
                ret
            })
            .await;
        merge_result_ever_applied
    }

    pub async fn downgrade_since(
        &mut self,
        reader_id: &LeasedReaderId,
        outstanding_seqno: Option<SeqNo>,
        new_since: &Antichain<T>,
        heartbeat_timestamp_ms: u64,
    ) -> (SeqNo, Since<T>, RoutineMaintenance) {
        let metrics = Arc::clone(&self.metrics);
        self.apply_unbatched_idempotent_cmd(&metrics.cmds.downgrade_since, |seqno, _cfg, state| {
            state.downgrade_since(
                reader_id,
                seqno,
                outstanding_seqno,
                new_since,
                heartbeat_timestamp_ms,
            )
        })
        .await
    }

    pub async fn compare_and_downgrade_since<O: Opaque + Codec64>(
        &mut self,
        reader_id: &CriticalReaderId,
        expected_opaque: &O,
        (new_opaque, new_since): (&O, &Antichain<T>),
    ) -> (Result<Since<T>, (O, Since<T>)>, RoutineMaintenance) {
        let metrics = Arc::clone(&self.metrics);
        let (_seqno, res, maintenance) = self
            .apply_unbatched_idempotent_cmd(
                &metrics.cmds.compare_and_downgrade_since,
                |_seqno, _cfg, state| {
                    state.compare_and_downgrade_since::<O>(
                        reader_id,
                        expected_opaque,
                        (new_opaque, new_since),
                    )
                },
            )
            .await;

        match res {
            Ok(since) => (Ok(since), maintenance),
            Err((opaque, since)) => (Err((opaque, since)), maintenance),
        }
    }

    pub async fn heartbeat_leased_reader(
        &mut self,
        reader_id: &LeasedReaderId,
        heartbeat_timestamp_ms: u64,
    ) -> (SeqNo, bool, RoutineMaintenance) {
        let metrics = Arc::clone(&self.metrics);
        let (seqno, existed, maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.heartbeat_reader, |_, _, state| {
                state.heartbeat_leased_reader(reader_id, heartbeat_timestamp_ms)
            })
            .await;
        (seqno, existed, maintenance)
    }

    pub async fn start_reader_heartbeat_task(self, reader_id: LeasedReaderId) -> JoinHandle<()> {
        let mut machine = self;
        spawn(|| "persist::heartbeat_read", async move {
            let sleep_duration = machine.cfg.reader_lease_duration / 2;
            loop {
                let before_sleep = Instant::now();
                tokio::time::sleep(sleep_duration).await;

                let elapsed_since_before_sleeping = before_sleep.elapsed();
                if elapsed_since_before_sleeping > sleep_duration + Duration::from_secs(60) {
                    warn!(
                        "reader ({}) of shard ({}) went {}s between heartbeats",
                        reader_id,
                        machine.shard_id(),
                        elapsed_since_before_sleeping.as_secs_f64()
                    );
                }

                let before_heartbeat = Instant::now();
                let (_seqno, existed, _maintenance) = machine
                    .heartbeat_leased_reader(&reader_id, (machine.cfg.now)())
                    .await;

                let elapsed_since_heartbeat = before_heartbeat.elapsed();
                if elapsed_since_heartbeat > Duration::from_secs(60) {
                    warn!(
                        "reader ({}) of shard ({}) heartbeat call took {}s",
                        reader_id,
                        machine.shard_id(),
                        elapsed_since_heartbeat.as_secs_f64(),
                    );
                }

                if !existed {
                    return;
                }
            }
        })
    }

    pub async fn heartbeat_writer(
        &mut self,
        writer_id: &WriterId,
        heartbeat_timestamp_ms: u64,
    ) -> (SeqNo, bool, RoutineMaintenance) {
        let metrics = Arc::clone(&self.metrics);
        let (seqno, existed, maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.heartbeat_writer, |_, _, state| {
                state.heartbeat_writer(writer_id, heartbeat_timestamp_ms)
            })
            .await;
        (seqno, existed, maintenance)
    }

    pub async fn start_writer_heartbeat_task(self, writer_id: WriterId) -> JoinHandle<()> {
        let mut machine = self;
        spawn(|| "persist::heartbeat_write", async move {
            let sleep_duration = machine.cfg.writer_lease_duration / 4;
            loop {
                let before_sleep = Instant::now();
                tokio::time::sleep(sleep_duration).await;

                let elapsed_since_before_sleeping = before_sleep.elapsed();
                if elapsed_since_before_sleeping > sleep_duration + Duration::from_secs(60) {
                    warn!(
                        "writer ({}) of shard ({}) went {}s between heartbeats",
                        writer_id,
                        machine.shard_id(),
                        elapsed_since_before_sleeping.as_secs_f64()
                    );
                }

                let before_heartbeat = Instant::now();
                let (_seqno, existed, _maintenance) = machine
                    .heartbeat_writer(&writer_id, (machine.cfg.now)())
                    .await;

                let elapsed_since_heartbeat = before_heartbeat.elapsed();
                if elapsed_since_heartbeat > Duration::from_secs(60) {
                    warn!(
                        "writer ({}) of shard ({}) heartbeat call took {}s",
                        writer_id,
                        machine.shard_id(),
                        elapsed_since_heartbeat.as_secs_f64(),
                    );
                }

                if !existed {
                    return;
                }
            }
        })
    }

    pub async fn expire_leased_reader(&mut self, reader_id: &LeasedReaderId) -> SeqNo {
        let metrics = Arc::clone(&self.metrics);
        let (seqno, _existed, _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.expire_reader, |_, _, state| {
                state.expire_leased_reader(reader_id)
            })
            .await;
        seqno
    }

    pub async fn expire_critical_reader(&mut self, reader_id: &CriticalReaderId) -> SeqNo {
        let metrics = Arc::clone(&self.metrics);
        let (seqno, _existed, _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.expire_reader, |_, _, state| {
                state.expire_critical_reader(reader_id)
            })
            .await;
        seqno
    }

    pub async fn expire_writer(&mut self, writer_id: &WriterId) -> SeqNo {
        let metrics = Arc::clone(&self.metrics);
        let (seqno, _existed, _maintenance) = self
            .apply_unbatched_idempotent_cmd(&metrics.cmds.expire_writer, |_, _, state| {
                state.expire_writer(writer_id)
            })
            .await;
        seqno
    }

    pub async fn snapshot(
        &mut self,
        as_of: &Antichain<T>,
    ) -> Result<Vec<HollowBatch<T>>, Since<T>> {
        let mut retry: Option<MetricsRetryStream> = None;
        loop {
            let upper = match self.state.snapshot(as_of) {
                Ok(Ok(x)) => return Ok(x),
                Ok(Err(Upper(upper))) => {
                    // The upper isn't ready yet, fall through and try again.
                    upper
                }
                Err(Since(since)) => return Err(Since(since)),
            };
            // Only sleep after the first fetch, because the first time through
            // maybe our state was just out of date.
            retry = Some(match retry.take() {
                None => self
                    .metrics
                    .retries
                    .snapshot
                    .stream(Retry::persist_defaults(SystemTime::now()).into_retry_stream()),
                Some(retry) => {
                    // Use a duration based threshold here instead of the usual
                    // INFO_MIN_ATTEMPTS because here we're waiting on an
                    // external thing to arrive.
                    if retry.next_sleep() >= Duration::from_millis(64) {
                        info!(
                            "snapshot {} as of {:?} not yet available for upper {:?} retrying in {:?}",
                            self.shard_id(),
                            as_of,
                            upper,
                            retry.next_sleep()
                        );
                    } else {
                        debug!(
                            "snapshot {} as of {:?} not yet available for upper {:?} retrying in {:?}",
                            self.shard_id(),
                            as_of,
                            upper,
                            retry.next_sleep()
                        );
                    }
                    retry.sleep().await
                }
            });
            self.fetch_and_update_state().await;
        }
    }

    // NB: Unlike the other methods here, this one is read-only.
    pub fn verify_listen(&self, as_of: &Antichain<T>) -> Result<(), Since<T>> {
        match self.state.verify_listen(as_of) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(Upper(_))) => {
                // The upper may not be ready yet (maybe it would be ready if we
                // re-fetched state), but that's okay! One way to think of
                // Listen is as an async stream where creating the stream at any
                // legal as_of does not block but then updates trickle in once
                // they are available.
                Ok(())
            }
            Err(Since(since)) => Err(Since(since)),
        }
    }

    pub fn next_listen_batch(&self, frontier: &Antichain<T>) -> Option<HollowBatch<T>> {
        self.state.next_listen_batch(frontier)
    }

    async fn apply_unbatched_idempotent_cmd<
        R,
        WorkFn: FnMut(SeqNo, &PersistConfig, &mut StateCollections<T>) -> ControlFlow<Infallible, R>,
    >(
        &mut self,
        cmd: &CmdMetrics,
        mut work_fn: WorkFn,
    ) -> (SeqNo, R, RoutineMaintenance) {
        let mut retry = self
            .metrics
            .retries
            .idempotent_cmd
            .stream(Retry::persist_defaults(SystemTime::now()).into_retry_stream());
        loop {
            match self.apply_unbatched_cmd(cmd, &mut work_fn).await {
                Ok((seqno, x, maintenance)) => match x {
                    Ok(x) => return (seqno, x, maintenance),
                    Err(infallible) => match infallible {},
                },
                Err(err) => {
                    if retry.attempt() >= INFO_MIN_ATTEMPTS {
                        info!("apply_unbatched_idempotent_cmd {} received an indeterminate error, retrying in {:?}: {}", cmd.name, retry.next_sleep(), err);
                    } else {
                        debug!("apply_unbatched_idempotent_cmd {} received an indeterminate error, retrying in {:?}: {}", cmd.name, retry.next_sleep(), err);
                    }
                    retry = retry.sleep().await;
                    continue;
                }
            }
        }
    }

    async fn apply_unbatched_cmd<
        R,
        E,
        WorkFn: FnMut(SeqNo, &PersistConfig, &mut StateCollections<T>) -> ControlFlow<E, R>,
    >(
        &mut self,
        cmd: &CmdMetrics,
        mut work_fn: WorkFn,
    ) -> Result<(SeqNo, Result<R, E>, RoutineMaintenance), Indeterminate> {
        let is_write = cmd.name == self.metrics.cmds.compare_and_append.name;
        cmd.run_cmd(|cas_mismatch_metric| async move {
            let mut garbage_collection;

            loop {
                let (work_ret, mut new_state) = match self
                    .state
                    .clone_apply(&self.cfg, &mut work_fn)
                {
                    Continue(x) => x,
                    Break(err) => {
                        return Ok((self.state.seqno(), Err(err), RoutineMaintenance::default()))
                    }
                };

                // Find out if this command has been selected to perform gc, so
                // that it will fire off a background request to the
                // GarbageCollector to delete eligible blobs and truncate the
                // state history. This is dependant both on `maybe_gc` returning
                // Some _and_ on this state being successfully compare_and_set.
                //
                // NB: Make sure this overwrites `garbage_collection` on every
                // run though the loop (i.e. no `if let Some` here). When we
                // lose a CaS race, we might discover that the winner got
                // assigned the gc.
                garbage_collection = new_state.maybe_gc(is_write);

                // NB: Make sure this is the very last thing before the
                // `try_compare_and_set_current` call. (In particular, it needs
                // to come after anything that might modify new_state, such as
                // `maybe_gc`.)
                let diff = StateDiff::from_diff(&self.state, &new_state);
                // Sanity check that our diff logic roundtrips and adds back up
                // correctly.
                #[cfg(any(test, debug_assertions))]
                {
                    if let Err(err) =
                        StateDiff::validate_roundtrip(&self.metrics, &self.state, &diff, &new_state)
                    {
                        panic!("validate_roundtrips failed: {}", err);
                    }
                }

                // SUBTLE! Unlike the other consensus and blob uses, we can't
                // automatically retry indeterminate ExternalErrors here. However,
                // if the state change itself is _idempotent_, then we're free to
                // retry even indeterminate errors. See
                // [Self::apply_unbatched_idempotent_cmd].
                let expected = self.state.seqno();
                let cas_res = self
                    .state_versions
                    .try_compare_and_set_current(
                        &cmd.name,
                        &self.shard_metrics,
                        Some(expected),
                        &new_state,
                        &diff,
                    )
                    .await?;
                match cas_res {
                    Ok(()) => {
                        assert!(
                            self.state.seqno <= new_state.seqno,
                            "state seqno regressed: {} vs {}",
                            self.state.seqno,
                            new_state.seqno
                        );
                        self.state = new_state;

                        let (expired_readers, expired_writers) =
                            self.state.handles_needing_expiration((self.cfg.now)());
                        self.metrics
                            .lease
                            .timeout_read
                            .inc_by(u64::cast_from(expired_readers.len()));
                        let lease_expiration = Some(LeaseExpiration {
                            readers: expired_readers,
                            writers: expired_writers,
                        });

                        if let Some(gc) = garbage_collection.as_ref() {
                            debug!("Assigned gc request: {:?}", gc);
                        }

                        let maintenance = RoutineMaintenance {
                            garbage_collection,
                            lease_expiration,
                            write_rollup: self.state.need_rollup(),
                        };

                        return Ok((self.state.seqno(), Ok(work_ret), maintenance));
                    }
                    Err(diffs_to_current) => {
                        cas_mismatch_metric.0.inc();

                        let seqno_before = self.state.seqno;
                        let diffs_apply = diffs_to_current
                            .first()
                            .map_or(true, |x| x.seqno == seqno_before.next());
                        if diffs_apply {
                            self.metrics.state.update_state_fast_path.inc();
                            self.state.apply_encoded_diffs(
                                &self.cfg,
                                &self.metrics,
                                &diffs_to_current,
                            )
                        } else {
                            // Otherwise, we've gc'd the diffs we'd need to
                            // advance self.state to where diffs_to_current
                            // starts so we need a new rollup.
                            self.metrics.state.update_state_slow_path.inc();
                            debug!(
                                "update_state didn't hit update_state fast path {} {:?}",
                                self.state.seqno,
                                diffs_to_current.first().map(|x| x.seqno),
                            );

                            // SUBTLE: Consensus::compare_and_set guarantees
                            // that we get back everything in
                            // `[max(expected.next(),earliest),current]` on
                            // expectation mismatch. If `diffs_apply` is false
                            // (this branch), then we know `earliest >
                            // expected.next()` and so `diffs_to_current` is
                            // already the full set of all live diffs. Sanity
                            // check this deduction with an assert and then use
                            // it to `fetch_current_state`.
                            assert!(
                                diffs_to_current
                                    .first()
                                    .map_or(false, |x| x.seqno > expected.next()),
                                "{:?} vs {}",
                                diffs_to_current.first(),
                                expected.next()
                            );
                            let all_live_diffs = diffs_to_current;
                            self.state = self
                                .state_versions
                                .fetch_current_state(&self.state.shard_id, all_live_diffs)
                                .await
                                .expect("shard codecs should not change");
                        }

                        // Intentionally don't backoff here. It would only make
                        // starvation issues even worse.
                        continue;
                    }
                }
            }
        })
        .await
    }

    pub async fn fetch_and_update_state(&mut self) {
        let seqno_before = self.state.seqno;
        self.state_versions
            .fetch_and_update_to_current(&mut self.state)
            .await
            .expect("shard codecs should not change");
        assert!(
            seqno_before <= self.state.seqno,
            "state seqno regressed: {} vs {}",
            seqno_before,
            self.state.seqno
        );
    }
}

pub const INFO_MIN_ATTEMPTS: usize = 3;

pub async fn retry_external<R, F, WorkFn>(metrics: &RetryMetrics, mut work_fn: WorkFn) -> R
where
    F: std::future::Future<Output = Result<R, ExternalError>>,
    WorkFn: FnMut() -> F,
{
    let mut retry = metrics.stream(Retry::persist_defaults(SystemTime::now()).into_retry_stream());
    loop {
        match work_fn().await {
            Ok(x) => {
                if retry.attempt() > 0 {
                    debug!(
                        "external operation {} succeeded after failing at least once",
                        metrics.name,
                    );
                }
                return x;
            }
            Err(err) => {
                if retry.attempt() >= INFO_MIN_ATTEMPTS {
                    info!(
                        "external operation {} failed, retrying in {:?}: {:#}",
                        metrics.name,
                        retry.next_sleep(),
                        err
                    );
                } else {
                    debug!(
                        "external operation {} failed, retrying in {:?}: {:#}",
                        metrics.name,
                        retry.next_sleep(),
                        err
                    );
                }
                retry = retry.sleep().await;
            }
        }
    }
}

pub async fn retry_determinate<R, F, WorkFn>(
    metrics: &RetryMetrics,
    mut work_fn: WorkFn,
) -> Result<R, Indeterminate>
where
    F: std::future::Future<Output = Result<R, ExternalError>>,
    WorkFn: FnMut() -> F,
{
    let mut retry = metrics.stream(Retry::persist_defaults(SystemTime::now()).into_retry_stream());
    loop {
        match work_fn().await {
            Ok(x) => {
                if retry.attempt() > 0 {
                    debug!(
                        "external operation {} succeeded after failing at least once",
                        metrics.name,
                    );
                }
                return Ok(x);
            }
            Err(ExternalError::Determinate(err)) => {
                // The determinate "could not serialize access" errors
                // happen often enough in dev (which uses Postgres) that
                // it's impeding people's work. At the same time, it's been
                // a source of confusion for eng. The situation is much
                // better on CRDB and we have metrics coverage in prod, so
                // this is redundant enough that it's more hurtful than
                // helpful. As a result, this intentionally ignores
                // INFO_MIN_ATTEMPTS and always logs at debug.
                debug!(
                    "external operation {} failed, retrying in {:?}: {:#}",
                    metrics.name,
                    retry.next_sleep(),
                    err
                );
                retry = retry.sleep().await;
                continue;
            }
            Err(ExternalError::Indeterminate(x)) => return Err(x),
        }
    }
}

#[cfg(test)]
pub mod datadriven {
    use std::collections::HashMap;
    use std::sync::Arc;

    use anyhow::anyhow;
    use differential_dataflow::consolidation::consolidate_updates;
    use differential_dataflow::trace::Description;
    use mz_build_info::DUMMY_BUILD_INFO;

    use crate::batch::{validate_truncate_batch, BatchBuilder};
    use crate::fetch::fetch_batch_part;
    use crate::internal::compact::{CompactReq, Compactor};
    use crate::internal::datadriven::DirectiveArgs;
    use crate::internal::gc::GcReq;
    use crate::internal::paths::{BlobKey, BlobKeyPrefix, PartialBlobKey};
    use crate::read::{Listen, ListenEvent};
    use crate::tests::new_test_client;
    use crate::{GarbageCollector, PersistClient};

    use super::*;

    /// Shared state for a single [crate::internal::machine] [datadriven::TestFile].
    #[derive(Debug)]
    pub struct MachineState {
        pub client: PersistClient,
        pub shard_id: ShardId,
        pub state_versions: Arc<StateVersions>,
        pub machine: Machine<String, (), u64, i64>,
        pub gc: GarbageCollector<String, (), u64, i64>,
        pub batches: HashMap<String, HollowBatch<u64>>,
        pub listens: HashMap<String, Listen<String, (), u64, i64>>,
        pub routine: Vec<RoutineMaintenance>,
    }

    impl MachineState {
        pub async fn new() -> Self {
            let shard_id = ShardId::new();
            let mut client = new_test_client().await;
            // Reset blob_target_size. Individual batch writes and compactions
            // can override it with an arg.
            client.cfg.blob_target_size =
                PersistConfig::new(&DUMMY_BUILD_INFO, client.cfg.now.clone()).blob_target_size;
            let state_versions = Arc::new(StateVersions::new(
                client.cfg.clone(),
                Arc::clone(&client.consensus),
                Arc::clone(&client.blob),
                Arc::clone(&client.metrics),
            ));
            let machine = Machine::new(
                client.cfg.clone(),
                shard_id,
                Arc::clone(&client.metrics),
                Arc::clone(&state_versions),
            )
            .await
            .expect("codecs should match");
            let gc = GarbageCollector::new(machine.clone());
            MachineState {
                shard_id,
                client,
                state_versions,
                machine,
                gc,
                batches: HashMap::default(),
                listens: HashMap::default(),
                routine: Vec::new(),
            }
        }
    }

    /// Scans consensus and returns all states with their SeqNos
    /// and which batches they reference
    pub async fn consensus_scan(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let from = args.expect("from_seqno");

        let mut states = datadriven
            .state_versions
            .fetch_all_live_states::<String, (), u64, i64>(&datadriven.shard_id)
            .await
            .expect("shard codecs should not change");
        let mut s = String::new();
        while let Some(x) = states.next() {
            if x.seqno < from {
                continue;
            }
            let mut batches = vec![];
            x.collections.trace.map_batches(|b| {
                for (batch_name, original_batch) in &datadriven.batches {
                    if original_batch.parts == b.parts {
                        batches.push(batch_name.to_owned());
                        break;
                    }
                }
            });
            write!(s, "seqno={} batches={}\n", x.seqno, batches.join(","));
        }
        Ok(s)
    }

    pub async fn blob_scan_batches(
        datadriven: &mut MachineState,
        _args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let key_prefix = BlobKeyPrefix::Shard(&datadriven.shard_id).to_string();

        let mut s = String::new();
        let () = datadriven
            .state_versions
            .blob
            .list_keys_and_metadata(&key_prefix, &mut |x| {
                let (_, key) = BlobKey::parse_ids(x.key).expect("key should be valid");
                if let PartialBlobKey::Batch(_, _) = key {
                    write!(s, "{}: {}b\n", x.key, x.size_in_bytes);
                }
            })
            .await?;
        Ok(s)
    }

    #[allow(clippy::unused_async)]
    pub async fn shard_desc(
        datadriven: &mut MachineState,
        _args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        Ok(format!(
            "since={:?} upper={:?}\n",
            datadriven.machine.state.since().elements(),
            datadriven.machine.state.upper().elements()
        ))
    }

    pub async fn downgrade_since(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let since = args.expect_antichain("since");
        let reader_id = args.expect("reader_id");
        let (_, since, routine) = datadriven
            .machine
            .downgrade_since(&reader_id, None, &since, (datadriven.machine.cfg.now)())
            .await;
        datadriven.routine.push(routine);
        Ok(format!(
            "{} {:?}\n",
            datadriven.machine.seqno(),
            since.0.elements()
        ))
    }

    pub async fn compare_and_downgrade_since(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let expected_opaque: u64 = args.expect("expect_opaque");
        let new_opaque: u64 = args.expect("opaque");
        let new_since = args.expect_antichain("since");
        let reader_id = args.expect("reader_id");
        let (res, routine) = datadriven
            .machine
            .compare_and_downgrade_since(&reader_id, &expected_opaque, (&new_opaque, &new_since))
            .await;
        datadriven.routine.push(routine);
        let since = res.map_err(|(opaque, since)| {
            anyhow!("mismatch: opaque={} since={:?}", opaque, since.0.elements())
        })?;
        Ok(format!(
            "{} {} {:?}\n",
            datadriven.machine.seqno(),
            new_opaque,
            since.0.elements()
        ))
    }

    pub async fn write_batch(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let output = args.expect_str("output");
        let lower = args.expect_antichain("lower");
        let upper = args.expect_antichain("upper");
        let target_size = args.optional("target_size");
        let parts_size_override = args.optional("parts_size_override");
        let updates = args.input.split('\n').flat_map(DirectiveArgs::parse_update);

        let mut cfg = datadriven.client.cfg.clone();
        if let Some(target_size) = target_size {
            cfg.blob_target_size = target_size;
        };
        let mut builder = BatchBuilder::new(
            cfg,
            Arc::clone(&datadriven.client.metrics),
            0,
            lower,
            Arc::clone(&datadriven.client.blob),
            Arc::clone(&datadriven.client.cpu_heavy_runtime),
            datadriven.shard_id.clone(),
            WriterId::new(),
        );
        for ((k, ()), t, d) in updates {
            builder.add(&k, &(), &t, &d).await.expect("invalid batch");
        }
        let batch = builder
            .finish(upper)
            .await
            .expect("invalid batch")
            .into_hollow_batch();

        if let Some(size) = parts_size_override {
            let mut batch = batch.clone();
            for part in batch.parts.iter_mut() {
                part.encoded_size_bytes = size;
            }
            datadriven.batches.insert(output.to_owned(), batch);
        } else {
            datadriven.batches.insert(output.to_owned(), batch.clone());
        }
        Ok(format!("parts={} len={}\n", batch.parts.len(), batch.len))
    }

    pub async fn fetch_batch(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let input = args.expect_str("input");
        let batch = datadriven.batches.get(input).expect("unknown batch");

        let mut s = String::new();
        for (idx, part) in batch.parts.iter().enumerate() {
            write!(s, "<part {idx}>\n");
            let blob_batch = datadriven
                .client
                .blob
                .get(&part.key.complete(&datadriven.shard_id))
                .await;
            match blob_batch {
                Ok(Some(_)) | Err(_) => {}
                // don't try to fetch/print the keys of the batch part
                // if the blob store no longer has it
                Ok(None) => {
                    s.push_str("<empty>\n");
                    continue;
                }
            };
            let mut part = fetch_batch_part(
                &datadriven.shard_id,
                datadriven.client.blob.as_ref(),
                datadriven.client.metrics.as_ref(),
                &datadriven.client.metrics.read.batch_fetcher,
                &part.key,
                &batch.desc,
            )
            .await
            .expect("invalid batch part");
            while let Some((k, _v, t, d)) = part.next() {
                let (k, d) = (String::decode(k).unwrap(), i64::decode(d));
                write!(s, "{k} {t} {d}\n");
            }
        }
        if !s.is_empty() {
            for (idx, run) in batch.runs().enumerate() {
                write!(s, "<run {idx}>\n");
                for part in run {
                    let part_idx = batch
                        .parts
                        .iter()
                        .position(|p| p == part)
                        .expect("part should exist");
                    write!(s, "part {part_idx}\n");
                }
            }
        }
        Ok(s)
    }

    #[allow(clippy::unused_async)]
    pub async fn truncate_batch_desc(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let input = args.expect_str("input");
        let output = args.expect_str("output");
        let lower = args.expect_antichain("lower");
        let upper = args.expect_antichain("upper");

        let mut batch = datadriven
            .batches
            .get(input)
            .expect("unknown batch")
            .clone();
        let truncated_desc = Description::new(lower, upper, batch.desc.since().clone());
        let () = validate_truncate_batch(&batch.desc, &truncated_desc)?;
        batch.desc = truncated_desc;
        datadriven.batches.insert(output.to_owned(), batch.clone());
        Ok(format!("parts={} len={}\n", batch.parts.len(), batch.len))
    }

    #[allow(clippy::unused_async)]
    pub async fn set_batch_parts_size(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let input = args.expect_str("input");
        let size = args.expect("size");
        let batch = datadriven.batches.get_mut(input).expect("unknown batch");
        for part in batch.parts.iter_mut() {
            part.encoded_size_bytes = size;
        }
        Ok("ok\n".to_string())
    }

    pub async fn compact(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let output = args.expect_str("output");
        let lower = args.expect_antichain("lower");
        let upper = args.expect_antichain("upper");
        let since = args.expect_antichain("since");
        let target_size = args.optional("target_size");
        let memory_bound = args.optional("memory_bound");

        let mut inputs = Vec::new();
        for input in args.args.get("inputs").expect("missing inputs") {
            inputs.push(
                datadriven
                    .batches
                    .get(input)
                    .expect("unknown batch")
                    .clone(),
            );
        }

        let mut cfg = datadriven.client.cfg.clone();
        if let Some(target_size) = target_size {
            cfg.blob_target_size = target_size;
        };
        if let Some(memory_bound) = memory_bound {
            cfg.compaction_memory_bound_bytes = memory_bound;
        }
        let req = CompactReq {
            shard_id: datadriven.shard_id,
            desc: Description::new(lower, upper, since),
            inputs,
        };
        let res = Compactor::<String, (), u64, i64>::compact(
            cfg,
            Arc::clone(&datadriven.client.blob),
            Arc::clone(&datadriven.client.metrics),
            Arc::clone(&datadriven.client.cpu_heavy_runtime),
            req,
            WriterId::new(),
        )
        .await?;

        datadriven
            .batches
            .insert(output.to_owned(), res.output.clone());
        Ok(format!(
            "parts={} len={}\n",
            res.output.parts.len(),
            res.output.len
        ))
    }

    pub async fn gc(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let new_seqno_since = args.expect("to_seqno");

        let req = GcReq {
            shard_id: datadriven.shard_id,
            new_seqno_since,
        };
        GarbageCollector::gc_and_truncate(&mut datadriven.machine, req).await;

        Ok(format!("{} ok\n", datadriven.machine.seqno()))
    }

    pub async fn snapshot(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let as_of = args.expect_antichain("as_of");
        let snapshot = datadriven
            .machine
            .snapshot(&as_of)
            .await
            .map_err(|err| anyhow!("{:?}", err))?;

        let mut updates = Vec::new();
        for batch in snapshot {
            for part in batch.parts {
                let mut part = fetch_batch_part(
                    &datadriven.shard_id,
                    datadriven.client.blob.as_ref(),
                    datadriven.client.metrics.as_ref(),
                    &datadriven.client.metrics.read.batch_fetcher,
                    &part.key,
                    &batch.desc,
                )
                .await
                .expect("invalid batch part");
                while let Some((k, _v, mut t, d)) = part.next() {
                    t.advance_by(as_of.borrow());
                    updates.push((String::decode(k).unwrap(), t, i64::decode(d)));
                }
            }
        }
        consolidate_updates(&mut updates);

        let mut s = String::new();
        for (k, t, d) in updates {
            write!(s, "{k} {t} {d}\n");
        }
        Ok(s)
    }

    pub async fn register_listen(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let output = args.expect_str("output");
        let as_of = args.expect_antichain("as_of");
        let read = datadriven
            .client
            .open_leased_reader::<String, (), u64, i64>(datadriven.shard_id, "")
            .await
            .expect("invalid shard types");
        let listen = read
            .listen(as_of)
            .await
            .map_err(|err| anyhow!("{:?}", err))?;
        datadriven.listens.insert(output.to_owned(), listen);
        Ok("ok\n".into())
    }

    pub async fn listen_through(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let input = args.expect_str("input");
        // It's not possible to listen _through_ the empty antichain, so this is
        // intentionally `expect` instead of `expect_antichain`.
        let frontier = args.expect("frontier");
        let listen = datadriven.listens.get_mut(input).expect("unknown listener");
        let mut s = String::new();
        loop {
            for event in listen.next().await {
                match event {
                    ListenEvent::Updates(x) => {
                        for ((k, _v), t, d) in x.iter() {
                            write!(s, "{} {} {}\n", k.as_ref().unwrap(), t, d);
                        }
                    }
                    ListenEvent::Progress(x) => {
                        if !x.less_than(&frontier) {
                            return Ok(s);
                        }
                    }
                }
            }
        }
    }

    pub async fn register_critical_reader(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let reader_id = args.expect("reader_id");
        let state = datadriven
            .machine
            .register_critical_reader::<u64>(&reader_id, "tests")
            .await;
        Ok(format!(
            "{} {:?}\n",
            datadriven.machine.seqno(),
            state.since.elements(),
        ))
    }

    pub async fn register_leased_reader(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let reader_id = args.expect("reader_id");
        let (_, state) = datadriven
            .machine
            .register_leased_reader(
                &reader_id,
                "tests",
                datadriven.client.cfg.reader_lease_duration,
                (datadriven.client.cfg.now)(),
            )
            .await;
        Ok(format!(
            "{} {:?}\n",
            datadriven.machine.seqno(),
            state.since.elements(),
        ))
    }

    pub async fn register_writer(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let writer_id = args.expect("writer_id");
        let (upper, _state) = datadriven
            .machine
            .register_writer(
                &writer_id,
                "tests",
                datadriven.client.cfg.writer_lease_duration,
                (datadriven.client.cfg.now)(),
            )
            .await;
        Ok(format!(
            "{} {:?}\n",
            datadriven.machine.seqno(),
            upper.0.elements()
        ))
    }

    pub async fn heartbeat_leased_reader(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let reader_id = args.expect("reader_id");
        let _ = datadriven
            .machine
            .heartbeat_leased_reader(&reader_id, (datadriven.client.cfg.now)())
            .await;
        Ok(format!("{} ok\n", datadriven.machine.seqno()))
    }

    pub async fn heartbeat_writer(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let writer_id = args.expect("writer_id");
        let _ = datadriven
            .machine
            .heartbeat_writer(&writer_id, (datadriven.client.cfg.now)())
            .await;
        Ok(format!("{} ok\n", datadriven.machine.seqno()))
    }

    pub async fn expire_critical_reader(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let reader_id = args.expect("reader_id");
        let _ = datadriven.machine.expire_critical_reader(&reader_id).await;
        Ok(format!("{} ok\n", datadriven.machine.seqno()))
    }

    pub async fn expire_leased_reader(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let reader_id = args.expect("reader_id");
        let _ = datadriven.machine.expire_leased_reader(&reader_id).await;
        Ok(format!("{} ok\n", datadriven.machine.seqno()))
    }

    pub async fn expire_writer(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let writer_id = args.expect("writer_id");
        let _ = datadriven.machine.expire_writer(&writer_id).await;
        Ok(format!("{} ok\n", datadriven.machine.seqno()))
    }

    pub async fn compare_and_append(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let input = args.expect_str("input");
        let writer_id = args.expect("writer_id");
        let batch = datadriven
            .batches
            .get(input)
            .expect("unknown batch")
            .clone();
        let token = args.optional("token").unwrap_or_else(IdempotencyToken::new);
        let indeterminate = args
            .optional::<String>("prev_indeterminate")
            .map(|x| Indeterminate::new(anyhow::Error::msg(x)));
        let now = (datadriven.client.cfg.now)();
        let (_, maintenance) = datadriven
            .machine
            .compare_and_append_idempotent(&batch, &writer_id, now, &token, indeterminate)
            .await
            .map_err(|err| anyhow!("{:?}", err))?
            .expect("invalid usage");
        // TODO: Don't throw away writer maintenance. It's slightly tricky
        // because we need a WriterId for Compactor.
        datadriven.routine.push(maintenance.routine);
        Ok(format!(
            "{} {:?}\n",
            datadriven.machine.seqno(),
            datadriven.machine.upper().elements(),
        ))
    }

    pub async fn apply_merge_res(
        datadriven: &mut MachineState,
        args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let input = args.expect_str("input");
        let batch = datadriven
            .batches
            .get(input)
            .expect("unknown batch")
            .clone();
        let merge_res = datadriven
            .machine
            .merge_res(&FueledMergeRes { output: batch })
            .await;
        Ok(format!(
            "{} {}\n",
            datadriven.machine.seqno(),
            merge_res.applied()
        ))
    }

    pub async fn perform_maintenance(
        datadriven: &mut MachineState,
        _args: DirectiveArgs<'_>,
    ) -> Result<String, anyhow::Error> {
        let mut s = String::new();
        for maintenance in datadriven.routine.drain(..) {
            let () = maintenance
                .perform(&datadriven.machine, &datadriven.gc)
                .await;
            let () = datadriven.machine.fetch_and_update_state().await;
            write!(s, "{} ok\n", datadriven.machine.seqno());
        }
        Ok(s)
    }
}

#[cfg(test)]
pub mod tests {
    use std::sync::Arc;

    use mz_ore::cast::CastFrom;
    use mz_ore::task::spawn;
    use mz_persist::intercept::{InterceptBlob, InterceptHandle};
    use mz_persist::location::{Blob, SeqNo};
    use timely::progress::Antichain;

    use crate::internal::gc::{GarbageCollector, GcReq};
    use crate::tests::new_test_client;
    use crate::ShardId;

    #[tokio::test(flavor = "multi_thread")]
    async fn apply_unbatched_cmd_truncate() {
        mz_ore::test::init_logging();

        let (mut write, _) = new_test_client()
            .await
            .expect_open::<String, (), u64, i64>(ShardId::new())
            .await;

        // Write a bunch of batches. This should result in a bounded number of
        // live entries in consensus.
        const NUM_BATCHES: u64 = 100;
        for idx in 0..NUM_BATCHES {
            let batch = write
                .expect_batch(&[((idx.to_string(), ()), idx, 1)], idx, idx + 1)
                .await;
            let (_, writer_maintenance) = write
                .machine
                .compare_and_append(
                    &batch.into_hollow_batch(),
                    &write.writer_id,
                    (write.cfg.now)(),
                )
                .await
                .expect("invalid usage")
                .expect("unexpected upper");
            writer_maintenance
                .perform(&write.machine, &write.gc, write.compact.as_ref())
                .await;
        }
        let live_diffs = write
            .machine
            .state_versions
            .fetch_all_live_diffs(&write.machine.shard_id())
            .await;
        // Make sure we constructed the key correctly.
        assert!(live_diffs.0.len() > 0);
        // Make sure the number of entries is bounded. (I think we could work
        // out a tighter bound than this, but the point is only that it's
        // bounded).
        let max_live_diffs = 2 * usize::cast_from(NUM_BATCHES.next_power_of_two().trailing_zeros());
        assert!(
            live_diffs.0.len() < max_live_diffs,
            "{} vs {}",
            live_diffs.0.len(),
            max_live_diffs
        );
    }

    // A regression test for #14719, where a bug in gc led to an incremental
    // state invariant being violated which resulted in gc being permanently
    // wedged for the shard.
    #[tokio::test(flavor = "multi_thread")]
    async fn regression_gc_skipped_req_and_interrupted() {
        let mut client = new_test_client().await;
        let intercept = InterceptHandle::default();
        client.blob = Arc::new(InterceptBlob::new(
            Arc::clone(&client.blob),
            intercept.clone(),
        )) as Arc<dyn Blob + Send + Sync>;
        let (_, mut read) = client
            .expect_open::<String, String, u64, i64>(ShardId::new())
            .await;

        // Create a new SeqNo
        read.downgrade_since(&Antichain::from_elem(1)).await;
        let new_seqno_since = read.machine.seqno();

        // Start a GC in the background for some SeqNo range that is not
        // contiguous compared to the last gc req (in this case, n/a) and then
        // crash when it gets to the blob deletes. In the regression case, this
        // renders the shard permanently un-gc-able.
        assert!(new_seqno_since > SeqNo::minimum());
        intercept.set_post_delete(Some(Arc::new(|_, _| panic!("boom"))));
        let mut machine = read.machine.clone();
        // Run this in a spawn so we can catch the boom panic
        let gc = spawn(|| "", async move {
            let req = GcReq {
                shard_id: machine.shard_id(),
                new_seqno_since,
            };
            GarbageCollector::gc_and_truncate(&mut machine, req).await
        });
        // Wait for gc to either panic (regression case) or finish (good case)
        // because it happens to not call blob delete.
        let _ = gc.await;

        // Allow blob deletes to go through and try GC again. In the regression
        // case, this hangs.
        intercept.set_post_delete(None);
        let req = GcReq {
            shard_id: read.machine.shard_id(),
            new_seqno_since,
        };
        let _ = GarbageCollector::gc_and_truncate(&mut read.machine, req.clone()).await;
    }
}
