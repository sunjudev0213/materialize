// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use chrono::Utc;
use clap::ArgEnum;
use futures::stream::{BoxStream, StreamExt};
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Affinity, Container, ContainerPort, EnvVar, EnvVarSource, ObjectFieldSelector, Pod,
    PodAffinityTerm, PodAntiAffinity, PodSpec, PodTemplateSpec, ResourceRequirements, Secret,
    Service as K8sService, ServicePort, ServiceSpec,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, LabelSelectorRequirement};
use kube::api::{Api, DeleteParams, ListParams, ObjectMeta, Patch, PatchParams};
use kube::client::Client;
use kube::error::Error;
use kube::runtime::{watcher, WatchStreamExt};
use kube::ResourceExt;
use maplit::btreemap;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::warn;

use mz_cloud_resources::crd::vpc_endpoint::v1::VpcEndpoint;
use mz_cloud_resources::AwsExternalIdPrefix;
use mz_orchestrator::{
    LabelSelectionLogic, NamespacedOrchestrator, Orchestrator, Service, ServiceAssignments,
    ServiceConfig, ServiceEvent, ServiceStatus,
};
use mz_orchestrator::{LabelSelector as MzLabelSelector, ServiceProcessMetrics};

pub mod cloud_resource_controller;
pub mod secrets;
pub mod util;

const FIELD_MANAGER: &str = "environmentd";

/// Configures a [`KubernetesOrchestrator`].
#[derive(Debug, Clone)]
pub struct KubernetesOrchestratorConfig {
    /// The name of a Kubernetes context to use, if the Kubernetes configuration
    /// is loaded from the local kubeconfig.
    pub context: String,
    /// Labels to install on every service created by the orchestrator.
    pub service_labels: HashMap<String, String>,
    /// Node selector to install on every service created by the orchestrator.
    pub service_node_selector: HashMap<String, String>,
    /// The service account that each service should run as, if any.
    pub service_account: Option<String>,
    /// The image pull policy to set for services created by the orchestrator.
    pub image_pull_policy: KubernetesImagePullPolicy,
    /// An AWS external ID prefix to use when making AWS operations on behalf
    /// of the environment.
    pub aws_external_id_prefix: Option<AwsExternalIdPrefix>,
}

/// Specifies whether Kubernetes should pull Docker images when creating pods.
#[derive(ArgEnum, Debug, Clone, Copy)]
pub enum KubernetesImagePullPolicy {
    /// Always pull the Docker image from the registry.
    Always,
    /// Pull the Docker image only if the image is not present.
    IfNotPresent,
    /// Never pull the Docker image.
    Never,
}

impl fmt::Display for KubernetesImagePullPolicy {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            KubernetesImagePullPolicy::Always => f.write_str("Always"),
            KubernetesImagePullPolicy::IfNotPresent => f.write_str("IfNotPresent"),
            KubernetesImagePullPolicy::Never => f.write_str("Never"),
        }
    }
}

/// An orchestrator backed by Kubernetes.
pub struct KubernetesOrchestrator {
    client: Client,
    kubernetes_namespace: String,
    config: KubernetesOrchestratorConfig,
    secret_api: Api<Secret>,
    vpc_endpoint_api: Api<VpcEndpoint>,
    namespaces: Mutex<HashMap<String, Arc<dyn NamespacedOrchestrator>>>,
}

impl fmt::Debug for KubernetesOrchestrator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("KubernetesOrchestrator").finish()
    }
}

impl KubernetesOrchestrator {
    /// Creates a new Kubernetes orchestrator from the provided configuration.
    pub async fn new(
        config: KubernetesOrchestratorConfig,
    ) -> Result<KubernetesOrchestrator, anyhow::Error> {
        let (client, kubernetes_namespace) = util::create_client(config.context.clone()).await?;
        Ok(KubernetesOrchestrator {
            client: client.clone(),
            kubernetes_namespace,
            config,
            secret_api: Api::default_namespaced(client.clone()),
            vpc_endpoint_api: Api::default_namespaced(client),
            namespaces: Mutex::new(HashMap::new()),
        })
    }
}

