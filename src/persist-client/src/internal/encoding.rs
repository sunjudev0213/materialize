// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use differential_dataflow::lattice::Lattice;
use differential_dataflow::trace::Description;
use mz_persist_types::{Codec, Codec64};
use mz_proto::{IntoRustIfSome, ProtoType, RustType, TryFromProtoError};
use prost::Message;
use semver::Version;
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;
use uuid::Uuid;

use crate::error::CodecMismatch;
use crate::fetch::{LeasedBatch, LeasedBatchMetadata};
use crate::internal::paths::{PartialBatchKey, PartialRollupKey};
use crate::internal::state::proto_leased_batch_metadata;
use crate::internal::state::{
    HollowBatch, ProtoHollowBatch, ProtoLeasedBatch, ProtoLeasedBatchMetadata, ProtoReaderState,
    ProtoStateDiff, ProtoStateFieldDiff, ProtoStateFieldDiffType, ProtoStateRollup, ProtoTrace,
    ProtoU64Antichain, ProtoU64Description, ProtoWriterState, ReaderState, State, StateCollections,
    WriterState,
};
use crate::internal::state_diff::{StateDiff, StateFieldDiff, StateFieldValDiff};
use crate::internal::trace::Trace;
use crate::read::ReaderId;
use crate::{ShardId, WriterId};

pub(crate) fn parse_id(id_prefix: char, id_type: &str, encoded: &str) -> Result<[u8; 16], String> {
    let uuid_encoded = match encoded.strip_prefix(id_prefix) {
        Some(x) => x,
        None => return Err(format!("invalid {} {}: incorrect prefix", id_type, encoded)),
    };
    let uuid = Uuid::parse_str(&uuid_encoded)
        .map_err(|err| format!("invalid {} {}: {}", id_type, encoded, err))?;
    Ok(*uuid.as_bytes())
}

// If persist gets some encoded ProtoState from the future (e.g. two versions of
// code are running simultaneously against the same shard), it might have a
// field that the current code doesn't know about. This would be silently
// discarded at proto decode time. Unknown Fields [1] are a tool we can use in
// the future to help deal with this, but in the short-term, it's best to keep
// the persist read-modify-CaS loop simple for as long as we can get away with
// it (i.e. until we have to offer the ability to do rollbacks).
//
// [1]: https://developers.google.com/protocol-buffers/docs/proto3#unknowns
//
// To detect the bad situation and disallow it, we tag every version of state
// written to consensus with the version of code used to encode it. Then at
// decode time, we're able to compare the current version against any we receive
// and assert as necessary.
//
// Initially we reject any version from the future (no forward compatibility,
// most conservative but easiest to reason about) but allow any from the past
// (permanent backward compatibility). If/when we support deploy rollbacks and
// rolling upgrades, we can adjust this assert as necessary to reflect the
// policy (e.g. by adding some window of X allowed versions of forward
// compatibility, computed by comparing semvers).
//
// We could do the same for blob data, but it shouldn't be necessary. Any blob
// data we read is going to be because we fetched it using a pointer stored in
// some persist state. If we can handle the state, we can handle the blobs it
// references, too.
fn check_applier_version(build_version: &Version, applier_version: &Version) {
    if build_version < applier_version {
        panic!(
            "{} received persist state from the future {}",
            build_version, applier_version
        );
    }
}

impl RustType<String> for ShardId {
    fn into_proto(&self) -> String {
        self.to_string()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        match parse_id('s', "ShardId", &proto) {
            Ok(x) => Ok(ShardId(x)),
            Err(_) => Err(TryFromProtoError::InvalidShardId(proto)),
        }
    }
}

impl RustType<String> for ReaderId {
    fn into_proto(&self) -> String {
        self.to_string()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        match parse_id('r', "ReaderId", &proto) {
            Ok(x) => Ok(ReaderId(x)),
            Err(_) => Err(TryFromProtoError::InvalidShardId(proto)),
        }
    }
}

