// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

//! Debug utility for stashes.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Write},
    path::PathBuf,
    process,
    str::FromStr,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use clap::Parser;
use once_cell::sync::Lazy;

use mz_adapter::{
    catalog::{
        storage::{self as catalog, BootstrapArgs},
        Catalog, ClusterReplicaSizeMap, Config,
    },
    DUMMY_AVAILABILITY_ZONE,
};
use mz_build_info::{build_info, BuildInfo};
use mz_ore::{
    cli::{self, CliConfig},
    error::ErrorExt,
    metrics::MetricsRegistry,
    now::SYSTEM_TIME,
};
use mz_secrets::InMemorySecretsController;
use mz_sql::{catalog::EnvironmentId, session::vars::ConnectionCounter};
use mz_stash::{Stash, StashFactory};
use mz_storage_client::controller as storage;

pub const BUILD_INFO: BuildInfo = build_info!();
pub static VERSION: Lazy<String> = Lazy::new(|| BUILD_INFO.human_version());

#[derive(Parser, Debug)]
#[clap(name = "stash", next_line_help = true, version = VERSION.as_str())]
pub struct Args {
    #[clap(long, env = "POSTGRES_URL")]
    postgres_url: String,

    #[clap(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    Dump {
        target: Option<PathBuf>,
    },
    Edit {
        collection: String,
        key: serde_json::Value,
        value: serde_json::Value,
    },
    /// Checks if the specified stash could be upgraded from its state to the
    /// adapter catalog at the version of this binary. Prints a success message
    /// or error message. Exits with 0 if the upgrade would succeed, otherwise
    /// non-zero. Can be used on a running environmentd. Operates without
    /// interfering with it or committing any data to that stash.
    UpgradeCheck {
        cluster_replica_sizes: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    let args = cli::parse_args(CliConfig {
        env_prefix: Some("MZ_STASH_DEBUG_"),
        enable_version_flag: true,
    });
    if let Err(err) = run(args).await {
        eprintln!("stash: fatal: {}", err.display_with_causes());
        process::exit(1);
    }
}

async fn run(args: Args) -> Result<(), anyhow::Error> {
    let tls = mz_postgres_util::make_tls(&tokio_postgres::config::Config::from_str(
        &args.postgres_url,
    )?)?;
    let factory = StashFactory::new(&MetricsRegistry::new());
    let mut stash = factory
        .open_readonly(args.postgres_url.clone(), None, tls.clone())
        .await?;
    let usage = Usage::from_stash(&mut stash).await?;

    match args.action {
        Action::Dump { target } => {
            let target: Box<dyn Write> = if let Some(path) = target {
                Box::new(File::create(path)?)
            } else {
                Box::new(io::stdout().lock())
            };
            dump(stash, usage, target).await
        }
        Action::Edit {
            collection,
            key,
            value,
        } => {
            // edit needs a mutable stash, so reconnect.
            let stash = factory.open(args.postgres_url, None, tls).await?;
            edit(stash, usage, collection, key, value).await
        }
        Action::UpgradeCheck {
            cluster_replica_sizes,
        } => {
            // upgrade needs fake writes, so use a savepoint.
            let stash = factory.open_savepoint(args.postgres_url, tls).await?;
            let cluster_replica_sizes: ClusterReplicaSizeMap = match cluster_replica_sizes {
                None => Default::default(),
                Some(json) => serde_json::from_str(&json).context("parsing replica size map")?,
            };
            upgrade_check(stash, usage, cluster_replica_sizes).await
        }
    }
}

async fn edit(
    mut stash: Stash,
    usage: Usage,
    collection: String,
    key: serde_json::Value,
    value: serde_json::Value,
) -> Result<(), anyhow::Error> {
    let prev = usage.edit(&mut stash, collection, key, value).await?;
    println!("previous value: {:?}", prev);
    Ok(())
}

