// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A cache of [PersistClient]s indexed by [PersistLocation]s.

use std::any::Any;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::future::Future;
use std::sync::{Arc, RwLock, TryLockError, Weak};
use std::time::{Duration, Instant};

use differential_dataflow::difference::Semigroup;
use differential_dataflow::lattice::Lattice;
use mz_ore::metrics::MetricsRegistry;
use mz_persist::cfg::{BlobConfig, ConsensusConfig};
use mz_persist::location::{
    Blob, Consensus, ExternalError, BLOB_GET_LIVENESS_KEY, CONSENSUS_HEAD_LIVENESS_KEY,
};
use mz_persist_types::{Codec, Codec64};
use timely::progress::Timestamp;
use tokio::sync::{Mutex, OnceCell};
use tokio::task::JoinHandle;
use tracing::instrument;

use crate::async_runtime::CpuHeavyRuntime;
use crate::error::{CodecConcreteType, CodecMismatch};
use crate::internal::machine::retry_external;
use crate::internal::metrics::{LockMetrics, Metrics, MetricsBlob, MetricsConsensus};
use crate::internal::state::TypedState;
use crate::{PersistClient, PersistConfig, PersistLocation, ShardId};

/// A cache of [PersistClient]s indexed by [PersistLocation]s.
///
/// There should be at most one of these per process. All production
/// PersistClients should be created through this cache.
///
/// This is because, in production, persist is heavily limited by the number of
/// server-side Postgres/Aurora connections. This cache allows PersistClients to
/// share, for example, these Postgres connections.
#[derive(Debug)]
pub struct PersistClientCache {
    pub(crate) cfg: PersistConfig,
    pub(crate) metrics: Arc<Metrics>,
    blob_by_uri: Mutex<BTreeMap<String, (RttLatencyTask, Arc<dyn Blob + Send + Sync>)>>,
    consensus_by_uri: Mutex<BTreeMap<String, (RttLatencyTask, Arc<dyn Consensus + Send + Sync>)>>,
    cpu_heavy_runtime: Arc<CpuHeavyRuntime>,
    state_cache: Arc<StateCache>,
}

#[derive(Debug)]
struct RttLatencyTask(JoinHandle<()>);

impl Drop for RttLatencyTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl PersistClientCache {
    /// Returns a new [PersistClientCache].
    pub fn new(cfg: PersistConfig, registry: &MetricsRegistry) -> Self {
        let metrics = Metrics::new(&cfg, registry);
        PersistClientCache {
            cfg,
            metrics: Arc::new(metrics),
            blob_by_uri: Mutex::new(BTreeMap::new()),
            consensus_by_uri: Mutex::new(BTreeMap::new()),
            cpu_heavy_runtime: Arc::new(CpuHeavyRuntime::new()),
            state_cache: Arc::new(StateCache::default()),
        }
    }

    /// A test helper that returns a [PersistClientCache] disconnected from
    /// metrics.
    #[cfg(test)]
    pub fn new_no_metrics() -> Self {
        Self::new(PersistConfig::new_for_tests(), &MetricsRegistry::new())
    }

    /// Returns the [PersistConfig] being used by this cache.
    pub fn cfg(&self) -> &PersistConfig {
        &self.cfg
    }

    /// Returns a new [PersistClient] for interfacing with persist shards made
    /// durable to the given [PersistLocation].
    ///
    /// The same `location` may be used concurrently from multiple processes.
    #[instrument(level = "debug", skip_all)]
    pub async fn open(&self, location: PersistLocation) -> Result<PersistClient, ExternalError> {
        let blob = self.open_blob(location.blob_uri).await?;
        let consensus = self.open_consensus(location.consensus_uri).await?;
        PersistClient::new(
            self.cfg.clone(),
            blob,
            consensus,
            Arc::clone(&self.metrics),
            Arc::clone(&self.cpu_heavy_runtime),
            Arc::clone(&self.state_cache),
        )
    }

    // No sense in measuring rtt latencies more often than this.
    const PROMETHEUS_SCRAPE_INTERVAL: Duration = Duration::from_secs(60);