impl RustType<String> for WriterId {
    fn into_proto(&self) -> String {
        self.to_string()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        match parse_id('w', "WriterId", &proto) {
            Ok(x) => Ok(WriterId(x)),
            Err(_) => Err(TryFromProtoError::InvalidShardId(proto)),
        }
    }
}

impl RustType<String> for PartialBatchKey {
    fn into_proto(&self) -> String {
        self.0.clone()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        Ok(PartialBatchKey(proto))
    }
}

impl RustType<String> for PartialRollupKey {
    fn into_proto(&self) -> String {
        self.0.clone()
    }

    fn from_proto(proto: String) -> Result<Self, TryFromProtoError> {
        Ok(PartialRollupKey(proto))
    }
}

impl<T: Timestamp + Lattice + Codec64> StateDiff<T> {
    pub fn decode(build_version: &Version, buf: &[u8]) -> Self {
        let proto = ProtoStateDiff::decode(buf)
            // We received a State that we couldn't decode. This could happen if
            // persist messes up backward/forward compatibility, if the durable
            // data was corrupted, or if operations messes up deployment. In any
            // case, fail loudly.
            .expect("internal error: invalid encoded state");
        let diff = Self::from_proto(proto).expect("internal error: invalid encoded state");
        check_applier_version(build_version, &diff.applier_version);
        diff
    }
}