async fn dump(mut stash: Stash, usage: Usage, mut target: impl Write) -> Result<(), anyhow::Error> {
    let data = usage.dump(&mut stash).await?;
    serde_json::to_writer_pretty(&mut target, &data)?;
    write!(&mut target, "\n")?;
    Ok(())
}
async fn upgrade_check(
    stash: Stash,
    usage: Usage,
    cluster_replica_sizes: ClusterReplicaSizeMap,
) -> Result<(), anyhow::Error> {
    let msg = usage.upgrade_check(stash, cluster_replica_sizes).await?;
    println!("{msg}");
    Ok(())
}

#[derive(Debug)]
enum Usage {
    Catalog,
    Storage,
}

impl Usage {
    fn all_usages() -> Vec<Usage> {
        vec![Self::Catalog, Self::Storage]
    }

    /// Returns an error if there is any overlap of collection names from all
    /// Usages.
    fn verify_all_usages() -> Result<(), anyhow::Error> {
        let mut all_names = BTreeSet::new();
        for usage in Self::all_usages() {
            let mut names = usage.names();
            if names.is_subset(&all_names) {
                anyhow::bail!(
                    "duplicate names; cannot determine usage: {:?}",
                    all_names.intersection(&names)
                );
            }
            all_names.append(&mut names);
        }
        Ok(())
    }

    async fn from_stash(stash: &mut Stash) -> Result<Self, anyhow::Error> {
        // Determine which usage we are on by any collection matching any
        // expected name of a usage. To do that safely, we need to verify that
        // there is no overlap between expected names.
        Self::verify_all_usages()?;

        let names = BTreeSet::from_iter(stash.collections().await?.into_values());
        for usage in Self::all_usages() {
            // Some TypedCollections exist before any entries have been written
            // to a collection, so `stash.collections()` won't return it, and we
            // have to look for any overlap to indicate which stash we are on.
            if usage.names().intersection(&names).next().is_some() {
                return Ok(usage);
            }
        }
        anyhow::bail!("could not determine usage: unknown names: {:?}", names);
    }

    fn names(&self) -> BTreeSet<String> {
        BTreeSet::from_iter(
            match self {
                Self::Catalog => catalog::ALL_COLLECTIONS,
                Self::Storage => storage::ALL_COLLECTIONS,
            }
            .iter()
            .map(|s| s.to_string()),
        )
    }

    async fn dump(
        &self,
        stash: &mut Stash,
    ) -> Result<BTreeMap<&str, serde_json::Value>, anyhow::Error> {
        let mut collections = Vec::new();
        let collection_names = BTreeSet::from_iter(stash.collections().await?.into_values());
        macro_rules! dump_col {
            ($col:expr) => {
                // Collections might not yet exist.
                if collection_names.contains($col.name()) {
                    collections.push(($col.name(), serde_json::to_value($col.iter(stash).await?)?));
                }
            };
        }

        match self {
            Usage::Catalog => {
                dump_col!(catalog::COLLECTION_CONFIG);
                dump_col!(catalog::COLLECTION_ID_ALLOC);
                dump_col!(catalog::COLLECTION_SYSTEM_GID_MAPPING);
                dump_col!(catalog::COLLECTION_CLUSTERS);
                dump_col!(catalog::COLLECTION_CLUSTER_INTROSPECTION_SOURCE_INDEX);
                dump_col!(catalog::COLLECTION_CLUSTER_REPLICAS);
                dump_col!(catalog::COLLECTION_DATABASE);
                dump_col!(catalog::COLLECTION_SCHEMA);
                dump_col!(catalog::COLLECTION_ITEM);
                dump_col!(catalog::COLLECTION_ROLE);
                dump_col!(catalog::COLLECTION_TIMESTAMP);
                dump_col!(catalog::COLLECTION_SYSTEM_CONFIGURATION);
                dump_col!(catalog::COLLECTION_AUDIT_LOG);
                dump_col!(catalog::COLLECTION_STORAGE_USAGE);
            }
            Usage::Storage => {
                dump_col!(storage::METADATA_COLLECTION);
                dump_col!(storage::METADATA_EXPORT);
            }
        }
        let data = BTreeMap::from_iter(collections);
        let data_names = BTreeSet::from_iter(data.keys().map(|k| k.to_string()));
        if data_names != self.names() {
            // This is useful to know because it can either be fine (collection
            // not yet created) or a programming error where this file was not
            // updated after adding a collection.
            eprintln!(
                "unexpected names, verify this program knows about all collections: got {:?}, expected {:?}",
                data_names,
                self.names()
            );
        }
        Ok(data)
    }