    async fn open_consensus(
        &self,
        consensus_uri: String,
    ) -> Result<Arc<dyn Consensus + Send + Sync>, ExternalError> {
        let mut consensus_by_uri = self.consensus_by_uri.lock().await;
        let consensus = match consensus_by_uri.entry(consensus_uri) {
            Entry::Occupied(x) => Arc::clone(&x.get().1),
            Entry::Vacant(x) => {
                // Intentionally hold the lock, so we don't double connect under
                // concurrency.
                let consensus = ConsensusConfig::try_from(
                    x.key(),
                    Box::new(self.cfg.clone()),
                    self.metrics.postgres_consensus.clone(),
                )?;
                let consensus =
                    retry_external(&self.metrics.retries.external.consensus_open, || {
                        consensus.clone().open()
                    })
                    .await;
                let consensus =
                    Arc::new(MetricsConsensus::new(consensus, Arc::clone(&self.metrics)));
                let task = consensus_rtt_latency_task(
                    Arc::clone(&consensus),
                    Arc::clone(&self.metrics),
                    Self::PROMETHEUS_SCRAPE_INTERVAL,
                )
                .await;
                Arc::clone(&x.insert((RttLatencyTask(task), consensus)).1)
            }
        };
        Ok(consensus)
    }

    async fn open_blob(
        &self,
        blob_uri: String,
    ) -> Result<Arc<dyn Blob + Send + Sync>, ExternalError> {
        let mut blob_by_uri = self.blob_by_uri.lock().await;
        let blob = match blob_by_uri.entry(blob_uri) {
            Entry::Occupied(x) => Arc::clone(&x.get().1),
            Entry::Vacant(x) => {
                // Intentionally hold the lock, so we don't double connect under
                // concurrency.
                let blob = BlobConfig::try_from(
                    x.key(),
                    Box::new(self.cfg.clone()),
                    self.metrics.s3_blob.clone(),
                )
                .await?;
                let blob = retry_external(&self.metrics.retries.external.blob_open, || {
                    blob.clone().open()
                })
                .await;
                let blob = Arc::new(MetricsBlob::new(blob, Arc::clone(&self.metrics)));
                let task = blob_rtt_latency_task(
                    Arc::clone(&blob),
                    Arc::clone(&self.metrics),
                    Self::PROMETHEUS_SCRAPE_INTERVAL,
                )
                .await;
                Arc::clone(&x.insert((RttLatencyTask(task), blob)).1)
            }
        };
        Ok(blob)
    }
}

/// Starts a task to periodically measure the persist-observed latency to
/// consensus.
///
/// This is a task, rather than something like looking at the latencies of prod
/// traffic, so that we minimize any issues around Futures not being polled
/// promptly (as can and does happen with the Timely-polled Futures).
///
/// The caller is responsible for shutdown via [JoinHandle::abort].
///
/// No matter whether we wrap MetricsConsensus before or after we start up the
/// rtt latency task, there's the possibility for it being confusing at some
/// point. Err on the side of more data (including the latency measurements) to
/// start.
async fn blob_rtt_latency_task(
    blob: Arc<MetricsBlob>,
    metrics: Arc<Metrics>,
    measurement_interval: Duration,
) -> JoinHandle<()> {
    mz_ore::task::spawn(|| "persist::blob_rtt_latency", async move {
        // Use the tokio Instant for next_measurement because the reclock tests
        // mess with the tokio sleep clock.
        let mut next_measurement = tokio::time::Instant::now();
        loop {
            tokio::time::sleep_until(next_measurement).await;
            let start = Instant::now();
            match blob.get(BLOB_GET_LIVENESS_KEY).await {
                Ok(_) => {
                    metrics.blob.rtt_latency.set(start.elapsed().as_secs_f64());
                }
                Err(_) => {
                    // Don't spam retries if this returns an error. We're
                    // guaranteed by the method signature that we've already got
                    // metrics coverage of these, so we'll count the errors.
                }
            }
            next_measurement = tokio::time::Instant::now() + measurement_interval;
        }
    })
}