impl<T> Codec for StateDiff<T>
where
    T: Timestamp + Lattice + Codec64,
{
    fn codec_name() -> String {
        "proto[StateDiff]".into()
    }

    fn encode<B>(&self, buf: &mut B)
    where
        B: bytes::BufMut,
    {
        self.into_proto()
            .encode(buf)
            .expect("no required fields means no initialization errors");
    }

    fn decode<'a>(buf: &'a [u8]) -> Result<Self, String> {
        let proto = ProtoStateDiff::decode(buf).map_err(|err| err.to_string())?;
        proto.into_rust().map_err(|err| err.to_string())
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoStateDiff> for StateDiff<T> {
    fn into_proto(&self) -> ProtoStateDiff {
        ProtoStateDiff {
            applier_version: self.applier_version.to_string(),
            seqno_from: self.seqno_from.into_proto(),
            seqno_to: self.seqno_to.into_proto(),
            latest_rollup_key: self.latest_rollup_key.into_proto(),
            rollups: field_diffs_into_proto(
                &self.rollups,
                |k| k.into_proto().encode_to_vec(),
                |v| v.into_proto().encode_to_vec(),
            ),
            last_gc_req: field_diffs_into_proto(
                &self.last_gc_req,
                |()| Vec::new(),
                |v| v.into_proto().encode_to_vec(),
            ),
            readers: field_diffs_into_proto(
                &self.readers,
                |k| k.into_proto().encode_to_vec(),
                |v| {
                    ProtoReaderState {
                        since: Some(v.since.into_proto()),
                        seqno: v.seqno.into_proto(),
                        last_heartbeat_timestamp_ms: v.last_heartbeat_timestamp_ms,
                    }
                    .encode_to_vec()
                },
            ),
            writers: field_diffs_into_proto(
                &self.writers,
                |k| k.into_proto().encode_to_vec(),
                |v| {
                    ProtoWriterState {
                        last_heartbeat_timestamp_ms: v.last_heartbeat_timestamp_ms,
                        lease_duration_ms: v.lease_duration_ms,
                    }
                    .encode_to_vec()
                },
            ),
            since: field_diffs_into_proto(
                &self.since,
                |()| Vec::new(),
                |v| v.into_proto().encode_to_vec(),
            ),
            spine: field_diffs_into_proto(
                &self.spine,
                |k| k.into_proto().encode_to_vec(),
                |()| Vec::new(),
            ),
        }
    }

    fn from_proto(proto: ProtoStateDiff) -> Result<Self, TryFromProtoError> {
        let applier_version = if proto.applier_version.is_empty() {
            // Backward compatibility with versions of ProtoState before we set
            // this field: if it's missing (empty), assume an infinitely old
            // version.
            semver::Version::new(0, 0, 0)
        } else {
            semver::Version::parse(&proto.applier_version).map_err(|err| {
                TryFromProtoError::InvalidSemverVersion(format!(
                    "invalid applier_version {}: {}",
                    proto.applier_version, err
                ))
            })?
        };
        Ok(StateDiff {
            applier_version,
            seqno_from: proto.seqno_from.into_rust()?,
            seqno_to: proto.seqno_to.into_rust()?,
            latest_rollup_key: proto.latest_rollup_key.into_rust()?,
            rollups: field_diffs_into_rust::<u64, String, _, _, _, _>(
                proto.rollups,
                |k| k.into_rust(),
                |v| v.into_rust(),
            )?
            .into_iter()
            .collect(),
            last_gc_req: field_diffs_into_rust::<(), u64, _, _, _, _>(
                proto.last_gc_req,
                |()| Ok(()),
                |v| v.into_rust(),
            )?,
            readers: field_diffs_into_rust::<String, ProtoReaderState, _, _, _, _>(
                proto.readers,
                |k| k.into_rust(),
                |v| {
                    Ok(ReaderState {
                        since: v.since.into_rust_if_some("since")?,
                        seqno: v.seqno.into_rust()?,
                        last_heartbeat_timestamp_ms: v.last_heartbeat_timestamp_ms,
                    })
                },
            )?
            .into_iter()
            .collect(),
            writers: field_diffs_into_rust::<String, ProtoWriterState, _, _, _, _>(
                proto.writers,
                |k| k.into_rust(),
                |v| {
                    Ok(WriterState {
                        last_heartbeat_timestamp_ms: v.last_heartbeat_timestamp_ms,
                        lease_duration_ms: v.lease_duration_ms,
                    })
                },
            )?
            .into_iter()
            .collect(),
            since: field_diffs_into_rust::<(), ProtoU64Antichain, _, _, _, _>(
                proto.since,
                |()| Ok(()),
                |v| v.into_rust(),
            )?,
            spine: field_diffs_into_rust::<ProtoHollowBatch, (), _, _, _, _>(
                proto.spine,
                |k| k.into_rust(),
                |()| Ok(()),
            )?,
        })
    }
}

fn field_diffs_into_proto<K, V, KFn, VFn>(
    diffs: &[StateFieldDiff<K, V>],
    k_fn: KFn,
    v_fn: VFn,
) -> Vec<ProtoStateFieldDiff>
where
    KFn: Fn(&K) -> Vec<u8>,
    VFn: Fn(&V) -> Vec<u8>,
{
    diffs
        .iter()
        .map(|diff| {
            let (diff_type, from, to) = match &diff.val {
                StateFieldValDiff::Insert(to) => {
                    (ProtoStateFieldDiffType::Insert, Vec::new(), v_fn(to))
                }
                StateFieldValDiff::Update(from, to) => {
                    (ProtoStateFieldDiffType::Update, v_fn(from), v_fn(to))
                }
                StateFieldValDiff::Delete(from) => {
                    (ProtoStateFieldDiffType::Delete, v_fn(from), Vec::new())
                }
            };
            ProtoStateFieldDiff {
                key: k_fn(&diff.key),
                diff_type: i32::from(diff_type),
                from,
                to,
            }
        })
        .collect()
}

fn field_diffs_into_rust<KP, VP, K, V, KFn, VFn>(
    protos: Vec<ProtoStateFieldDiff>,
    k_fn: KFn,
    v_fn: VFn,
) -> Result<Vec<StateFieldDiff<K, V>>, TryFromProtoError>
where
    KP: prost::Message + Default,
    VP: prost::Message + Default,
    KFn: Fn(KP) -> Result<K, TryFromProtoError>,
    VFn: Fn(VP) -> Result<V, TryFromProtoError>,
{
    let mut diffs = Vec::new();
    for proto in protos {
        let val = match ProtoStateFieldDiffType::from_i32(proto.diff_type) {
            Some(ProtoStateFieldDiffType::Insert) => {
                let to = VP::decode(proto.to.as_slice())
                    .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;
                StateFieldValDiff::Insert(v_fn(to)?)
            }
            Some(ProtoStateFieldDiffType::Update) => {
                let from = VP::decode(proto.from.as_slice())
                    .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;
                let to = VP::decode(proto.to.as_slice())
                    .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;

                StateFieldValDiff::Update(v_fn(from)?, v_fn(to)?)
            }
            Some(ProtoStateFieldDiffType::Delete) => {
                let from = VP::decode(proto.from.as_slice())
                    .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;
                StateFieldValDiff::Delete(v_fn(from)?)
            }
            None => {
                return Err(TryFromProtoError::unknown_enum_variant(format!(
                    "ProtoStateFieldDiffType {}",
                    proto.diff_type,
                )))
            }
        };
        let key = KP::decode(proto.key.as_slice())
            .map_err(|err| TryFromProtoError::InvalidPersistState(err.to_string()))?;
        diffs.push(StateFieldDiff {
            key: k_fn(key)?,
            val,
        });
    }
    Ok(diffs)
}

impl<K, V, T, D> State<K, V, T, D>
where
    K: Codec,
    V: Codec,
    T: Timestamp + Lattice + Codec64,
    D: Codec64,
{
    pub fn encode<B>(&self, buf: &mut B)
    where
        B: bytes::BufMut,
    {
        self.into_proto()
            .encode(buf)
            .expect("no required fields means no initialization errors");
    }

    pub fn decode(build_version: &Version, buf: &[u8]) -> Result<Self, CodecMismatch> {
        let proto = ProtoStateRollup::decode(buf)
            // We received a State that we couldn't decode. This could happen if
            // persist messes up backward/forward compatibility, if the durable
            // data was corrupted, or if operations messes up deployment. In any
            // case, fail loudly.
            .expect("internal error: invalid encoded state");
        let state = Self::try_from(proto).expect("internal error: invalid encoded state")?;
        check_applier_version(build_version, &state.applier_version);
        Ok(state)
    }
}

impl<K, V, T, D> RustType<ProtoStateRollup> for State<K, V, T, D>
where
    K: Codec,
    V: Codec,
    T: Timestamp + Lattice + Codec64,
    D: Codec64,
{
    fn into_proto(&self) -> ProtoStateRollup {
        ProtoStateRollup {
            applier_version: self.applier_version.to_string(),
            shard_id: self.shard_id.into_proto(),
            seqno: self.seqno.into_proto(),
            key_codec: K::codec_name(),
            val_codec: V::codec_name(),
            ts_codec: T::codec_name(),
            diff_codec: D::codec_name(),
            last_gc_req: self.collections.last_gc_req.into_proto(),
            rollups: self
                .collections
                .rollups
                .iter()
                .map(|(seqno, key)| (seqno.into_proto(), key.into_proto()))
                .collect(),
            readers: self
                .collections
                .readers
                .iter()
                .map(|(id, state)| (id.into_proto(), state.into_proto()))
                .collect(),
            writers: self
                .collections
                .writers
                .iter()
                .map(|(id, state)| (id.into_proto(), state.into_proto()))
                .collect(),
            trace: Some(self.collections.trace.into_proto()),
        }
    }

    fn from_proto(proto: ProtoStateRollup) -> Result<Self, TryFromProtoError> {
        match State::try_from(proto) {
            Ok(Ok(x)) => Ok(x),
            Ok(Err(err)) => Err(TryFromProtoError::CodecMismatch(err.to_string())),
            Err(err) => Err(err),
        }
    }
}

impl<K, V, T, D> State<K, V, T, D>
where
    K: Codec,
    V: Codec,
    T: Timestamp + Lattice + Codec64,
    D: Codec64,
{
    fn try_from(x: ProtoStateRollup) -> Result<Result<Self, CodecMismatch>, TryFromProtoError> {
        if K::codec_name() != x.key_codec
            || V::codec_name() != x.val_codec
            || T::codec_name() != x.ts_codec
            || D::codec_name() != x.diff_codec
        {
            return Ok(Err(CodecMismatch {
                requested: (
                    K::codec_name(),
                    V::codec_name(),
                    T::codec_name(),
                    D::codec_name(),
                ),
                actual: (x.key_codec, x.val_codec, x.ts_codec, x.diff_codec),
            }));
        }

        let applier_version = if x.applier_version.is_empty() {
            // Backward compatibility with versions of ProtoState before we set
            // this field: if it's missing (empty), assume an infinitely old
            // version.
            semver::Version::new(0, 0, 0)
        } else {
            semver::Version::parse(&x.applier_version).map_err(|err| {
                TryFromProtoError::InvalidSemverVersion(format!(
                    "invalid applier_version {}: {}",
                    x.applier_version, err
                ))
            })?
        };

        let mut rollups = BTreeMap::new();
        for (seqno, key) in x.rollups {
            rollups.insert(seqno.into_rust()?, key.into_rust()?);
        }
        let mut readers = BTreeMap::new();
        for (id, state) in x.readers {
            readers.insert(id.into_rust()?, state.into_rust()?);
        }
        let mut writers = BTreeMap::new();
        for (id, state) in x.writers {
            writers.insert(id.into_rust()?, state.into_rust()?);
        }
        let collections = StateCollections {
            rollups,
            last_gc_req: x.last_gc_req.into_rust()?,
            readers,
            writers,
            trace: x.trace.into_rust_if_some("trace")?,
        };
        Ok(Ok(State {
            applier_version,
            shard_id: x.shard_id.into_rust()?,
            seqno: x.seqno.into_rust()?,
            collections,
            _phantom: PhantomData,
        }))
    }
}

impl<T: Timestamp + Lattice + Codec64> RustType<ProtoTrace> for Trace<T> {
    fn into_proto(&self) -> ProtoTrace {
        let mut spine = Vec::new();
        self.map_batches(|b| {
            spine.push(b.into_proto());
        });
        ProtoTrace {
            since: Some(self.since().into_proto()),
            spine,
        }
    }

    fn from_proto(proto: ProtoTrace) -> Result<Self, TryFromProtoError> {
        let mut ret = Trace::default();
        ret.downgrade_since(&proto.since.into_rust_if_some("since")?);
        for batch in proto.spine.into_iter() {
            let batch: HollowBatch<T> = batch.into_rust()?;
            if PartialOrder::less_than(ret.since(), batch.desc.since()) {
                return Err(TryFromProtoError::InvalidPersistState(format!(
                    "invalid ProtoTrace: the spine's since {:?} was less than a batch's since {:?}",
                    ret.since(),
                    batch.desc.since()
                )));
            }
            // We could perhaps more directly serialize and rehydrate the
            // internals of the Spine, but this is nice because it insulates
            // us against changes in the Spine logic. The current logic has
            // turned out to be relatively expensive in practice, but as we
            // tune things (especially when we add inc state) the rate of
            // this deserialization should go down. Revisit as necessary.
            //
            // Ignore merge_reqs because whichever process generated this diff is
            // assigned the work.
            let _merge_reqs = ret.push_batch(batch);
        }
        Ok(ret)
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoReaderState> for ReaderState<T> {
    fn into_proto(&self) -> ProtoReaderState {
        ProtoReaderState {
            seqno: self.seqno.into_proto(),
            since: Some(self.since.into_proto()),
            last_heartbeat_timestamp_ms: self.last_heartbeat_timestamp_ms.into_proto(),
        }
    }

    fn from_proto(proto: ProtoReaderState) -> Result<Self, TryFromProtoError> {
        Ok(ReaderState {
            seqno: proto.seqno.into_rust()?,
            since: proto.since.into_rust_if_some("ProtoReaderState::since")?,
            last_heartbeat_timestamp_ms: proto.last_heartbeat_timestamp_ms.into_rust()?,
        })
    }
}

impl RustType<ProtoWriterState> for WriterState {
    fn into_proto(&self) -> ProtoWriterState {
        ProtoWriterState {
            last_heartbeat_timestamp_ms: self.last_heartbeat_timestamp_ms.into_proto(),
            lease_duration_ms: self.lease_duration_ms.into_proto(),
        }
    }

    fn from_proto(proto: ProtoWriterState) -> Result<Self, TryFromProtoError> {
        Ok(WriterState {
            last_heartbeat_timestamp_ms: proto.last_heartbeat_timestamp_ms.into_rust()?,
            lease_duration_ms: proto.lease_duration_ms.into_rust()?,
        })
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoHollowBatch> for HollowBatch<T> {
    fn into_proto(&self) -> ProtoHollowBatch {
        ProtoHollowBatch {
            desc: Some(self.desc.into_proto()),
            keys: self.keys.into_proto(),
            len: self.len.into_proto(),
        }
    }

    fn from_proto(proto: ProtoHollowBatch) -> Result<Self, TryFromProtoError> {
        Ok(HollowBatch {
            desc: proto.desc.into_rust_if_some("desc")?,
            keys: proto.keys.into_rust()?,
            len: proto.len.into_rust()?,
        })
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoU64Description> for Description<T> {
    fn into_proto(&self) -> ProtoU64Description {
        ProtoU64Description {
            lower: Some(self.lower().into_proto()),
            upper: Some(self.upper().into_proto()),
            since: Some(self.since().into_proto()),
        }
    }

    fn from_proto(proto: ProtoU64Description) -> Result<Self, TryFromProtoError> {
        Ok(Description::new(
            proto.lower.into_rust_if_some("lower")?,
            proto.upper.into_rust_if_some("upper")?,
            proto.since.into_rust_if_some("since")?,
        ))
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoU64Antichain> for Antichain<T> {
    fn into_proto(&self) -> ProtoU64Antichain {
        ProtoU64Antichain {
            elements: self
                .elements()
                .iter()
                .map(|x| i64::from_le_bytes(T::encode(x)))
                .collect(),
        }
    }

    fn from_proto(proto: ProtoU64Antichain) -> Result<Self, TryFromProtoError> {
        let elements = proto
            .elements
            .iter()
            .map(|x| T::decode(x.to_le_bytes()))
            .collect::<Vec<_>>();
        Ok(Antichain::from(elements))
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoLeasedBatchMetadata> for LeasedBatchMetadata<T> {
    fn into_proto(&self) -> ProtoLeasedBatchMetadata {
        use proto_leased_batch_metadata::*;
        ProtoLeasedBatchMetadata {
            kind: Some(match self {
                LeasedBatchMetadata::Snapshot { as_of } => {
                    Kind::Snapshot(ProtoLeasedBatchMetadataSnapshot {
                        as_of: Some(as_of.into_proto()),
                    })
                }
                LeasedBatchMetadata::Listen { as_of, until } => {
                    Kind::Listen(ProtoLeasedBatchMetadataListen {
                        as_of: Some(as_of.into_proto()),
                        until: Some(until.into_proto()),
                    })
                }
            }),
        }
    }

    fn from_proto(proto: ProtoLeasedBatchMetadata) -> Result<Self, TryFromProtoError> {
        use proto_leased_batch_metadata::Kind::*;
        Ok(match proto.kind {
            Some(Snapshot(snapshot)) => LeasedBatchMetadata::Snapshot {
                as_of: snapshot
                    .as_of
                    .into_rust_if_some("ProtoLeasedBatchMetadata::Kind::Snapshot::as_of")?,
            },
            Some(Listen(listen)) => LeasedBatchMetadata::Listen {
                as_of: listen
                    .as_of
                    .into_rust_if_some("ProtoLeasedBatchMetadata::Kind::Listen::as_of")?,
                until: listen
                    .until
                    .into_rust_if_some("ProtoLeasedBatchMetadata::Kind::Listen::until")?,
            },
            None => {
                return Err(TryFromProtoError::missing_field(
                    "ProtoLeasedBatchMetadata::Kind",
                ))
            }
        })
    }
}

impl<T: Timestamp + Codec64> RustType<ProtoLeasedBatch> for LeasedBatch<T> {
    /// n.b. this is used with [`crate::fetch::SerdeLeasedBatch`].
    fn into_proto(&self) -> ProtoLeasedBatch {
        ProtoLeasedBatch {
            shard_id: self.shard_id.into_proto(),
            reader_id: self.reader_id.into_proto(),
            reader_metadata: Some(self.metadata.into_proto()),
            batch: Some(self.batch.into_proto()),
            leased_seqno: self.leased_seqno.into_proto(),
        }
    }

    /// n.b. this is used with [`crate::fetch::SerdeLeasedBatch`].
    fn from_proto(proto: ProtoLeasedBatch) -> Result<Self, TryFromProtoError> {
        Ok(LeasedBatch {
            shard_id: proto.shard_id.into_rust()?,
            reader_id: proto.reader_id.into_rust()?,
            metadata: proto
                .reader_metadata
                .into_rust_if_some("ProtoLeasedBatch::reader_metadata")?,
            batch: proto.batch.into_rust_if_some("ProtoLeasedBatch::batch")?,
            leased_seqno: proto.leased_seqno.into_rust()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use mz_persist::location::SeqNo;
    use mz_persist_types::Codec;

    use crate::internal::paths::PartialRollupKey;
    use crate::internal::state::State;
    use crate::internal::state_diff::StateDiff;
    use crate::ShardId;

    #[test]
    fn applier_version_state() {
        let v1 = semver::Version::new(1, 0, 0);
        let v2 = semver::Version::new(2, 0, 0);
        let v3 = semver::Version::new(3, 0, 0);

        // Code version v2 evaluates and writes out some State.
        let state = State::<(), (), u64, i64>::new(v2.clone(), ShardId::new());
        let mut buf = Vec::new();
        state.encode(&mut buf);

        // We can read it back using persist code v2 and v3.
        assert_eq!(State::decode(&v2, &buf).as_ref(), Ok(&state));
        assert_eq!(State::decode(&v3, &buf).as_ref(), Ok(&state));

        // But we can't read it back using v1 because v1 might corrupt it by
        // losing or misinterpreting something written out by a future version
        // of code.
        let v1_res = std::panic::catch_unwind(|| State::<(), (), u64, i64>::decode(&v1, &buf));
        assert!(v1_res.is_err());
    }

    #[test]
    fn applier_version_state_diff() {
        let v1 = semver::Version::new(1, 0, 0);
        let v2 = semver::Version::new(2, 0, 0);
        let v3 = semver::Version::new(3, 0, 0);

        // Code version v2 evaluates and writes out some State.
        let diff = StateDiff::<u64>::new(
            v2.clone(),
            SeqNo(0),
            SeqNo(1),
            PartialRollupKey("rollup".into()),
        );
        let mut buf = Vec::new();
        diff.encode(&mut buf);

        // We can read it back using persist code v2 and v3.
        assert_eq!(StateDiff::decode(&v2, &buf), diff);
        assert_eq!(StateDiff::decode(&v3, &buf), diff);

        // But we can't read it back using v1 because v1 might corrupt it by
        // losing or misinterpreting something written out by a future version
        // of code.
        let v1_res = std::panic::catch_unwind(|| StateDiff::<u64>::decode(&v1, &buf));
        assert!(v1_res.is_err());
    }
}
