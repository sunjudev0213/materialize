// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Metrics HTTP endpoints.

use axum::response::IntoResponse;
use axum::TypedHeader;
use headers::ContentType;
use http::StatusCode;
use mz_ore::metrics::MetricsRegistry;
use prometheus::Encoder;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum MetricsVariant {
    Regular,
    ThirdPartyVisible,
}

/// Serves metrics from the selected metrics registry variant.
pub async fn handle_prometheus(
    registry: &MetricsRegistry,
    variant: MetricsVariant,
) -> impl IntoResponse {
    let metric_families = match variant {
        MetricsVariant::Regular => registry.gather(),
        MetricsVariant::ThirdPartyVisible => registry.gather_third_party_visible(),
    };
    let mut buffer = Vec::new();
    let encoder = prometheus::TextEncoder::new();
    encoder
        .encode(&metric_families, &mut buffer)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok::<_, (StatusCode, String)>((TypedHeader(ContentType::text()), buffer))
}