/// Starts a task to periodically measure the persist-observed latency to
/// consensus.
///
/// This is a task, rather than something like looking at the latencies of prod
/// traffic, so that we minimize any issues around Futures not being polled
/// promptly (as can and does happen with the Timely-polled Futures).
///
/// The caller is responsible for shutdown via [JoinHandle::abort].
///
/// No matter whether we wrap MetricsConsensus before or after we start up the
/// rtt latency task, there's the possibility for it being confusing at some
/// point. Err on the side of more data (including the latency measurements) to
/// start.
async fn consensus_rtt_latency_task(
    consensus: Arc<MetricsConsensus>,
    metrics: Arc<Metrics>,
    measurement_interval: Duration,
) -> JoinHandle<()> {
    mz_ore::task::spawn(|| "persist::blob_rtt_latency", async move {
        // Use the tokio Instant for next_measurement because the reclock tests
        // mess with the tokio sleep clock.
        let mut next_measurement = tokio::time::Instant::now();
        loop {
            tokio::time::sleep_until(next_measurement).await;
            let start = Instant::now();
            match consensus.head(CONSENSUS_HEAD_LIVENESS_KEY).await {
                Ok(_) => {
                    metrics
                        .consensus
                        .rtt_latency
                        .set(start.elapsed().as_secs_f64());
                }
                Err(_) => {
                    // Don't spam retries if this returns an error. We're
                    // guaranteed by the method signature that we've already got
                    // metrics coverage of these, so we'll count the errors.
                }
            }
            next_measurement = tokio::time::Instant::now() + measurement_interval;
        }
    })
}

trait DynState: Debug + Send + Sync {
    fn codecs(&self) -> (String, String, String, String, Option<CodecConcreteType>);
    fn as_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync>;
}

impl<K, V, T, D> DynState for RwLock<TypedState<K, V, T, D>>
where
    K: Codec,
    V: Codec,
    T: Timestamp + Codec64,
    D: Codec64,
{
    fn codecs(&self) -> (String, String, String, String, Option<CodecConcreteType>) {
        (
            K::codec_name(),
            V::codec_name(),
            T::codec_name(),
            D::codec_name(),
            Some(CodecConcreteType(std::any::type_name::<
                TypedState<K, V, T, D>,
            >())),
        )
    }

    fn as_any(self: Arc<Self>) -> Arc<dyn Any + Send + Sync> {
        self
    }
}

/// A cache of `TypedState`, shared between all machines for that shard.
///
/// This is shared between all machines that come out of the same
/// [PersistClientCache], but in production there is one of those per process,
/// so in practice, we have one copy of state per shard per process.
///
/// The mutex contention between commands is not an issue, because if two
/// command for the same shard are executing concurrently, only one can win
/// anyway, the other will retry. With the mutex, we even get to avoid the retry
/// if the racing commands are on the same process.
#[derive(Debug, Default)]
pub struct StateCache {
    states: Mutex<BTreeMap<ShardId, Arc<OnceCell<Weak<dyn DynState>>>>>,
}

#[derive(Debug)]
enum StateCacheInit {
    Init(Arc<dyn DynState>),
    NeedInit(Arc<OnceCell<Weak<dyn DynState>>>),
}

impl StateCache {
    pub(crate) async fn get<K, V, T, D, F, InitFn>(
        &self,
        shard_id: ShardId,
        mut init_fn: InitFn,
    ) -> Result<LockingTypedState<K, V, T, D>, Box<CodecMismatch>>
    where
        K: Debug + Codec,
        V: Debug + Codec,
        T: Timestamp + Lattice + Codec64,
        D: Semigroup + Codec64,
        F: Future<Output = Result<TypedState<K, V, T, D>, Box<CodecMismatch>>>,
        InitFn: FnMut() -> F,
    {
        loop {
            let init = {
                let mut states = self.states.lock().await;
                let state = states.entry(shard_id).or_default();
                match state.get() {
                    Some(once_val) => match once_val.upgrade() {
                        Some(x) => StateCacheInit::Init(x),
                        None => {
                            // If the Weak has lost the ability to upgrade,
                            // we've dropped the State and it's gone. Clear the
                            // OnceCell and init a new one.
                            *state = Arc::new(OnceCell::new());
                            StateCacheInit::NeedInit(Arc::clone(state))
                        }
                    },
                    None => StateCacheInit::NeedInit(Arc::clone(state)),
                }
            };

            let state = match init {
                StateCacheInit::Init(x) => x,
                StateCacheInit::NeedInit(init_once) => {
                    let mut did_init: Option<Arc<RwLock<TypedState<K, V, T, D>>>> = None;
                    let state = init_once
                        .get_or_try_init::<Box<CodecMismatch>, _, _>(|| async {
                            let init_res = init_fn().await;
                            let state = Arc::new(RwLock::new(init_res?));
                            let ret = Arc::downgrade(&state);
                            did_init = Some(state);
                            let ret: Weak<dyn DynState> = ret;
                            Ok(ret)
                        })
                        .await?;
                    if let Some(x) = did_init {
                        // We actually did the init work, don't bother casting back
                        // the type erased and weak version.
                        return Ok(LockingTypedState(x));
                    }
                    let Some(state) = state.upgrade() else {
                        // Race condition. Between when we first checked the
                        // OnceCell and the `get_or_try_init` call, (1) the
                        // initialization finished, (2) the other user dropped
                        // the strong ref, and (3) the Arc noticed it was down
                        // to only weak refs and dropped the value. Nothing we
                        // can do except try again.
                        continue;
                    };
                    state
                }
            };

            match Arc::clone(&state)
                .as_any()
                .downcast::<RwLock<TypedState<K, V, T, D>>>()
            {
                Ok(x) => return Ok(LockingTypedState(x)),
                Err(_) => {
                    return Err(Box::new(CodecMismatch {
                        requested: (
                            K::codec_name(),
                            V::codec_name(),
                            T::codec_name(),
                            D::codec_name(),
                            Some(CodecConcreteType(std::any::type_name::<
                                TypedState<K, V, T, D>,
                            >())),
                        ),
                        actual: state.codecs(),
                    }))
                }
            }
        }
    }