impl Orchestrator for KubernetesOrchestrator {
    fn namespace(&self, namespace: &str) -> Arc<dyn NamespacedOrchestrator> {
        let mut namespaces = self.namespaces.lock().expect("lock poisoned");
        Arc::clone(namespaces.entry(namespace.into()).or_insert_with(|| {
            Arc::new(NamespacedKubernetesOrchestrator {
                metrics_api: Api::default_namespaced(self.client.clone()),
                service_api: Api::default_namespaced(self.client.clone()),
                stateful_set_api: Api::default_namespaced(self.client.clone()),
                pod_api: Api::default_namespaced(self.client.clone()),
                kubernetes_namespace: self.kubernetes_namespace.clone(),
                namespace: namespace.into(),
                config: self.config.clone(),
                service_scales: std::sync::Mutex::new(HashMap::new()),
            })
        }))
    }
}

struct NamespacedKubernetesOrchestrator {
    metrics_api: Api<PodMetrics>,
    service_api: Api<K8sService>,
    stateful_set_api: Api<StatefulSet>,
    pod_api: Api<Pod>,
    kubernetes_namespace: String,
    namespace: String,
    config: KubernetesOrchestratorConfig,
    service_scales: std::sync::Mutex<HashMap<String, NonZeroUsize>>,
}

