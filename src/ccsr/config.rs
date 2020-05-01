// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::client::Client;
use crate::tls::{CertDetails, Identity, Certificate};

/// Represents the Confluent Schema Registry you want to connect to, including
/// potential TLS configuration.
#[serde(rename_all = "snake_case")]
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClientConfig {
    pub url: Url,
    pub root_certs: Vec<Certificate>,
    pub identity: Option<Identity>,
}

impl ClientConfig {
    pub fn new(url: Url) -> ClientConfig {
        ClientConfig {
            url,
            root_certs: Vec::new(),
            identity: None,
        }
    }
    pub fn add_root_certificate(mut self, cert: Certificate) -> ClientConfig {
        self.root_certs.push(cert);
        self
    }
    pub fn identity(mut self, identity: Identity) -> ClientConfig {
        self.identity = Some(identity);
        self
    }
    pub fn build(&self) -> Client {
        let mut builder = reqwest::Client::builder();

        for root_cert in &self.root_certs {
            builder = builder.add_root_certificate(root_cert.clone().into());
        }

        if let Some(ident) = &self.identity {
            match ident.cert {
                CertDetails::PEM(_) => {
                    builder = builder.use_rustls_tls();
                }
                CertDetails::DER(_, _) => {
                    builder = builder.use_native_tls();
                }
            }
            builder = builder.identity(ident.clone().into());
        }

        let inner = builder
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        Client {
            inner,
            url: self.url.clone(),
        }
    }
}