    #[cfg(test)]
    async fn get_cached(&self, shard_id: &ShardId) -> Option<Arc<dyn DynState>> {
        self.states
            .lock()
            .await
            .get(shard_id)
            .and_then(|x| x.get())
            .and_then(|x| x.upgrade())
    }

    #[cfg(test)]
    async fn initialized_count(&self) -> usize {
        self.states
            .lock()
            .await
            .values()
            .filter(|x| x.initialized())
            .count()
    }

    #[cfg(test)]
    async fn strong_count(&self) -> usize {
        self.states
            .lock()
            .await
            .values()
            .filter(|x| x.get().map_or(false, |x| x.upgrade().is_some()))
            .count()
    }
}

/// A locked decorator for TypedState that abstracts out the specific lock implementation used.
/// Guards the private lock with public accessor fns to make locking scopes more explicit and
/// simpler to reason about.
#[derive(Debug)]
pub(crate) struct LockingTypedState<K, V, T, D>(Arc<RwLock<TypedState<K, V, T, D>>>);

impl<K, V, T, D> Clone for LockingTypedState<K, V, T, D> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<K, V, T, D> LockingTypedState<K, V, T, D> {
    pub(crate) fn read_lock<R, F: FnMut(&TypedState<K, V, T, D>) -> R>(
        &self,
        metrics: &LockMetrics,
        mut f: F,
    ) -> R {
        metrics.acquire_count.inc();
        let state = match self.0.try_read() {
            Ok(x) => x,
            Err(TryLockError::WouldBlock) => {
                metrics.blocking_acquire_count.inc();
                let start = Instant::now();
                let state = self.0.read().expect("lock poisoned");
                metrics
                    .blocking_seconds
                    .inc_by(start.elapsed().as_secs_f64());
                state
            }
            Err(TryLockError::Poisoned(err)) => panic!("state read lock poisoned: {}", err),
        };
        f(&state)
    }