impl fmt::Debug for NamespacedKubernetesOrchestrator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("NamespacedKubernetesOrchestrator")
            .field("kubernetes_namespace", &self.kubernetes_namespace)
            .field("namespace", &self.namespace)
            .field("config", &self.config)
            .finish()
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct PodMetricsContainer {
    pub name: String,
    pub usage: PodMetricsContainerUsage,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PodMetricsContainerUsage {
    pub cpu: Quantity,
    pub memory: Quantity,
}

#[derive(Deserialize, Clone, Debug)]
pub struct PodMetrics {
    pub metadata: ObjectMeta,
    pub timestamp: String,
    pub window: String,
    pub containers: Vec<PodMetricsContainer>,
}

impl k8s_openapi::Resource for PodMetrics {
    const GROUP: &'static str = "metrics.k8s.io";
    const KIND: &'static str = "PodMetrics";
    const VERSION: &'static str = "v1beta1";
    const API_VERSION: &'static str = "metrics.k8s.io/v1beta1";
    const URL_PATH_SEGMENT: &'static str = "pods";

    type Scope = k8s_openapi::NamespaceResourceScope;
}

impl k8s_openapi::Metadata for PodMetrics {
    type Ty = ObjectMeta;

    fn metadata(&self) -> &Self::Ty {
        &self.metadata
    }

    fn metadata_mut(&mut self) -> &mut Self::Ty {
        &mut self.metadata
    }
}

impl NamespacedKubernetesOrchestrator {
    /// Return a `ListParams` instance that limits results to the namespace
    /// assigned to this orchestrator.
    fn list_pod_params(&self) -> ListParams {
        let ns_selector = format!(
            "environmentd.materialize.cloud/namespace={}",
            self.namespace
        );
        ListParams::default().labels(&ns_selector)
    }
    /// Convert a higher-level label key to the actual one we
    /// will give to Kubernetes
    fn make_label_key(&self, key: &str) -> String {
        format!("{}.environmentd.materialize.cloud/{}", self.namespace, key)
    }
    fn label_selector_to_k8s(
        &self,
        MzLabelSelector { label_name, logic }: MzLabelSelector,
    ) -> Result<LabelSelectorRequirement, anyhow::Error> {
        let (operator, values) = match logic {
            LabelSelectionLogic::Eq { value } => Ok(("In", vec![value])),
            LabelSelectionLogic::NotEq { value } => Ok(("NotIn", vec![value])),
            LabelSelectionLogic::Exists => Ok(("Exists", vec![])),
            LabelSelectionLogic::NotExists => Ok(("DoesNotExist", vec![])),
            LabelSelectionLogic::InSet { values } => {
                if values.is_empty() {
                    Err(anyhow!(
                        "Invalid selector logic for {label_name}: empty `in` set"
                    ))
                } else {
                    Ok(("In", values))
                }
            }
            LabelSelectionLogic::NotInSet { values } => {
                if values.is_empty() {
                    Err(anyhow!(
                        "Invalid selector logic for {label_name}: empty `notin` set"
                    ))
                } else {
                    Ok(("NotIn", values))
                }
            }
        }?;
        let lsr = LabelSelectorRequirement {
            key: self.make_label_key(&label_name),
            operator: operator.to_string(),
            values: Some(values),
        };
        Ok(lsr)
    }
}

#[derive(Debug)]
struct ScaledQuantity {
    integral_part: u64,
    exponent: i8,
    base10: bool,
}

impl ScaledQuantity {
    pub fn try_to_integer(&self, scale: i8, base10: bool) -> Option<u64> {
        if base10 != self.base10 {
            return None;
        }
        let exponent = self.exponent - scale;
        let mut result = self.integral_part;
        let base = if self.base10 { 10 } else { 2 };
        if exponent < 0 {
            for _ in exponent..0 {
                result /= base;
            }
        } else {
            for _ in 0..exponent {
                result = result.checked_mul(2)?;
            }
        }
        Some(result)
    }
}

// Parse a k8s `Quantity` object
// into a numeric value.
//
// This is intended to support collecting CPU and Memory data.
// Thus, there are a few that things Kubernetes attempts to do, that we don't,
// because I've never observed metrics-server specifically sending them:
// (1) Handle negative numbers (because it's not useful for that use-case)
// (2) Handle non-integers (because I have never observed them being actually sent)
// (3) Handle scientific notation (e.g. 1.23e2)
fn parse_k8s_quantity(s: &str) -> Result<ScaledQuantity, anyhow::Error> {
    const DEC_SUFFIXES: &[(&str, i8)] = &[
        ("n", -9),
        ("u", -6),
        ("m", -3),
        ("", 0),
        ("k", 3), // yep, intentionally lowercase.
        ("M", 6),
        ("G", 9),
        ("T", 12),
        ("P", 15),
        ("E", 18),
    ];
    const BIN_SUFFIXES: &[(&str, i8)] = &[
        ("", 0),
        ("Ki", 10),
        ("Mi", 20),
        ("Gi", 30),
        ("Ti", 40),
        ("Pi", 50),
        ("Ei", 60),
    ];

    let (positive, s) = match s.chars().next() {
        Some('+') => (true, &s[1..]),
        Some('-') => (false, &s[1..]),
        _ => (true, s),
    };

    if !positive {
        anyhow::bail!("Negative numbers not supported")
    }

    fn is_suffix_char(ch: char) -> bool {
        "numkMGTPEKi".contains(ch)
    }
    let (num, suffix) = match s.find(is_suffix_char) {
        None => (s, ""),
        Some(idx) => s.split_at(idx),
    };
    let num: u64 = num.parse()?;
    let (exponent, base10) = if let Some((_, exponent)) =
        DEC_SUFFIXES.iter().find(|(target, _)| suffix == *target)
    {
        (exponent, true)
    } else if let Some((_, exponent)) = BIN_SUFFIXES.iter().find(|(target, _)| suffix == *target) {
        (exponent, false)
    } else {
        anyhow::bail!("Unrecognized suffix: {suffix}");
    };
    Ok(ScaledQuantity {
        integral_part: num,
        exponent: *exponent,
        base10,
    })
}

#[async_trait]
impl NamespacedOrchestrator for NamespacedKubernetesOrchestrator {
    async fn fetch_service_metrics(
        &self,
        id: &str,
    ) -> Result<Vec<ServiceProcessMetrics>, anyhow::Error> {
        let Some(&scale) = self.service_scales.lock().expect("poisoned lock").get(id) else {
            // This should have been set in `ensure_service`.
            tracing::error!("Failed to get scale for {id}");
            anyhow::bail!("Failed to get scale for {id}");
        };
        /// Get metrics for a particular service and process, converting them into a sane (i.e., numeric) format.
        ///
        /// Note that we want to keep going even if a lookup fails for whatever reason,
        /// so this function is infallible. If we fail to get cpu or memory for a particular pod,
        /// we just log a warning and install `None` in the returned struct.
        async fn get_metrics(
            self_: &NamespacedKubernetesOrchestrator,
            id: &str,
            i: usize,
        ) -> ServiceProcessMetrics {
            let name = format!("{}-{id}-{i}", self_.namespace);
            let metrics = match self_.metrics_api.get(&name).await {
                Ok(metrics) => metrics,
                Err(e) => {
                    warn!("Failed to get metrics for {name}: {e}");
                    return ServiceProcessMetrics::default();
                }
            };
            let Some(PodMetricsContainer { usage: PodMetricsContainerUsage { cpu: Quantity(cpu_str), memory: Quantity(mem_str) }, .. }) = metrics.containers.get(0) else {
                warn!("metrics result contained no containers for {name}");
                return ServiceProcessMetrics::default();
            };

            let cpu = match parse_k8s_quantity(cpu_str) {
                Ok(q) => match q.try_to_integer(-9, true) {
                    Some(i) => Some(i),
                    None => {
                        tracing::error!("CPU value {q:? }out of range");
                        None
                    }
                },
                Err(e) => {
                    tracing::error!("Failed to parse CPU value {cpu_str}: {e}");
                    None
                }
            };
            let memory = match parse_k8s_quantity(mem_str) {
                Ok(q) => match q.try_to_integer(3, false) {
                    Some(i) => Some(i),
                    None => {
                        tracing::error!("Memory value {q:?} out of range");
                        None
                    }
                },
                Err(e) => {
                    tracing::error!("Failed to parse memory value {mem_str}: {e}");
                    None
                }
            };

            ServiceProcessMetrics {
                nano_cpus: cpu,
                bytes_memory: memory,
            }
        }
        let ret = futures::future::join_all((0..scale.get()).map(|i| get_metrics(self, id, i)));

        Ok(ret.await)
    }

    async fn ensure_service(
        &self,
        id: &str,
        ServiceConfig {
            image,
            init_container_image,
            args,
            ports: ports_in,
            memory_limit,
            cpu_limit,
            scale,
            labels: labels_in,
            availability_zone,
            anti_affinity,
        }: ServiceConfig<'_>,
    ) -> Result<Box<dyn Service>, anyhow::Error> {
        let name = format!("{}-{id}", self.namespace);
        // The match labels should be the minimal set of labels that uniquely
        // identify the pods in the stateful set. Changing these after the
        // `StatefulSet` is created is not permitted by Kubernetes, and we're
        // not yet smart enough to handle deleting and recreating the
        // `StatefulSet`.
        let match_labels = btreemap! {
            "environmentd.materialize.cloud/namespace".into() => self.namespace.clone(),
            "environmentd.materialize.cloud/service-id".into() => id.into(),
        };
        let mut labels = match_labels.clone();
        for (key, value) in labels_in {
            labels.insert(self.make_label_key(&key), value);
        }
        for port in &ports_in {
            labels.insert(
                format!("environmentd.materialize.cloud/port-{}", port.name),
                "true".into(),
            );
        }
        for (key, value) in &self.config.service_labels {
            labels.insert(key.clone(), value.clone());
        }
        let mut limits = BTreeMap::new();
        if let Some(memory_limit) = memory_limit {
            limits.insert(
                "memory".into(),
                Quantity(memory_limit.0.as_u64().to_string()),
            );
        }
        if let Some(cpu_limit) = cpu_limit {
            limits.insert(
                "cpu".into(),
                Quantity(format!("{}m", cpu_limit.as_millicpus())),
            );
        }
        let service = K8sService {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                ports: Some(
                    ports_in
                        .iter()
                        .map(|port| ServicePort {
                            port: port.port_hint.into(),
                            name: Some(port.name.clone()),
                            ..Default::default()
                        })
                        .collect(),
                ),
                cluster_ip: None,
                selector: Some(match_labels.clone()),
                ..Default::default()
            }),
            status: None,
        };

        let hosts = (0..scale.get())
            .map(|i| {
                format!(
                    "{name}-{i}.{name}.{}.svc.cluster.local",
                    self.kubernetes_namespace
                )
            })
            .collect::<Vec<_>>();
        let ports = ports_in
            .iter()
            .map(|p| (p.name.clone(), p.port_hint))
            .collect::<HashMap<_, _>>();
        let peers = hosts
            .iter()
            .map(|host| (host.clone(), ports.clone()))
            .collect::<Vec<_>>();

        let mut node_selector: BTreeMap<String, String> = self
            .config
            .service_node_selector
            .clone()
            .into_iter()
            .collect();
        if let Some(availability_zone) = availability_zone {
            node_selector.insert(
                "materialize.cloud/availability-zone".to_string(),
                availability_zone,
            );
        }
        let mut args = args(&ServiceAssignments {
            listen_host: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            ports: &ports,
            index: None,
            peers: &peers,
        });
        args.push("--secrets-reader=kubernetes".into());
        args.push(format!(
            "--secrets-reader-kubernetes-context={}",
            self.config.context
        ));

        let anti_affinity = anti_affinity
            .map(|label_selectors| -> Result<_, anyhow::Error> {
                let label_selector_requirements = label_selectors
                    .into_iter()
                    .map(|ls| self.label_selector_to_k8s(ls))
                    .collect::<Result<Vec<_>, _>>()?;
                let ls = LabelSelector {
                    match_expressions: Some(label_selector_requirements),
                    ..Default::default()
                };
                let pat = PodAffinityTerm {
                    label_selector: Some(ls),
                    topology_key: "kubernetes.io/hostname".to_string(),
                    ..Default::default()
                };
                Ok(PodAntiAffinity {
                    required_during_scheduling_ignored_during_execution: Some(vec![pat]),
                    ..Default::default()
                })
            })
            .transpose()?;
        let pod_annotations = btreemap! {
            // Prevent the cluster-autoscaler from evicting these pods in attempts to scale down
            // and terminate nodes.
            // This will cost us more money, but should give us better uptime.
            // This does not prevent all evictions by Kubernetes, only the ones initiated by the
            // cluster-autoscaler. Notably, eviction of pods for resource overuse is still enabled.
            "cluster-autoscaler.kubernetes.io/safe-to-evict".to_owned() => "false".to_string(),
        };

        let container_name = image
            .splitn(2, '/')
            .skip(1)
            .next()
            .and_then(|name_version| name_version.splitn(2, ':').next())
            .context("`image` is not ORG/NAME:VERSION")?
            .to_string();

        let init_containers = init_container_image.map(|image| {
            vec![Container {
                name: "k8s-init-container".to_string(),
                image: Some(image),
                image_pull_policy: Some(self.config.image_pull_policy.to_string()),
                env: Some(vec![
                    EnvVar {
                        name: "MZ_NAMESPACE".to_string(),
                        value_from: Some(EnvVarSource {
                            field_ref: Some(ObjectFieldSelector {
                                field_path: "metadata.namespace".to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    EnvVar {
                        name: "MZ_POD_NAME".to_string(),
                        value_from: Some(EnvVarSource {
                            field_ref: Some(ObjectFieldSelector {
                                field_path: "metadata.name".to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    EnvVar {
                        name: "MZ_NODE_NAME".to_string(),
                        value_from: Some(EnvVarSource {
                            field_ref: Some(ObjectFieldSelector {
                                field_path: "spec.nodeName".to_string(),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }]
        });

        let mut pod_template_spec = PodTemplateSpec {
            metadata: Some(ObjectMeta {
                labels: Some(labels.clone()),
                annotations: Some(pod_annotations), // Do not delete, we insert into it below.
                ..Default::default()
            }),
            spec: Some(PodSpec {
                init_containers,
                containers: vec![Container {
                    name: container_name,
                    image: Some(image),
                    args: Some(args),
                    image_pull_policy: Some(self.config.image_pull_policy.to_string()),
                    ports: Some(
                        ports_in
                            .iter()
                            .map(|port| ContainerPort {
                                container_port: port.port_hint.into(),
                                name: Some(port.name.clone()),
                                ..Default::default()
                            })
                            .collect(),
                    ),
                    resources: Some(ResourceRequirements {
                        limits: Some(limits),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                node_selector: Some(node_selector),
                service_account: self.config.service_account.clone(),
                affinity: Some(Affinity {
                    pod_anti_affinity: anti_affinity,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        };
        let pod_template_json = serde_json::to_string(&pod_template_spec).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(pod_template_json);
        let pod_template_hash = format!("{:x}", hasher.finalize());
        let pod_template_hash_annotation = "environmentd.materialize.cloud/pod-template-hash";
        pod_template_spec
            .metadata
            .as_mut()
            .unwrap()
            .annotations
            .as_mut()
            .unwrap()
            .insert(
                pod_template_hash_annotation.to_owned(),
                pod_template_hash.clone(),
            );

        let stateful_set = StatefulSet {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                ..Default::default()
            },
            spec: Some(StatefulSetSpec {
                selector: LabelSelector {
                    match_labels: Some(match_labels),
                    ..Default::default()
                },
                service_name: name.clone(),
                replicas: Some(scale.get().try_into()?),
                template: pod_template_spec,
                pod_management_policy: Some("Parallel".to_string()),
                ..Default::default()
            }),
            status: None,
        };
        self.service_api
            .patch(
                &name,
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(service),
            )
            .await?;
        self.stateful_set_api
            .patch(
                &name,
                &PatchParams::apply(FIELD_MANAGER).force(),
                &Patch::Apply(stateful_set),
            )
            .await?;
        // Explicitly delete any pods in the stateful set that don't match the
        // template. In theory, Kubernetes would do this automatically, but
        // in practice we have observed that it does not.
        // See: https://github.com/kubernetes/kubernetes/issues/67250
        for pod_id in 0..scale.get() {
            let pod_name = format!("{}-{}", &name, pod_id);
            let pod = match self.pod_api.get(&pod_name).await {
                Ok(pod) => pod,
                // Pod already doesn't exist.
                Err(kube::Error::Api(e)) if e.code == 404 => continue,
                Err(e) => return Err(e.into()),
            };
            if pod.annotations().get(pod_template_hash_annotation) != Some(&pod_template_hash) {
                match self
                    .pod_api
                    .delete(&pod_name, &DeleteParams::default())
                    .await
                {
                    Ok(_) => (),
                    // Pod got deleted while we were looking at it.
                    Err(kube::Error::Api(e)) if e.code == 404 => (),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        self.service_scales
            .lock()
            .expect("poisoned lock")
            .insert(id.to_string(), scale);
        Ok(Box::new(KubernetesService { hosts, ports }))
    }

    /// Drops the identified service, if it exists.
    async fn drop_service(&self, id: &str) -> Result<(), anyhow::Error> {
        fail::fail_point!("kubernetes_drop_service", |_| Err(anyhow!("failpoint")));
        self.service_scales
            .lock()
            .expect("poisoned lock")
            .remove(id);
        let name = format!("{}-{id}", self.namespace);
        let res = self
            .stateful_set_api
            .delete(&name, &DeleteParams::default())
            .await;
        match res {
            Ok(_) => (),
            Err(Error::Api(e)) if e.code == 404 => (),
            Err(e) => return Err(e.into()),
        }

        let res = self
            .service_api
            .delete(&name, &DeleteParams::default())
            .await;
        match res {
            Ok(_) => Ok(()),
            Err(Error::Api(e)) if e.code == 404 => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// Lists the identifiers of all known services.
    async fn list_services(&self) -> Result<Vec<String>, anyhow::Error> {
        let stateful_sets = self.stateful_set_api.list(&Default::default()).await?;
        let name_prefix = format!("{}-", self.namespace);
        Ok(stateful_sets
            .into_iter()
            .filter_map(|ss| {
                ss.metadata
                    .name
                    .unwrap()
                    .strip_prefix(&name_prefix)
                    .map(Into::into)
            })
            .collect())
    }

    fn watch_services(&self) -> BoxStream<'static, Result<ServiceEvent, anyhow::Error>> {
        fn into_service_event(pod: Pod) -> Result<ServiceEvent, anyhow::Error> {
            let process_id = pod.name_any().split('-').last().unwrap().parse()?;
            let service_id_label = "environmentd.materialize.cloud/service-id";
            let service_id = pod
                .labels()
                .get(service_id_label)
                .ok_or_else(|| anyhow!("missing label: {service_id_label}"))?
                .clone();

            let (pod_ready, last_probe_time) = pod
                .status
                .and_then(|status| status.conditions)
                .and_then(|conditions| conditions.into_iter().find(|c| c.type_ == "Ready"))
                .map(|c| (c.status == "True", c.last_probe_time))
                .unwrap_or((false, None));

            let status = if pod_ready {
                ServiceStatus::Ready
            } else {
                ServiceStatus::NotReady
            };
            let time = if let Some(time) = last_probe_time {
                time.0
            } else {
                Utc::now()
            };

            Ok(ServiceEvent {
                service_id,
                process_id,
                status,
                time,
            })
        }

        let stream = watcher(self.pod_api.clone(), self.list_pod_params())
            .touched_objects()
            .filter_map(|object| async move {
                match object {
                    Ok(pod) => Some(into_service_event(pod)),
                    Err(error) => {
                        // We assume that errors returned by Kubernetes are usually transient, so we
                        // just log a warning and ignore them otherwise.
                        tracing::warn!("service watch error: {error}");
                        None
                    }
                }
            });
        Box::pin(stream)
    }
}

#[derive(Debug, Clone)]
struct KubernetesService {
    hosts: Vec<String>,
    ports: HashMap<String, u16>,
}

impl Service for KubernetesService {
    fn addresses(&self, port: &str) -> Vec<String> {
        let port = self.ports[port];
        self.hosts
            .iter()
            .map(|host| format!("{host}:{port}"))
            .collect()
    }
}