    async fn edit(
        &self,
        stash: &mut Stash,
        collection: String,
        key: serde_json::Value,
        value: serde_json::Value,
    ) -> Result<Option<serde_json::Value>, anyhow::Error> {
        macro_rules! edit_col {
            ($col:expr) => {
                if collection == $col.name() {
                    let key = serde_json::from_value(key)?;
                    let value = serde_json::from_value(value)?;
                    let (prev, _next) = $col
                        .upsert_key(stash, key, move |_| {
                            Ok::<_, std::convert::Infallible>(value)
                        })
                        .await??;
                    return Ok(prev.map(|v| serde_json::to_value(v).unwrap()));
                }
            };
        }

        match self {
            Usage::Catalog => {
                edit_col!(catalog::COLLECTION_CONFIG);
                edit_col!(catalog::COLLECTION_ID_ALLOC);
                edit_col!(catalog::COLLECTION_SYSTEM_GID_MAPPING);
                edit_col!(catalog::COLLECTION_CLUSTERS);
                edit_col!(catalog::COLLECTION_CLUSTER_INTROSPECTION_SOURCE_INDEX);
                edit_col!(catalog::COLLECTION_CLUSTER_REPLICAS);
                edit_col!(catalog::COLLECTION_DATABASE);
                edit_col!(catalog::COLLECTION_SCHEMA);
                edit_col!(catalog::COLLECTION_ITEM);
                edit_col!(catalog::COLLECTION_ROLE);
                edit_col!(catalog::COLLECTION_TIMESTAMP);
                edit_col!(catalog::COLLECTION_SYSTEM_CONFIGURATION);
                edit_col!(catalog::COLLECTION_AUDIT_LOG);
                edit_col!(catalog::COLLECTION_STORAGE_USAGE);
            }
            Usage::Storage => {
                edit_col!(storage::METADATA_COLLECTION);
                edit_col!(storage::METADATA_EXPORT);
            }
        }
        anyhow::bail!("unknown collection {} for stash {:?}", collection, self)
    }

    async fn upgrade_check(
        &self,
        stash: Stash,
        cluster_replica_sizes: ClusterReplicaSizeMap,
    ) -> Result<String, anyhow::Error> {
        if !matches!(self, Self::Catalog) {
            anyhow::bail!("upgrade_check expected Catalog stash, found {:?}", self);
        }

        let metrics_registry = &MetricsRegistry::new();
        let now = SYSTEM_TIME.clone();
        let storage = mz_adapter::catalog::storage::Connection::open(
            stash,
            now.clone(),
            &BootstrapArgs {
                default_cluster_replica_size: "1".into(),
                builtin_cluster_replica_size: "1".into(),
                default_availability_zone: DUMMY_AVAILABILITY_ZONE.into(),
                bootstrap_role: None,
            },
        )
        .await?;
        let secrets_reader = Arc::new(InMemorySecretsController::new());

        let (_catalog, _, _, last_catalog_version) = Catalog::open(Config {
            storage,
            unsafe_mode: true,
            build_info: &BUILD_INFO,
            environment_id: EnvironmentId::for_tests(),
            now,
            skip_migrations: false,
            metrics_registry,
            cluster_replica_sizes,
            default_storage_cluster_size: None,
            system_parameter_defaults: Default::default(),
            availability_zones: vec![],
            secrets_reader,
            egress_ips: vec![],
            aws_principal_context: None,
            aws_privatelink_availability_zones: None,
            system_parameter_frontend: None,
            storage_usage_retention_period: None,
            connection_context: None,
            active_connection_count: Arc::new(Mutex::new(ConnectionCounter::new(0))),
        })
        .await?;

        Ok(format!(
            "catalog upgrade from {} to {} would succeed",
            last_catalog_version,
            BUILD_INFO.human_version(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_all_usages() {
        Usage::verify_all_usages().unwrap();
    }
}
