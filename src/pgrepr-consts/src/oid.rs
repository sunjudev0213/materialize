// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

#![allow(missing_docs)]

//! PostgreSQL OID constants.

// PostgreSQL builtin type OIDs
pub const TYPE_ANY_OID: u32 = 2276;
pub const TYPE_ANYARRAY_OID: u32 = 2277;
pub const TYPE_ANYCOMPATIBLE_OID: u32 = 5077;
pub const TYPE_ANYCOMPATIBLEARRAY_OID: u32 = 5078;
pub const TYPE_ANYCOMPATIBLENONARRAY_OID: u32 = 5079;
pub const TYPE_ANYELEMENT_OID: u32 = 2283;
pub const TYPE_ANYNONARRAY_OID: u32 = 2776;
pub const TYPE_BOOL_ARRAY_OID: u32 = 1000;
pub const TYPE_BOOL_OID: u32 = 16;
pub const TYPE_BPCHAR_ARRAY_OID: u32 = 1014;
pub const TYPE_BPCHAR_OID: u32 = 1042;
pub const TYPE_BYTEA_ARRAY_OID: u32 = 1001;
pub const TYPE_BYTEA_OID: u32 = 17;
pub const TYPE_CHAR_ARRAY_OID: u32 = 1002;
pub const TYPE_CHAR_OID: u32 = 18;
pub const TYPE_DATE_ARRAY_OID: u32 = 1182;
pub const TYPE_DATE_OID: u32 = 1082;
pub const TYPE_FLOAT4_ARRAY_OID: u32 = 1021;
pub const TYPE_FLOAT4_OID: u32 = 700;
pub const TYPE_FLOAT8_ARRAY_OID: u32 = 1022;
pub const TYPE_FLOAT8_OID: u32 = 701;
pub const TYPE_INT2_ARRAY_OID: u32 = 1005;
pub const TYPE_INT2_OID: u32 = 21;
pub const TYPE_INT2_VECTOR_ARRAY_OID: u32 = 1006;
pub const TYPE_INT2_VECTOR_OID: u32 = 22;
pub const TYPE_INT4_ARRAY_OID: u32 = 1007;
pub const TYPE_INT4_OID: u32 = 23;
pub const TYPE_INT8_ARRAY_OID: u32 = 1016;
pub const TYPE_INT8_OID: u32 = 20;
pub const TYPE_INTERVAL_ARRAY_OID: u32 = 1187;
pub const TYPE_INTERVAL_OID: u32 = 1186;
pub const TYPE_JSONB_ARRAY_OID: u32 = 3807;
pub const TYPE_JSONB_OID: u32 = 3802;
pub const TYPE_LIST_OID_OID: u32 = 16_384;
pub const TYPE_NUMERIC_ARRAY_OID: u32 = 1231;
pub const TYPE_NUMERIC_OID: u32 = 1700;
pub const TYPE_OID_ARRAY_OID: u32 = 1028;
pub const TYPE_OID_OID: u32 = 26;
pub const TYPE_RECORD_ARRAY_OID: u32 = 2287;
pub const TYPE_RECORD_OID: u32 = 2249;
pub const TYPE_REGCLASS_ARRAY_OID: u32 = 2210;
pub const TYPE_REGCLASS_OID: u32 = 2205;
pub const TYPE_REGPROC_ARRAY_OID: u32 = 1008;
pub const TYPE_REGPROC_OID: u32 = 24;
pub const TYPE_REGTYPE_ARRAY_OID: u32 = 2211;
pub const TYPE_REGTYPE_OID: u32 = 2206;
pub const TYPE_TEXT_ARRAY_OID: u32 = 1009;
pub const TYPE_TEXT_OID: u32 = 25;
pub const TYPE_TIME_ARRAY_OID: u32 = 1183;
pub const TYPE_TIME_OID: u32 = 1083;
pub const TYPE_TIMESTAMP_ARRAY_OID: u32 = 1115;
pub const TYPE_TIMESTAMP_OID: u32 = 1114;
pub const TYPE_TIMESTAMPTZ_ARRAY_OID: u32 = 1185;
pub const TYPE_TIMESTAMPTZ_OID: u32 = 1184;
pub const TYPE_UUID_ARRAY_OID: u32 = 2951;
pub const TYPE_UUID_OID: u32 = 2950;
pub const TYPE_VARCHAR_ARRAY_OID: u32 = 1015;
pub const TYPE_VARCHAR_OID: u32 = 1043;
pub const TYPE_INT4RANGE_OID: u32 = 3904;
pub const TYPE_INT4RANGE_ARRAY_OID: u32 = 3905;
pub const TYPE_ANYRANGE_OID: u32 = 3831;
pub const TYPE_ANYCOMPATIBLERANGE_OID: u32 = 5080;
pub const TYPE_INT8RANGE_OID: u32 = 3926;
pub const TYPE_INT8RANGE_ARRAY_OID: u32 = 3927;
pub const TYPE_DATERANGE_OID: u32 = 3912;
pub const TYPE_DATERANGE_ARRAY_OID: u32 = 3913;
pub const TYPE_NUMRANGE_OID: u32 = 3906;
pub const TYPE_NUMRANGE_ARRAY_OID: u32 = 3907;
pub const TYPE_TSRANGE_OID: u32 = 3908;
pub const TYPE_TSRANGE_ARRAY_OID: u32 = 3909;
pub const TYPE_TSTZRANGE_OID: u32 = 3910;
pub const TYPE_TSTZRANGE_ARRAY_OID: u32 = 3911;