    pub(crate) fn write_lock<R, F: FnOnce(&mut TypedState<K, V, T, D>) -> R>(
        &self,
        metrics: &LockMetrics,
        f: F,
    ) -> R {
        metrics.acquire_count.inc();
        let mut state = match self.0.try_write() {
            Ok(x) => x,
            Err(TryLockError::WouldBlock) => {
                metrics.blocking_acquire_count.inc();
                let start = Instant::now();
                let state = self.0.write().expect("lock poisoned");
                metrics
                    .blocking_seconds
                    .inc_by(start.elapsed().as_secs_f64());
                state
            }
            Err(TryLockError::Poisoned(err)) => panic!("state read lock poisoned: {}", err),
        };
        f(&mut state)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Deref;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::stream::{FuturesUnordered, StreamExt};
    use mz_build_info::DUMMY_BUILD_INFO;
    use mz_ore::now::SYSTEM_TIME;
    use mz_ore::task::spawn;

    use super::*;

    #[tokio::test]
    #[cfg_attr(miri, ignore)] // unsupported operation: can't call foreign function `epoll_wait` on OS `linux`
    async fn client_cache() {
        let cache = PersistClientCache::new(
            PersistConfig::new(&DUMMY_BUILD_INFO, SYSTEM_TIME.clone()),
            &MetricsRegistry::new(),
        );
        assert_eq!(cache.blob_by_uri.lock().await.len(), 0);
        assert_eq!(cache.consensus_by_uri.lock().await.len(), 0);

        // Opening a location on an empty cache saves the results.
        let _ = cache
            .open(PersistLocation {
                blob_uri: "mem://blob_zero".to_owned(),
                consensus_uri: "mem://consensus_zero".to_owned(),
            })
            .await
            .expect("failed to open location");
        assert_eq!(cache.blob_by_uri.lock().await.len(), 1);
        assert_eq!(cache.consensus_by_uri.lock().await.len(), 1);

        // Opening a location with an already opened consensus reuses it, even
        // if the blob is different.
        let _ = cache
            .open(PersistLocation {
                blob_uri: "mem://blob_one".to_owned(),
                consensus_uri: "mem://consensus_zero".to_owned(),
            })
            .await
            .expect("failed to open location");
        assert_eq!(cache.blob_by_uri.lock().await.len(), 2);
        assert_eq!(cache.consensus_by_uri.lock().await.len(), 1);

        // Ditto the other way.
        let _ = cache
            .open(PersistLocation {
                blob_uri: "mem://blob_one".to_owned(),
                consensus_uri: "mem://consensus_one".to_owned(),
            })
            .await
            .expect("failed to open location");
        assert_eq!(cache.blob_by_uri.lock().await.len(), 2);
        assert_eq!(cache.consensus_by_uri.lock().await.len(), 2);

        // Query params and path matter, so we get new instances.
        let _ = cache
            .open(PersistLocation {
                blob_uri: "mem://blob_one?foo".to_owned(),
                consensus_uri: "mem://consensus_one/bar".to_owned(),
            })
            .await
            .expect("failed to open location");
        assert_eq!(cache.blob_by_uri.lock().await.len(), 3);
        assert_eq!(cache.consensus_by_uri.lock().await.len(), 3);

        // User info and port also matter, so we get new instances.
        let _ = cache
            .open(PersistLocation {
                blob_uri: "mem://user@blob_one".to_owned(),
                consensus_uri: "mem://@consensus_one:123".to_owned(),
            })
            .await
            .expect("failed to open location");
        assert_eq!(cache.blob_by_uri.lock().await.len(), 4);
        assert_eq!(cache.consensus_by_uri.lock().await.len(), 4);
    }

    #[tokio::test]
    async fn state_cache() {
        mz_ore::test::init_logging();
        fn new_state<K, V, T, D>(shard_id: ShardId) -> TypedState<K, V, T, D>
        where
            K: Codec,
            V: Codec,
            T: Timestamp + Lattice + Codec64,
            D: Codec64,
        {
            TypedState::new(
                DUMMY_BUILD_INFO.semver_version(),
                shard_id,
                "host".into(),
                0,
            )
        }
        fn assert_same<K, V, T, D>(
            state1: &LockingTypedState<K, V, T, D>,
            state2: &LockingTypedState<K, V, T, D>,
        ) {
            let pointer1 = format!("{:p}", state1.0.read().expect("lock").deref());
            let pointer2 = format!("{:p}", state2.0.read().expect("lock").deref());
            assert_eq!(pointer1, pointer2);
        }

        let s1 = ShardId::new();
        let states = Arc::new(StateCache::default());

        // The cache starts empty.
        assert_eq!(states.states.lock().await.len(), 0);

        // Panic'ing during init_fn .
        let s = Arc::clone(&states);
        let res = spawn(|| "test", async move {
            s.get::<(), (), u64, i64, _, _>(s1, || async { panic!("boom") })
                .await
        })
        .await;
        assert!(res.is_err());
        assert_eq!(states.initialized_count().await, 0);

        // Returning an error from init_fn doesn't initialize an entry in the cache.
        let res = states
            .get::<(), (), u64, i64, _, _>(s1, || async {
                Err(Box::new(CodecMismatch {
                    requested: ("".into(), "".into(), "".into(), "".into(), None),
                    actual: ("".into(), "".into(), "".into(), "".into(), None),
                }))
            })
            .await;
        assert!(res.is_err());
        assert_eq!(states.initialized_count().await, 0);

        // Initialize one shard.
        let did_work = Arc::new(AtomicBool::new(false));
        let s1_state1 = states
            .get::<(), (), u64, i64, _, _>(s1, || {
                let did_work = Arc::clone(&did_work);
                async move {
                    did_work.store(true, Ordering::SeqCst);
                    Ok(new_state(s1))
                }
            })
            .await
            .expect("should successfully initialize");
        assert_eq!(did_work.load(Ordering::SeqCst), true);
        assert_eq!(states.initialized_count().await, 1);
        assert_eq!(states.strong_count().await, 1);

        // Trying to initialize it again does no work and returns the same state.
        let did_work = Arc::new(AtomicBool::new(false));
        let s1_state2 = states
            .get::<(), (), u64, i64, _, _>(s1, || {
                let did_work = Arc::clone(&did_work);
                async move {
                    did_work.store(true, Ordering::SeqCst);
                    did_work.store(true, Ordering::SeqCst);
                    Ok(new_state(s1))
                }
            })
            .await
            .expect("should successfully initialize");
        assert_eq!(did_work.load(Ordering::SeqCst), false);
        assert_eq!(states.initialized_count().await, 1);
        assert_eq!(states.strong_count().await, 1);
        assert_same(&s1_state1, &s1_state2);

        // Trying to initialize with different types doesn't work.
        let did_work = Arc::new(AtomicBool::new(false));
        let res = states
            .get::<String, (), u64, i64, _, _>(s1, || {
                let did_work = Arc::clone(&did_work);
                async move {
                    did_work.store(true, Ordering::SeqCst);
                    Ok(new_state(s1))
                }
            })
            .await;
        assert_eq!(did_work.load(Ordering::SeqCst), false);
        assert_eq!(
            format!("{}", res.expect_err("types shouldn't match")),
            "requested codecs (\"String\", \"()\", \"u64\", \"i64\", Some(CodecConcreteType(\"mz_persist_client::internal::state::TypedState<alloc::string::String, (), u64, i64>\"))) did not match ones in durable storage (\"()\", \"()\", \"u64\", \"i64\", Some(CodecConcreteType(\"mz_persist_client::internal::state::TypedState<(), (), u64, i64>\")))"
        );
        assert_eq!(states.initialized_count().await, 1);
        assert_eq!(states.strong_count().await, 1);

        // We can add a shard of a different type.
        let s2 = ShardId::new();
        let s2_state1 = states
            .get::<String, (), u64, i64, _, _>(s2, || async { Ok(new_state(s2)) })
            .await
            .expect("should successfully initialize");
        assert_eq!(states.initialized_count().await, 2);
        assert_eq!(states.strong_count().await, 2);
        let s2_state2 = states
            .get::<String, (), u64, i64, _, _>(s2, || async { Ok(new_state(s2)) })
            .await
            .expect("should successfully initialize");
        assert_same(&s2_state1, &s2_state2);

        // The cache holds weak references to State so we reclaim memory if the
        // shards stops being used.
        drop(s1_state1);
        assert_eq!(states.strong_count().await, 2);
        drop(s1_state2);
        assert_eq!(states.strong_count().await, 1);
        assert_eq!(states.initialized_count().await, 2);
        assert!(states.get_cached(&s1).await.is_none());

        // But we can re-init that shard if necessary.
        let s1_state1 = states
            .get::<(), (), u64, i64, _, _>(s1, || async { Ok(new_state(s1)) })
            .await
            .expect("should successfully initialize");
        assert_eq!(states.initialized_count().await, 2);
        assert_eq!(states.strong_count().await, 2);
        drop(s1_state1);
        assert_eq!(states.strong_count().await, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn state_cache_concurrency() {
        mz_ore::test::init_logging();

        const COUNT: usize = 1000;
        let id = ShardId::new();
        let cache = Arc::new(StateCache::default());

        let mut futures = (0..COUNT)
            .map(|_| {
                cache.get::<(), (), u64, i64, _, _>(id, || async {
                    Ok(TypedState::new(
                        DUMMY_BUILD_INFO.semver_version(),
                        id,
                        "host".into(),
                        0,
                    ))
                })
            })
            .collect::<FuturesUnordered<_>>();

        for _ in 0..COUNT {
            let _ = futures.next().await.unwrap();
        }
    }
}