/// The first OID in PostgreSQL's system catalog that is not pinned during
/// bootstrapping.
///
/// See: <https://github.com/postgres/postgres/blob/aa0105141/src/include/access/transam.h#L173-L175>
pub const FIRST_UNPINNED_OID: u32 = 12000;

/// The first OID that is assigned by Materialize rather than PostgreSQL.
pub const FIRST_MATERIALIZE_OID: u32 = 16384;

/// The first OID that is assigned to user objects rather than system builtins.
pub const FIRST_USER_OID: u32 = 20_000;

// Postgres builtins in the "unpinned" OID range. We get to choose whatever OIDs
// we like for these builtins.
pub const FUNC_PG_EXPAND_ARRAY: u32 = 12000;

// Materialize-specific builtin OIDs.
pub const TYPE_LIST_OID: u32 = 16_384;
pub const TYPE_MAP_OID: u32 = 16_385;
pub const FUNC_CEIL_F32_OID: u32 = 16_386;
pub const FUNC_CONCAT_AGG_OID: u32 = 16_387;
pub const FUNC_CSV_EXTRACT_OID: u32 = 16_388;
pub const FUNC_CURRENT_TIMESTAMP_OID: u32 = 16_389;
pub const FUNC_FLOOR_F32_OID: u32 = 16_390;
pub const FUNC_LIST_APPEND_OID: u32 = 16_392;
pub const FUNC_LIST_CAT_OID: u32 = 16_393;
pub const FUNC_LIST_LENGTH_MAX_OID: u32 = 16_394;
pub const FUNC_LIST_LENGTH_OID: u32 = 16_395;
pub const FUNC_LIST_N_LAYERS_OID: u32 = 16_396;
pub const FUNC_LIST_PREPEND_OID: u32 = 16_397;
pub const FUNC_MAX_BOOL_OID: u32 = 16_398;
pub const FUNC_MIN_BOOL_OID: u32 = 16_399;
pub const FUNC_MZ_ALL_OID: u32 = 16_400;
pub const FUNC_MZ_ANY_OID: u32 = 16_401;
pub const FUNC_MZ_AVG_PROMOTION_DECIMAL_OID: u32 = 16_402;
pub const FUNC_MZ_AVG_PROMOTION_F32_OID: u32 = 16_403;
pub const FUNC_MZ_AVG_PROMOTION_F64_OID: u32 = 16_404;
pub const FUNC_MZ_AVG_PROMOTION_I32_OID: u32 = 16_405;
pub const FUNC_MZ_ENVIRONMENT_ID_OID: u32 = 16_407;
pub const FUNC_MZ_LOGICAL_TIMESTAMP_OID: u32 = 16_409;
pub const FUNC_MZ_RENDER_TYPMOD_OID: u32 = 16_410;
pub const FUNC_MZ_VERSION_OID: u32 = 16_411;
pub const FUNC_REGEXP_EXTRACT_OID: u32 = 16_412;
pub const FUNC_REPEAT_OID: u32 = 16_413;
pub const FUNC_ROUND_F32_OID: u32 = 16_414;
pub const FUNC_UNNEST_LIST_OID: u32 = 16_416;
pub const OP_CONCAT_ELEMENY_LIST_OID: u32 = 16_417;
pub const OP_CONCAT_LIST_ELEMENT_OID: u32 = 16_418;
pub const OP_CONCAT_LIST_LIST_OID: u32 = 16_419;
pub const OP_CONTAINED_JSONB_STRING_OID: u32 = 16_420;
pub const OP_CONTAINED_MAP_MAP_OID: u32 = 16_421;
pub const OP_CONTAINED_STRING_JSONB_OID: u32 = 16_422;
pub const OP_CONTAINS_ALL_KEYS_MAP_OID: u32 = 16_423;
pub const OP_CONTAINS_ANY_KEYS_MAP_OID: u32 = 16_424;
pub const OP_CONTAINS_JSONB_STRING_OID: u32 = 16_425;
pub const OP_CONTAINS_KEY_MAP_OID: u32 = 16_426;
pub const OP_CONTAINS_MAP_MAP_OID: u32 = 16_427;
pub const OP_CONTAINS_STRING_JSONB_OID: u32 = 16_428;
pub const OP_GET_VALUE_MAP_OID: u32 = 16_429;
pub const OP_GET_VALUES_MAP_OID: u32 = 16_430;
pub const OP_MOD_F32_OID: u32 = 16_431;
pub const OP_MOD_F64_OID: u32 = 16_432;
pub const OP_UNARY_PLUS_OID: u32 = 16_433;
pub const FUNC_MZ_SLEEP_OID: u32 = 16_434;
pub const FUNC_MZ_SESSION_ID_OID: u32 = 16_435;
pub const FUNC_MZ_UPTIME_OID: u32 = 16_436;
pub const FUNC_MZ_WORKERS_OID: u32 = 16_437;
pub const __DEPRECATED_TYPE_APD_OID: u32 = 16_438;
pub const FUNC_LIST_EQ_OID: u32 = 16_439;
pub const FUNC_MZ_ROW_SIZE: u32 = 16_440;
pub const FUNC_MAX_NUMERIC_OID: u32 = 16_441;
pub const FUNC_MIN_NUMERIC_OID: u32 = 16_442;
pub const FUNC_MZ_AVG_PROMOTION_I16_OID: u32 = 16_443;
pub const FUNC_LIST_AGG_OID: u32 = 16_444;
pub const FUNC_MZ_ERROR_IF_NULL_OID: u32 = 16_445;
pub const FUNC_MZ_DATE_BIN_UNIX_EPOCH_TS_OID: u32 = 16_446;
pub const FUNC_MZ_DATE_BIN_UNIX_EPOCH_TSTZ_OID: u32 = 16_447;
pub const FUNC_LIST_REMOVE_OID: u32 = 16_448;
pub const FUNC_MZ_DATE_BIN_HOPPING_UNIX_EPOCH_TS_OID: u32 = 16_449;
pub const FUNC_MZ_DATE_BIN_HOPPING_UNIX_EPOCH_TSTZ_OID: u32 = 16_450;
pub const FUNC_MZ_DATE_BIN_HOPPING_TS_OID: u32 = 16_451;
pub const FUNC_MZ_DATE_BIN_HOPPING_TSTZ_OID: u32 = 16_452;
pub const FUNC_MZ_TYPE_NAME: u32 = 16_453;
pub const TYPE_ANYCOMPATIBLELIST_OID: u32 = 16_454;
pub const TYPE_ANYCOMPATIBLEMAP_OID: u32 = 16_455;
pub const FUNC_MAP_LENGTH_OID: u32 = 16_456;
pub const FUNC_MZ_PANIC_OID: u32 = 16_457;
pub const FUNC_MZ_VERSION_NUM_OID: u32 = 16_458;
pub const FUNC_TRUNC_F32_OID: u32 = 16_459;
pub const TYPE_UINT2_OID: u32 = 16_460;
pub const TYPE_UINT2_ARRAY_OID: u32 = 16_461;
pub const TYPE_UINT4_OID: u32 = 16_462;
pub const TYPE_UINT4_ARRAY_OID: u32 = 16_463;
pub const TYPE_UINT8_OID: u32 = 16_464;
pub const TYPE_UINT8_ARRAY_OID: u32 = 16_465;
pub const FUNC_ADD_UINT16: u32 = 16_466;
pub const FUNC_ADD_UINT32: u32 = 16_467;
pub const FUNC_ADD_UINT64: u32 = 16_468;
pub const FUNC_SUB_UINT16: u32 = 16_469;
pub const FUNC_SUB_UINT32: u32 = 16_470;
pub const FUNC_SUB_UINT64: u32 = 16_471;
pub const FUNC_MUL_UINT16: u32 = 16_472;
pub const FUNC_MUL_UINT32: u32 = 16_473;
pub const FUNC_MUL_UINT64: u32 = 16_474;
pub const FUNC_DIV_UINT16: u32 = 16_475;
pub const FUNC_DIV_UINT32: u32 = 16_476;
pub const FUNC_DIV_UINT64: u32 = 16_477;
pub const FUNC_MOD_UINT16: u32 = 16_478;
pub const FUNC_MOD_UINT32: u32 = 16_479;
pub const FUNC_MOD_UINT64: u32 = 16_480;
pub const FUNC_AND_UINT16: u32 = 16_481;
pub const FUNC_AND_UINT32: u32 = 16_482;
pub const FUNC_AND_UINT64: u32 = 16_483;
pub const FUNC_OR_UINT16: u32 = 16_484;
pub const FUNC_OR_UINT32: u32 = 16_485;
pub const FUNC_OR_UINT64: u32 = 16_486;
pub const FUNC_XOR_UINT16: u32 = 16_487;
pub const FUNC_XOR_UINT32: u32 = 16_488;
pub const FUNC_XOR_UINT64: u32 = 16_489;
pub const FUNC_SHIFT_LEFT_UINT16: u32 = 16_490;
pub const FUNC_SHIFT_LEFT_UINT32: u32 = 16_491;
pub const FUNC_SHIFT_LEFT_UINT64: u32 = 16_492;
pub const FUNC_SHIFT_RIGHT_UINT16: u32 = 16_493;
pub const FUNC_SHIFT_RIGHT_UINT32: u32 = 16_494;
pub const FUNC_SHIFT_RIGHT_UINT64: u32 = 16_495;
pub const FUNC_MAX_UINT16_OID: u32 = 16_496;
pub const FUNC_MAX_UINT32_OID: u32 = 16_497;
pub const FUNC_MAX_UINT64_OID: u32 = 16_498;
pub const FUNC_MIN_UINT16_OID: u32 = 16_499;
pub const FUNC_MIN_UINT32_OID: u32 = 16_500;
pub const FUNC_MIN_UINT64_OID: u32 = 16_501;
pub const FUNC_SUM_UINT16_OID: u32 = 16_502;
pub const FUNC_SUM_UINT32_OID: u32 = 16_503;
pub const FUNC_SUM_UINT64_OID: u32 = 16_504;
pub const FUNC_AVG_UINT16_OID: u32 = 16_505;
pub const FUNC_AVG_UINT32_OID: u32 = 16_506;
pub const FUNC_AVG_UINT64_OID: u32 = 16_507;
pub const FUNC_MOD_UINT16_OID: u32 = 16_508;
pub const FUNC_MOD_UINT32_OID: u32 = 16_509;
pub const FUNC_MOD_UINT64_OID: u32 = 16_510;
pub const FUNC_STDDEV_UINT16_OID: u32 = 16_511;
pub const FUNC_STDDEV_UINT32_OID: u32 = 16_512;
pub const FUNC_STDDEV_UINT64_OID: u32 = 16_513;
pub const FUNC_STDDEV_POP_UINT16_OID: u32 = 16_514;
pub const FUNC_STDDEV_POP_UINT32_OID: u32 = 16_515;
pub const FUNC_STDDEV_POP_UINT64_OID: u32 = 16_516;
pub const FUNC_STDDEV_SAMP_UINT16_OID: u32 = 16_517;
pub const FUNC_STDDEV_SAMP_UINT32_OID: u32 = 16_518;
pub const FUNC_STDDEV_SAMP_UINT64_OID: u32 = 16_519;
pub const FUNC_VARIANCE_UINT16_OID: u32 = 16_520;
pub const FUNC_VARIANCE_UINT32_OID: u32 = 16_521;
pub const FUNC_VARIANCE_UINT64_OID: u32 = 16_522;
pub const FUNC_VAR_POP_UINT16_OID: u32 = 16_523;
pub const FUNC_VAR_POP_UINT32_OID: u32 = 16_524;
pub const FUNC_VAR_POP_UINT64_OID: u32 = 16_525;
pub const FUNC_VAR_SAMP_UINT16_OID: u32 = 16_526;
pub const FUNC_VAR_SAMP_UINT32_OID: u32 = 16_527;
pub const FUNC_VAR_SAMP_UINT64_OID: u32 = 16_528;
pub const FUNC_BIT_NOT_UINT16_OID: u32 = 16_529;
pub const FUNC_BIT_NOT_UINT32_OID: u32 = 16_530;
pub const FUNC_BIT_NOT_UINT64_OID: u32 = 16_531;
pub const FUNC_LT_UINT16_OID: u32 = 16_532;
pub const FUNC_LT_UINT32_OID: u32 = 16_533;
pub const FUNC_LT_UINT64_OID: u32 = 16_534;
pub const FUNC_LTE_UINT16_OID: u32 = 16_535;
pub const FUNC_LTE_UINT32_OID: u32 = 16_536;
pub const FUNC_LTE_UINT64_OID: u32 = 16_537;
pub const FUNC_GT_UINT16_OID: u32 = 16_538;
pub const FUNC_GT_UINT32_OID: u32 = 16_539;
pub const FUNC_GT_UINT64_OID: u32 = 16_540;
pub const FUNC_GTE_UINT16_OID: u32 = 16_541;
pub const FUNC_GTE_UINT32_OID: u32 = 16_542;
pub const FUNC_GTE_UINT64_OID: u32 = 16_543;
pub const FUNC_EQ_UINT16_OID: u32 = 16_544;
pub const FUNC_EQ_UINT32_OID: u32 = 16_545;
pub const FUNC_EQ_UINT64_OID: u32 = 16_546;
pub const FUNC_NOT_EQ_UINT16_OID: u32 = 16_547;
pub const FUNC_NOT_EQ_UINT32_OID: u32 = 16_548;
pub const FUNC_NOT_EQ_UINT64_OID: u32 = 16_549;
pub const FUNC_MZ_AVG_PROMOTION_U16_OID: u32 = 16_550;
pub const FUNC_MZ_AVG_PROMOTION_U32_OID: u32 = 16_551;
pub const TYPE_MZ_TIMESTAMP_OID: u32 = 16_552;
pub const TYPE_MZ_TIMESTAMP_ARRAY_OID: u32 = 16_553;
pub const FUNC_MZ_TIMESTAMP_EQ_MZ_TIMESTAMP_OID: u32 = 16_554;
pub const FUNC_MZ_TIMESTAMP_NOT_EQ_MZ_TIMESTAMP_OID: u32 = 16_555;
pub const FUNC_MZ_TIMESTAMP_LT_MZ_TIMESTAMP_OID: u32 = 16_556;
pub const FUNC_MZ_TIMESTAMP_LTE_MZ_TIMESTAMP_OID: u32 = 16_557;
pub const FUNC_MZ_TIMESTAMP_GT_MZ_TIMESTAMP_OID: u32 = 16_558;
pub const FUNC_MZ_TIMESTAMP_GTE_MZ_TIMESTAMP_OID: u32 = 16_559;
pub const FUNC_MZ_NOW_OID: u32 = 16_560;
pub const FUNC_MAX_MZ_TIMESTAMP_OID: u32 = 16_561;
pub const FUNC_MIN_MZ_TIMESTAMP_OID: u32 = 16_562;
pub const FUNC_DATE_FROM_TEXT: u32 = 16_563;
pub const FUNC_CEILING_F32_OID: u32 = 16_564;
pub const FUNC_PG_UUID_GENERATE_V5: u32 = 16_565;
pub const TYPE_MZ_ACL_ITEM_OID: u32 = 16_566;
pub const TYPE_MZ_ACL_ITEM_ARRAY_OID: u32 = 16_567;
pub const FUNC_MZ_ACL_ITEM_EQ_MZ_ACL_ITEM_OID: u32 = 16_568;
pub const FUNC_MZ_ACL_ITEM_NOT_EQ_MZ_ACL_ITEM_OID: u32 = 16_569;
pub const FUNC_MAKE_MZ_ACL_ITEM_OID: u32 = 16_570;
pub const FUNC_MZ_ACL_ITEM_GRANTOR_OID: u32 = 16_571;
pub const FUNC_MZ_ACL_ITEM_GRANTEE_OID: u32 = 16_572;
pub const FUNC_MZ_ACL_ITEM_PRIVILEGES_OID: u32 = 16_573;
pub const FUNC_IS_RBAC_ENABLED_OID: u32 = 16_574;
pub const FUNC_MZ_ACL_ITEM_CONTAINS_PRIVILEGE_OID: u32 = 16_575;
pub const FUNC_MZ_VALIDATE_PRIVILEGES_OID: u32 = 16_576;
pub const FUNC_ROLE_OID_OID: u32 = 16_577;
pub const FUNC_MZ_NAME_TO_GLOBAL_ID_FULL_QUAL: u32 = 16_578;
pub const FUNC_MZ_NAME_TO_GLOBAL_ID_ANY_SCHEMA: u32 = 16_579;
pub const FUNC_MZ_NAME_TO_GLOBAL_ID_ONE_SCHEMA: u32 = 16_580;
pub const FUNC_MZ_NAME_TO_GLOBAL_ID_ITEM_NAME: u32 = 16_581;
pub const FUNC_MZ_GLOBAL_ID_TO_NAME: u32 = 16_582;
