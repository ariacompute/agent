//! Docker orchestration for `AGENT_EGRESS_ENFORCEMENT=network` (ADR-0024).
//!
//! Topology: one `internal` bridge network, one proxy container attached to it
//! **and** to the control plane's network. Sandboxes are created with
//! `network_mode = <internal network>`, so they have no default route: the proxy
//! is the only reachable peer.
//!
//! Everything here is idempotent and fails loudly. A half-configured network is
//! worse than no network, because it looks enforced.

use crate::DockerSandbox;
use crate::SandboxError;
use bollard::container::{
    Config, CreateContainerOptions, ListContainersOptions, StartContainerOptions,
};
use bollard::network::{ConnectNetworkOptions, CreateNetworkOptions};
use bollard::service::{
    ContainerInspectResponse, EndpointSettings, HealthStatusEnum, HostConfig, PortBinding,
    RestartPolicy, RestartPolicyNameEnum,
};
use std::collections::HashMap;
use std::time::Duration;

/// Default name of the internal network.
pub const DEFAULT_NETWORK: &str = "aria-egress";
/// Default container name of the proxy.
pub const DEFAULT_PROXY_CONTAINER: &str = "aria-egress-proxy";
/// Label the control plane writes at spawn time: the tenant that owns a sandbox.
pub const TENANT_LABEL: &str = "aria.tenant_id";
/// Label carrying the session id (optional).
pub const SESSION_LABEL: &str = "aria.session_id";
/// Port the proxy listens on inside its container.
pub const PROXY_PORT: u16 = 8080;

/// Environment that points a process at the egress proxy.
///
/// This is the single source of the variable set: the control plane, the core
/// runtime's own wrapping and the shell seam all use it, so a sandbox cannot end
/// up with a half-configured proxy environment.
pub fn proxy_env(proxy_url: &str) -> Vec<(String, String)> {
    vec![
        ("HTTP_PROXY".to_string(), proxy_url.to_string()),
        ("HTTPS_PROXY".to_string(), proxy_url.to_string()),
        ("ALL_PROXY".to_string(), proxy_url.to_string()),
        // Loopback must stay direct or the proxy would talk to itself.
        (
            "NO_PROXY".to_string(),
            "localhost,127.0.0.1,::1".to_string(),
        ),
    ]
}

/// What the orchestration needs to know. Read from the environment by
/// [`EgressConfig::from_env`], constructed by hand in tests.
#[derive(Debug, Clone)]
pub struct EgressConfig {
    pub network: String,
    pub proxy_container: String,
    pub proxy_image: String,
    pub control_plane_url: String,
    pub token: String,
    /// Networks of the control plane container (discovered from the hostname).
    pub control_networks: Vec<String>,
}

impl EgressConfig {
    pub fn from_env() -> Result<Self, SandboxError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Same as [`Self::from_env`], with the values injected.
    ///
    /// The control plane publishes `AGENT_EGRESS_*` before reading them back and
    /// the tests inject a fixed map, so the parsing rules live in one place
    /// instead of being re-implemented against the process environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, SandboxError> {
        let required = |key: &str| -> Result<String, SandboxError> {
            get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    SandboxError::NotConfigured(format!(
                        "{key} must be set for AGENT_EGRESS_ENFORCEMENT=network"
                    ))
                })
        };
        let optional = |key: &str, default: &str| -> String {
            get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Ok(Self {
            network: optional("AGENT_EGRESS_NETWORK", DEFAULT_NETWORK),
            proxy_container: optional("EGRESS_PROXY_CONTAINER", DEFAULT_PROXY_CONTAINER),
            proxy_image: required("EGRESS_PROXY_IMAGE")?,
            control_plane_url: required("CONTROL_PLANE_URL")?,
            token: required("EGRESS_PROXY_TOKEN")?,
            control_networks: Vec::new(),
        })
    }

    /// Address a sandbox must use to reach the proxy (container DNS).
    pub fn proxy_url_for_sandbox(&self) -> String {
        format!("http://{}:{PROXY_PORT}", self.proxy_container)
    }

    /// Veto configurations that would silently weaken enforcement.
    pub fn validate(&self) -> Result<(), SandboxError> {
        if self.network.trim().is_empty() || self.proxy_container.trim().is_empty() {
            return Err(SandboxError::NotConfigured(
                "egress network and proxy container names must not be empty".into(),
            ));
        }
        if self.control_networks.is_empty() {
            return Err(SandboxError::NotConfigured(
                "network enforcement needs the control plane's Docker network; \
                 run the cloud service inside a container (compose) or use enforcement=proxy"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// One step of the idempotent bring-up, separated out so the decision logic is
/// testable without a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    CreateNetwork,
    CreateProxy,
    StartProxy,
    ConnectProxyToControl,
    ConnectProxyToEgress,
}

/// Decide what still has to happen. Pure: the caller executes the steps.
pub fn plan(
    network_present: bool,
    proxy_present: bool,
    proxy_running: bool,
    proxy_on_egress: bool,
    proxy_on_control: bool,
) -> Vec<Step> {
    let mut steps = Vec::new();
    if !network_present {
        steps.push(Step::CreateNetwork);
    }
    if !proxy_present {
        // `CreateProxy` attaches the new container to the control networks and
        // to the egress network in one go. Asking for the connections again
        // makes Docker answer `403 endpoint ... already exists in network`, so a
        // fresh bring-up would fail on its own second `connect`.
        steps.push(Step::CreateProxy);
    } else {
        if !proxy_on_control {
            steps.push(Step::ConnectProxyToControl);
        }
        if !proxy_on_egress {
            steps.push(Step::ConnectProxyToEgress);
        }
    }
    if !proxy_running {
        steps.push(Step::StartProxy);
    }
    steps
}

/// A resolved sandbox identity: tenant + optional session, from container labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerIdentity {
    pub principal_id: String,
    pub session_id: Option<String>,
    pub sandbox_id: String,
}

impl PeerIdentity {
    /// Labels are the only trusted source. A container without a tenant label is
    /// not attributable, so it is not resolved (and the proxy will refuse it).
    pub fn from_inspect(name: &str, container: &ContainerInspectResponse) -> Option<Self> {
        let labels = container.config.as_ref()?.labels.as_ref()?;
        let principal_id = labels.get(TENANT_LABEL)?.trim().to_string();
        if principal_id.is_empty() {
            return None;
        }
        Some(Self {
            principal_id,
            session_id: labels
                .get(SESSION_LABEL)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty()),
            sandbox_id: name.to_string(),
        })
    }

    /// IP of this container on `network`.
    pub fn ip_on(container: &ContainerInspectResponse, network: &str) -> Option<String> {
        let networks = container.network_settings.as_ref()?.networks.as_ref()?;
        let endpoint: &EndpointSettings = networks.get(network)?;
        let ip = endpoint.ip_address.as_deref()?.trim();
        if ip.is_empty() {
            None
        } else {
            Some(ip.to_string())
        }
    }
}

/// The brought-up network. Holds the resolved network name for spawn callers.
#[derive(Debug, Clone)]
pub struct EgressNetwork {
    network: String,
    proxy_container: String,
}

impl EgressNetwork {
    pub fn network(&self) -> &str {
        &self.network
    }

    pub fn proxy_container(&self) -> &str {
        &self.proxy_container
    }

    /// Idempotent bring-up. Any failure is fatal for the caller (fail boot).
    pub async fn ensure(sandbox: &DockerSandbox, cfg: &EgressConfig) -> Result<Self, SandboxError> {
        cfg.validate()?;
        let docker = sandbox.client().await?;

        let networks = docker
            .list_networks(None::<bollard::network::ListNetworksOptions<String>>)
            .await
            .map_err(|e| SandboxError::NotConfigured(format!("list docker networks: {e}")))?;
        let network_present = networks
            .iter()
            .any(|n| n.name.as_deref() == Some(cfg.network.as_str()));

        let proxy = docker
            .inspect_container(cfg.proxy_container.as_str(), None)
            .await
            .ok();
        let proxy_present = proxy.is_some();
        let proxy_running = proxy
            .as_ref()
            .and_then(|c| c.state.as_ref())
            .and_then(|s| s.running)
            .unwrap_or(false);
        let proxy_on_egress = proxy
            .as_ref()
            .map(|c| PeerIdentity::ip_on(c, &cfg.network).is_some())
            .unwrap_or(false);
        let proxy_on_control = proxy
            .as_ref()
            .map(|c| {
                cfg.control_networks
                    .iter()
                    .any(|net| PeerIdentity::ip_on(c, net).is_some())
            })
            .unwrap_or(false);

        for step in plan(
            network_present,
            proxy_present,
            proxy_running,
            proxy_on_egress,
            proxy_on_control,
        ) {
            match step {
                Step::CreateNetwork => {
                    docker
                        .create_network(CreateNetworkOptions {
                            name: cfg.network.as_str(),
                            driver: "bridge",
                            // The whole point: no route out of this network.
                            internal: true,
                            ..Default::default()
                        })
                        .await
                        .map_err(|e| {
                            SandboxError::NotConfigured(format!(
                                "create network {}: {e}",
                                cfg.network
                            ))
                        })?;
                }
                Step::CreateProxy => {
                    let env = vec![
                        "RUST_LOG=aria_egress_proxy=info".to_string(),
                        format!("EGRESS_PROXY_TOKEN={}", cfg.token),
                        format!("CONTROL_PLANE_URL={}", cfg.control_plane_url),
                        format!("EGRESS_PROXY_BIND=0.0.0.0:{PROXY_PORT}"),
                    ];
                    let labels: HashMap<String, String> =
                        HashMap::from([("aria.role".to_string(), "egress-proxy".to_string())]);
                    docker
                        .create_container(
                            Some(CreateContainerOptions {
                                name: cfg.proxy_container.as_str(),
                                platform: None,
                            }),
                            Config {
                                image: Some(cfg.proxy_image.clone()),
                                env: Some(env),
                                labels: Some(labels),
                                host_config: Some(HostConfig {
                                    restart_policy: Some(RestartPolicy {
                                        name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                                        maximum_retry_count: None,
                                    }),
                                    // Never publish the proxy port: it must only be
                                    // reachable from the sandbox network.
                                    port_bindings: None::<HashMap<String, Option<Vec<PortBinding>>>>,
                                    ..Default::default()
                                }),
                                ..Default::default()
                            },
                        )
                        .await
                        .map_err(|e| {
                            SandboxError::NotConfigured(format!(
                                "create proxy container {}: {e}",
                                cfg.proxy_container
                            ))
                        })?;
                    for net in cfg
                        .control_networks
                        .iter()
                        .chain(std::iter::once(&cfg.network))
                    {
                        docker
                            .connect_network(
                                net.as_str(),
                                ConnectNetworkOptions {
                                    container: cfg.proxy_container.as_str(),
                                    endpoint_config: Default::default(),
                                },
                            )
                            .await
                            .map_err(|e| {
                                SandboxError::NotConfigured(format!(
                                    "attach proxy to network {net}: {e}"
                                ))
                            })?;
                    }
                }
                Step::StartProxy => {
                    docker
                        .start_container(
                            cfg.proxy_container.as_str(),
                            None::<StartContainerOptions<String>>,
                        )
                        .await
                        .map_err(|e| {
                            SandboxError::NotConfigured(format!(
                                "start proxy container {}: {e}",
                                cfg.proxy_container
                            ))
                        })?;
                    // Give the listener a moment, then require it to be up.
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                Step::ConnectProxyToControl => {
                    for net in &cfg.control_networks {
                        docker
                            .connect_network(
                                net.as_str(),
                                ConnectNetworkOptions {
                                    container: cfg.proxy_container.as_str(),
                                    endpoint_config: Default::default(),
                                },
                            )
                            .await
                            .map_err(|e| {
                                SandboxError::NotConfigured(format!(
                                    "attach proxy to control network {net}: {e}"
                                ))
                            })?;
                    }
                }
                Step::ConnectProxyToEgress => {
                    docker
                        .connect_network(
                            cfg.network.as_str(),
                            ConnectNetworkOptions {
                                container: cfg.proxy_container.as_str(),
                                endpoint_config: Default::default(),
                            },
                        )
                        .await
                        .map_err(|e| {
                            SandboxError::NotConfigured(format!(
                                "attach proxy to {}: {e}",
                                cfg.network
                            ))
                        })?;
                }
            }
        }

        let network = Self {
            network: cfg.network.clone(),
            proxy_container: cfg.proxy_container.clone(),
        };
        network.probe(sandbox).await?;
        Ok(network)
    }

    /// Refuse to continue unless the proxy is actually running.
    pub async fn probe(&self, sandbox: &DockerSandbox) -> Result<(), SandboxError> {
        let docker = sandbox.client().await?;
        let container = docker
            .inspect_container(self.proxy_container.as_str(), None)
            .await
            .map_err(|e| {
                SandboxError::NotConfigured(format!(
                    "egress proxy container {} is missing: {e}",
                    self.proxy_container
                ))
            })?;
        if !container
            .state
            .as_ref()
            .and_then(|s| s.running)
            .unwrap_or(false)
        {
            return Err(SandboxError::NotConfigured(format!(
                "egress proxy {} is not running; check `docker logs {}`",
                self.proxy_container, self.proxy_container
            )));
        }
        if PeerIdentity::ip_on(&container, &self.network).is_none() {
            return Err(SandboxError::NotConfigured(format!(
                "egress proxy {} is not attached to {}",
                self.proxy_container, self.network
            )));
        }
        Ok(())
    }

    /// Container health, for `/v1/governance/status`.
    pub async fn health(&self, sandbox: &DockerSandbox) -> Option<HealthStatusEnum> {
        let docker = sandbox.client().await.ok()?;
        let container = docker
            .inspect_container(self.proxy_container.as_str(), None)
            .await
            .ok()?;
        container.state?.health?.status
    }

    /// Resolve a source IP to a tenant by inspecting the containers on the
    /// egress network. `None` means "not attributable" → the proxy must refuse.
    pub async fn resolve_peer(
        sandbox: &DockerSandbox,
        network: &str,
        ip: &str,
    ) -> Result<Option<PeerIdentity>, SandboxError> {
        let docker = sandbox.client().await?;
        let mut filters: HashMap<String, Vec<String>> = HashMap::new();
        filters.insert("network".into(), vec![network.to_string()]);
        let containers = docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters,
                ..Default::default()
            }))
            .await
            .map_err(|e| SandboxError::NotConfigured(format!("list sandbox containers: {e}")))?;
        for summary in containers {
            let Some(name) = summary.names.as_ref().and_then(|n| n.first()) else {
                continue;
            };
            let name = name.trim_start_matches('/');
            let Ok(inspect) = docker.inspect_container(name, None).await else {
                continue;
            };
            if PeerIdentity::ip_on(&inspect, network).as_deref() != Some(ip) {
                continue;
            }
            // The proxy container is also on this network; it has no tenant label.
            if let Some(identity) = PeerIdentity::from_inspect(name, &inspect) {
                return Ok(Some(identity));
            }
        }
        Ok(None)
    }

    /// The container's own networks, discovered from its hostname. Used to place
    /// the proxy where it can reach the control plane.
    pub async fn control_networks(sandbox: &DockerSandbox) -> Result<Vec<String>, SandboxError> {
        let docker = sandbox.client().await?;
        let hostname = std::env::var("HOSTNAME").unwrap_or_default();
        if hostname.trim().is_empty() {
            return Err(SandboxError::NotConfigured(
                "HOSTNAME is empty: network enforcement must run inside a container".into(),
            ));
        }
        let container = docker
            .inspect_container(hostname.as_str(), None)
            .await
            .map_err(|e| {
                SandboxError::NotConfigured(format!(
                    "cannot discover the control plane network from hostname `{hostname}`: {e}; \
                 run the cloud service in a container (compose) or use enforcement=proxy"
                ))
            })?;
        let networks = container
            .network_settings
            .and_then(|s| s.networks)
            .map(|m| m.into_keys().collect::<Vec<_>>())
            .unwrap_or_default();
        if networks.is_empty() {
            return Err(SandboxError::NotConfigured(
                "the control plane container has no Docker network".into(),
            ));
        }
        Ok(networks)
    }
}

/// Wraps **any** backend and puts every session on the governed network.
///
/// The shell seam already sets `network`/`labels`, but the agent loop and the
/// terminal family call the sandbox directly. Wrapping the handle the agent
/// receives closes those paths structurally: a new caller cannot forget.
#[derive(Clone)]
pub struct NetworkingSandbox {
    inner: std::sync::Arc<dyn crate::Sandbox>,
    network: String,
    labels: Vec<(String, String)>,
    env: Vec<(String, String)>,
}

impl std::fmt::Debug for NetworkingSandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetworkingSandbox")
            .field("network", &self.network)
            .field("labels", &self.labels.len())
            .finish_non_exhaustive()
    }
}

impl NetworkingSandbox {
    pub fn new(
        inner: std::sync::Arc<dyn crate::Sandbox>,
        network: impl Into<String>,
        labels: Vec<(String, String)>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            inner,
            network: network.into(),
            labels,
            env,
        }
    }

    /// Fill in what the caller did not set; never overwrite an explicit value.
    fn attach(&self, spec: &crate::ExecSpec) -> crate::ExecSpec {
        let mut out = spec.clone();
        if out.network.is_none() {
            out.network = Some(self.network.clone());
        }
        if out.labels.is_empty() {
            out.labels = self.labels.clone();
        }
        if out.env.is_empty() {
            out.env = self.env.clone();
        }
        out
    }
}

#[async_trait::async_trait]
impl crate::Sandbox for NetworkingSandbox {
    async fn spawn(&self, spec: &crate::ExecSpec) -> Result<crate::SandboxHandle, SandboxError> {
        self.inner.spawn(&self.attach(spec)).await
    }

    async fn exec(
        &self,
        handle: &crate::SandboxHandle,
        cmd: &[String],
    ) -> Result<crate::ExecOutput, SandboxError> {
        self.inner.exec(handle, cmd).await
    }

    async fn destroy(&self, handle: crate::SandboxHandle) -> Result<(), SandboxError> {
        self.inner.destroy(handle).await
    }

    async fn exec_attached(
        &self,
        handle: &crate::SandboxHandle,
        spec: &crate::AttachedSpec,
    ) -> Result<crate::AttachedExec, SandboxError> {
        // The process is created in a container this decorator already put on the
        // governed network, so there is nothing to attach here — but it must be
        // forwarded, or the decorator would report the capability as missing.
        self.inner.exec_attached(handle, spec).await
    }

    async fn copy_in(
        &self,
        handle: &crate::SandboxHandle,
        target: &str,
        tar: Vec<u8>,
    ) -> Result<(), SandboxError> {
        self.inner.copy_in(handle, target, tar).await
    }

    async fn has_image(&self, image: &str) -> Result<bool, SandboxError> {
        self.inner.has_image(image).await
    }

    fn capabilities(&self) -> crate::SandboxCapabilities {
        self.inner.capabilities()
    }

    async fn status(
        &self,
        handle: &crate::SandboxHandle,
    ) -> Result<crate::SandboxLifecycle, SandboxError> {
        self.inner.status(handle).await
    }

    async fn pause(&self, handle: &crate::SandboxHandle) -> Result<(), SandboxError> {
        self.inner.pause(handle).await
    }

    async fn resume(&self, handle: &crate::SandboxHandle) -> Result<(), SandboxError> {
        self.inner.resume(handle).await
    }

    async fn suspend(&self, handle: &crate::SandboxHandle) -> Result<(), SandboxError> {
        self.inner.suspend(handle).await
    }

    async fn checkpoint(
        &self,
        handle: &crate::SandboxHandle,
    ) -> Result<crate::CheckpointRef, SandboxError> {
        self.inner.checkpoint(handle).await
    }

    async fn restore(
        &self,
        checkpoint: &crate::CheckpointRef,
        spec: &crate::ExecSpec,
    ) -> Result<crate::SandboxHandle, SandboxError> {
        self.inner.restore(checkpoint, &self.attach(spec)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Sandbox as _;
    use bollard::service::{ContainerConfig, ContainerState, NetworkSettings};

    fn inspect(tenant: Option<&str>, ip: Option<&str>) -> ContainerInspectResponse {
        let mut labels = HashMap::new();
        if let Some(tenant) = tenant {
            labels.insert(TENANT_LABEL.to_string(), tenant.to_string());
        }
        let mut networks = HashMap::new();
        if let Some(ip) = ip {
            networks.insert(
                DEFAULT_NETWORK.to_string(),
                EndpointSettings {
                    ip_address: Some(ip.to_string()),
                    ..Default::default()
                },
            );
        }
        ContainerInspectResponse {
            config: Some(ContainerConfig {
                labels: Some(labels),
                ..Default::default()
            }),
            state: Some(ContainerState {
                running: Some(true),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn identity_comes_only_from_labels() {
        let labelled = inspect(Some("tenant-a"), Some("10.0.0.5"));
        let identity = PeerIdentity::from_inspect("sbx-1", &labelled).unwrap();
        assert_eq!(identity.principal_id, "tenant-a");
        assert_eq!(identity.sandbox_id, "sbx-1");
        assert!(PeerIdentity::from_inspect("proxy", &inspect(None, Some("10.0.0.2"))).is_none());
        assert!(PeerIdentity::from_inspect("blank", &inspect(Some("   "), None)).is_none());
    }

    #[test]
    fn ip_is_read_from_the_named_network() {
        let container = inspect(Some("t"), Some("10.0.0.5"));
        assert_eq!(
            PeerIdentity::ip_on(&container, DEFAULT_NETWORK).as_deref(),
            Some("10.0.0.5")
        );
        assert!(PeerIdentity::ip_on(&container, "other").is_none());
    }

    #[test]
    fn bring_up_is_idempotent_and_complete() {
        assert!(plan(false, false, false, false, false).contains(&Step::CreateNetwork));
        assert!(plan(false, false, false, false, false).contains(&Step::CreateProxy));
        // Creating the proxy attaches it; asking for the connections again makes
        // Docker refuse the second `connect` with `403 ... already exists`.
        assert!(!plan(false, false, false, false, false).contains(&Step::ConnectProxyToControl));
        assert!(!plan(false, false, false, false, false).contains(&Step::ConnectProxyToEgress));
        assert_eq!(
            plan(true, true, true, true, true),
            Vec::new(),
            "nothing to do when everything is up"
        );
        assert_eq!(plan(true, true, false, true, true), vec![Step::StartProxy]);
        assert_eq!(
            plan(true, true, true, false, true),
            vec![Step::ConnectProxyToEgress]
        );
        assert_eq!(
            plan(true, true, true, true, false),
            vec![Step::ConnectProxyToControl]
        );
    }

    #[derive(Default)]
    struct Capture {
        last: std::sync::Mutex<Option<crate::ExecSpec>>,
        last_target: std::sync::Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl crate::Sandbox for Capture {
        async fn spawn(
            &self,
            spec: &crate::ExecSpec,
        ) -> Result<crate::SandboxHandle, SandboxError> {
            *self.last.lock().unwrap() = Some(spec.clone());
            Ok(crate::SandboxHandle { id: "sbx".into() })
        }
        async fn exec(
            &self,
            _handle: &crate::SandboxHandle,
            _cmd: &[String],
        ) -> Result<crate::ExecOutput, SandboxError> {
            Err(SandboxError::Exec("not used".into()))
        }
        async fn destroy(&self, _handle: crate::SandboxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
        async fn exec_attached(
            &self,
            _handle: &crate::SandboxHandle,
            spec: &crate::AttachedSpec,
        ) -> Result<crate::AttachedExec, SandboxError> {
            Ok(crate::AttachedExec {
                exec_id: spec.command.join(" "),
                stdin: Box::pin(tokio::io::sink()),
                output: Box::pin(futures::stream::empty()),
            })
        }
        async fn copy_in(
            &self,
            _handle: &crate::SandboxHandle,
            target: &str,
            _tar: Vec<u8>,
        ) -> Result<(), SandboxError> {
            *self.last_target.lock().unwrap() = Some(target.to_string());
            Ok(())
        }
        async fn has_image(&self, _image: &str) -> Result<bool, SandboxError> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn the_decorator_forwards_attach_copy_and_image_probes() {
        // A network wrapper that forgot to forward would turn "attach" into
        // `Unsupported` and break the sandboxed harness.
        let capture = std::sync::Arc::new(Capture::default());
        let sandbox = NetworkingSandbox::new(
            capture.clone(),
            "aria-egress",
            vec![(TENANT_LABEL.into(), "tenant-a".into())],
            Vec::new(),
        );
        let handle = crate::SandboxHandle { id: "sbx".into() };
        assert!(sandbox.has_image("aria-dsh-sandbox:latest").await.unwrap());
        sandbox
            .copy_in(&handle, "/aria/session/.aria", Vec::new())
            .await
            .unwrap();
        assert_eq!(
            capture.last_target.lock().unwrap().as_deref(),
            Some("/aria/session/.aria")
        );
        let attached = sandbox
            .exec_attached(&handle, &crate::AttachedSpec::command(vec!["dsh".into()]))
            .await
            .unwrap();
        assert_eq!(attached.exec_id, "dsh");
    }

    #[tokio::test]
    async fn the_decorator_fills_only_what_the_caller_left_empty() {
        let capture = std::sync::Arc::new(Capture::default());
        let sandbox = NetworkingSandbox::new(
            capture.clone(),
            "aria-egress",
            vec![(TENANT_LABEL.into(), "tenant-a".into())],
            vec![("HTTP_PROXY".into(), "http://proxy:8080".into())],
        );
        let bare = crate::ExecSpec::command(vec!["true".into()]);
        sandbox.spawn(&bare).await.unwrap();
        let seen = capture.last.lock().unwrap().clone().unwrap();
        assert_eq!(seen.network.as_deref(), Some("aria-egress"));
        assert_eq!(seen.labels.len(), 1);
        assert_eq!(seen.env.len(), 1);

        let mut explicit = crate::ExecSpec::command(vec!["true".into()]);
        explicit.network = Some("other".into());
        explicit.env = vec![("A".into(), "1".into())];
        sandbox.spawn(&explicit).await.unwrap();
        let seen = capture.last.lock().unwrap().clone().unwrap();
        assert_eq!(seen.network.as_deref(), Some("other"), "explicit wins");
        assert_eq!(seen.env, vec![("A".to_string(), "1".to_string())]);
        assert_eq!(seen.labels.len(), 1, "labels are filled in");

        // An identity the caller already stamped is not replaced either: the
        // proxy resolves the tenant from these labels.
        let mut labeled = crate::ExecSpec::command(vec!["true".into()]);
        labeled.labels = vec![(TENANT_LABEL.into(), "tenant-b".into())];
        sandbox.spawn(&labeled).await.unwrap();
        let seen = capture.last.lock().unwrap().clone().unwrap();
        assert_eq!(
            seen.labels,
            vec![(TENANT_LABEL.to_string(), "tenant-b".to_string())]
        );
        assert!(!format!("{sandbox:?}").is_empty());
    }

    #[test]
    fn config_without_a_control_network_is_refused() {
        let cfg = EgressConfig {
            network: DEFAULT_NETWORK.into(),
            proxy_container: DEFAULT_PROXY_CONTAINER.into(),
            proxy_image: "img".into(),
            control_plane_url: "http://cp:8080".into(),
            token: "t".into(),
            control_networks: Vec::new(),
        };
        assert!(matches!(
            cfg.validate(),
            Err(SandboxError::NotConfigured(_))
        ));
        let mut ok = cfg.clone();
        ok.control_networks = vec!["aria_default".into()];
        assert!(ok.validate().is_ok());
        assert_eq!(ok.proxy_url_for_sandbox(), "http://aria-egress-proxy:8080");
    }

    #[test]
    fn blank_names_are_refused() {
        let cfg = EgressConfig {
            network: "  ".into(),
            proxy_container: DEFAULT_PROXY_CONTAINER.into(),
            proxy_image: "img".into(),
            control_plane_url: "http://cp:8080".into(),
            token: "t".into(),
            control_networks: vec!["n".into()],
        };
        assert!(matches!(
            cfg.validate(),
            Err(SandboxError::NotConfigured(_))
        ));
    }

    // --- Scripted Docker Engine API ---------------------------------------
    //
    // Every request is answered from a queue: a test queues exactly the call
    // sequence `ensure` / `probe` / `resolve_peer` make, so each branch is
    // deterministic and no daemon is involved. `bollard` is spoken to over a
    // real loopback socket, so the request building and response parsing under
    // test are the ones a deployment uses.

    struct Reply {
        status: u16,
        body: String,
    }

    impl Reply {
        fn ok(body: impl Into<String>) -> Self {
            Self {
                status: 200,
                body: body.into(),
            }
        }
        fn no_content() -> Self {
            Self {
                status: 204,
                body: String::new(),
            }
        }
        fn status(status: u16) -> Self {
            Self {
                status,
                body: format!("{{\"message\":\"refused ({status})\"}}"),
            }
        }
    }

    struct Engine {
        endpoint: String,
        seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Engine {
        /// `script` entries are `(method-and-path fragment, reply)`; a request
        /// takes the first entry whose fragment it matches, so the order of the
        /// queue is the order of the calls.
        fn start(script: Vec<(&str, Reply)>) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let queue = std::sync::Arc::new(std::sync::Mutex::new(
                script
                    .into_iter()
                    .map(|(fragment, reply)| (fragment.to_string(), reply))
                    .collect::<Vec<_>>(),
            ));
            let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = std::sync::Arc::clone(&seen);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    serve(&mut stream, &queue, &recorded);
                }
            });
            Self { endpoint, seen }
        }

        fn sandbox(&self) -> DockerSandbox {
            DockerSandbox::with_client(
                bollard::Docker::connect_with_http(&self.endpoint, 5, bollard::API_DEFAULT_VERSION)
                    .unwrap(),
            )
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn serve(
        stream: &mut std::net::TcpStream,
        queue: &std::sync::Mutex<Vec<(String, Reply)>>,
        seen: &std::sync::Mutex<Vec<String>>,
    ) {
        use std::io::{Read, Write};
        let mut buf = Vec::new();
        let mut chunk = [0u8; 512];
        let head_end = loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut parts = head.lines().next().unwrap_or_default().split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        seen.lock().unwrap().push(format!("{method} {path}"));

        // Drain the declared body so the client is not writing into a closed
        // socket (container create carries a JSON config).
        let declared = head
            .lines()
            .find_map(|line| {
                let line = line.to_ascii_lowercase();
                line.strip_prefix("content-length:")
                    .map(|value| value.trim().to_string())
            })
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < head_end + declared {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }

        let call = format!("{method} {path}");
        let reply = {
            let mut queue = queue.lock().unwrap();
            match queue
                .iter()
                .position(|(fragment, _)| call.contains(fragment.as_str()))
            {
                Some(index) => queue.remove(index).1,
                None => Reply::status(404),
            }
        };
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            reply.status,
            if reply.status < 300 { "OK" } else { "Error" },
            reply.body.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(reply.body.as_bytes());
        let _ = stream.flush();
    }

    fn governed() -> EgressConfig {
        EgressConfig {
            network: DEFAULT_NETWORK.into(),
            proxy_container: DEFAULT_PROXY_CONTAINER.into(),
            proxy_image: "aria-egress-proxy:test".into(),
            control_plane_url: "http://cloud:8080".into(),
            token: "tok".into(),
            control_networks: vec!["aria_default".into()],
        }
    }

    fn network(name: &str, proxy_container: &str) -> EgressNetwork {
        EgressNetwork {
            network: name.into(),
            proxy_container: proxy_container.into(),
        }
    }

    /// The same shape as `inspect`, as the JSON the engine would return.
    fn inspect_body(tenant: Option<&str>, ip: Option<&str>) -> String {
        let labels = tenant
            .map(|t| format!("\"{TENANT_LABEL}\":\"{t}\""))
            .unwrap_or_default();
        let networks = ip
            .map(|ip| format!("\"{DEFAULT_NETWORK}\":{{\"IPAddress\":\"{ip}\"}}"))
            .unwrap_or_default();
        format!(
            "{{\"Id\":\"c1\",\"State\":{{\"Running\":true,\"Status\":\"running\"}},\
              \"Config\":{{\"Labels\":{{{labels}}}}},\
              \"NetworkSettings\":{{\"Networks\":{{{networks}}}}}}}"
        )
    }

    fn inspect_json(running: bool, attached: &[&str]) -> String {
        let networks = attached
            .iter()
            .map(|name| format!("\"{name}\":{{\"IPAddress\":\"10.0.0.9\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"Id\":\"c1\",\"Name\":\"/{DEFAULT_PROXY_CONTAINER}\",\
              \"State\":{{\"Running\":{running},\"Status\":\"{}\"}},\
              \"Config\":{{\"Labels\":{{}}}},\
              \"NetworkSettings\":{{\"Networks\":{{{networks}}}}}}}",
            if running { "running" } else { "exited" }
        )
    }

    #[tokio::test]
    async fn ensure_creates_the_network_and_brings_the_proxy_up() {
        let engine = Engine::start(vec![
            ("/networks", Reply::ok("[]")),
            ("/aria-egress-proxy/json", Reply::status(404)),
            (
                "/networks/create",
                Reply::ok("{\"Id\":\"n1\",\"Warning\":\"\"}"),
            ),
            (
                "/containers/create",
                Reply::ok("{\"Id\":\"c1\",\"Warnings\":[]}"),
            ),
            ("/aria_default/connect", Reply::ok("{}")),
            ("/aria-egress/connect", Reply::ok("{}")),
            ("/aria-egress-proxy/start", Reply::no_content()),
            (
                "/aria-egress-proxy/json",
                Reply::ok(inspect_json(true, &["aria_default", "aria-egress"])),
            ),
        ]);
        let sandbox = engine.sandbox();
        let egress = EgressNetwork::ensure(&sandbox, &governed()).await.unwrap();
        assert_eq!(egress.network(), DEFAULT_NETWORK);
        assert_eq!(egress.proxy_container(), DEFAULT_PROXY_CONTAINER);

        let calls = engine.seen();
        assert!(calls.iter().any(|c| c.contains("/networks/create")));
        assert!(calls.iter().any(|c| c.contains("/containers/create")));
        assert_eq!(
            calls.iter().filter(|c| c.contains("/connect")).count(),
            2,
            "the proxy joins the control plane and the egress network"
        );
    }

    #[tokio::test]
    async fn ensure_is_a_no_op_when_the_proxy_is_already_governed() {
        let engine = Engine::start(vec![
            (
                "/networks",
                Reply::ok("[{\"Name\":\"aria-egress\",\"Id\":\"n1\",\"Driver\":\"bridge\"}]"),
            ),
            (
                "/aria-egress-proxy/json",
                Reply::ok(inspect_json(true, &["aria_default", "aria-egress"])),
            ),
            (
                "/aria-egress-proxy/json",
                Reply::ok(inspect_json(true, &["aria_default", "aria-egress"])),
            ),
        ]);
        EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .expect("nothing to do");
        assert_eq!(
            engine.seen().iter().filter(|c| c.contains("POST")).count(),
            0,
            "a governed proxy is never re-created"
        );
    }

    #[tokio::test]
    async fn ensure_reports_each_failed_step() {
        // Network creation fails.
        let engine = Engine::start(vec![
            ("/networks", Reply::ok("[]")),
            ("GET /containers/", Reply::status(404)),
            ("/networks/create", Reply::status(500)),
        ]);
        let err = EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("create network"));

        // Proxy creation fails.
        let engine = Engine::start(vec![
            ("/networks", Reply::ok("[]")),
            ("GET /containers/", Reply::status(404)),
            (
                "/networks/create",
                Reply::ok("{\"Id\":\"n1\",\"Warning\":\"\"}"),
            ),
            ("/containers/create", Reply::status(500)),
        ]);
        let err = EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("create proxy container"));

        // Attaching to the control plane fails.
        let engine = Engine::start(vec![
            ("/networks", Reply::ok("[]")),
            ("GET /containers/", Reply::status(404)),
            (
                "/networks/create",
                Reply::ok("{\"Id\":\"n1\",\"Warning\":\"\"}"),
            ),
            (
                "/containers/create",
                Reply::ok("{\"Id\":\"c1\",\"Warnings\":[]}"),
            ),
            ("/aria_default/connect", Reply::status(500)),
        ]);
        let err = EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("attach proxy to network"));

        // Starting the proxy fails.
        let engine = Engine::start(vec![
            ("/networks", Reply::ok("[]")),
            ("GET /containers/", Reply::status(404)),
            (
                "/networks/create",
                Reply::ok("{\"Id\":\"n1\",\"Warning\":\"\"}"),
            ),
            (
                "/containers/create",
                Reply::ok("{\"Id\":\"c1\",\"Warnings\":[]}"),
            ),
            ("/aria_default/connect", Reply::ok("{}")),
            ("/aria-egress/connect", Reply::status(500)),
        ]);
        let err = EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("attach proxy to"));
    }

    #[tokio::test]
    async fn probe_refuses_a_missing_stopped_or_detached_proxy() {
        let sandbox = Engine::start(vec![("GET /containers/", Reply::status(404))]).sandbox();
        let err = network(DEFAULT_NETWORK, DEFAULT_PROXY_CONTAINER)
            .probe(&sandbox)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("is missing"));

        let sandbox = Engine::start(vec![(
            "GET /containers/",
            Reply::ok(inspect_json(false, &["aria-egress"])),
        )])
        .sandbox();
        let err = network(DEFAULT_NETWORK, DEFAULT_PROXY_CONTAINER)
            .probe(&sandbox)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("is not running"));

        let sandbox = Engine::start(vec![(
            "GET /containers/",
            Reply::ok(inspect_json(true, &["aria_default"])),
        )])
        .sandbox();
        let err = network(DEFAULT_NETWORK, DEFAULT_PROXY_CONTAINER)
            .probe(&sandbox)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("is not attached"));
    }

    #[tokio::test]
    async fn health_is_optional() {
        let sandbox = Engine::start(vec![("GET /containers/", Reply::status(404))]).sandbox();
        assert!(network(DEFAULT_NETWORK, DEFAULT_PROXY_CONTAINER)
            .health(&sandbox)
            .await
            .is_none());

        let sandbox = Engine::start(vec![(
            "GET /containers/",
            Reply::ok(inspect_json(true, &["aria-egress"])),
        )])
        .sandbox();
        assert!(network(DEFAULT_NETWORK, DEFAULT_PROXY_CONTAINER)
            .health(&sandbox)
            .await
            .is_none());

        let sandbox = Engine::start(vec![(
            "GET /containers/",
            Reply::ok(
                "{\"Id\":\"c1\",\"State\":{\"Running\":true,\"Health\":{\"Status\":\"healthy\"}}}",
            ),
        )])
        .sandbox();
        assert_eq!(
            network(DEFAULT_NETWORK, DEFAULT_PROXY_CONTAINER)
                .health(&sandbox)
                .await,
            Some(HealthStatusEnum::HEALTHY)
        );
    }

    #[tokio::test]
    async fn resolve_peer_attributes_an_ip_through_labels() {
        // The sandbox and the proxy both sit on the network; only the labelled
        // one is attributable.
        let engine = Engine::start(vec![
            (
                "/containers/json",
                Reply::ok(
                    "[{\"Id\":\"c1\",\"Names\":[\"/aria-egress-proxy\"]},\
                      {\"Id\":\"c2\",\"Names\":[\"/sbx-1\"]},\
                      {\"Id\":\"c3\"}]",
                ),
            ),
            (
                "/aria-egress-proxy/json",
                Reply::ok(inspect_json(true, &["aria-egress"])),
            ),
            (
                "/containers/sbx-1/json",
                Reply::ok(inspect_body(Some("tenant-a"), Some("10.0.0.9"))),
            ),
            // A container that cannot be inspected is skipped, not fatal.
            ("GET /containers/", Reply::status(500)),
        ]);
        let found = EgressNetwork::resolve_peer(&engine.sandbox(), "aria-egress", "10.0.0.9")
            .await
            .unwrap()
            .expect("attributed");
        assert_eq!(found.principal_id, "tenant-a");
        assert_eq!(found.sandbox_id, "sbx-1");

        // An IP nobody owns is unattributable.
        let engine = Engine::start(vec![
            (
                "/containers/json",
                Reply::ok("[{\"Id\":\"c1\",\"Names\":[\"/sbx-1\"]}]"),
            ),
            (
                "/containers/sbx-1/json",
                Reply::ok(inspect_body(Some("tenant-a"), Some("10.0.0.5"))),
            ),
        ]);
        assert!(
            EgressNetwork::resolve_peer(&engine.sandbox(), "aria-egress", "10.0.0.9")
                .await
                .unwrap()
                .is_none()
        );

        // A listing failure is reported.
        let engine = Engine::start(vec![("/containers/json", Reply::status(500))]);
        let err = EgressNetwork::resolve_peer(&engine.sandbox(), "aria-egress", "10.0.0.9")
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("list sandbox containers"));
    }

    // Async mutex: the guard is deliberately held across the awaits below.
    static HOSTNAME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn control_networks_come_from_the_hostname_container() {
        let _guard = HOSTNAME_LOCK.lock().await;
        let saved = std::env::var("HOSTNAME").ok();
        std::env::set_var("HOSTNAME", "cloud");

        let engine = Engine::start(vec![(
            "/containers/cloud/json",
            Reply::ok(inspect_json(true, &["aria_default", "aria-egress"])),
        )]);
        let nets = EgressNetwork::control_networks(&engine.sandbox())
            .await
            .unwrap();
        assert!(nets.contains(&"aria_default".to_string()));

        // A host that is not a container cannot discover its networks.
        let engine = Engine::start(vec![("/containers/cloud/json", Reply::status(404))]);
        assert!(EgressNetwork::control_networks(&engine.sandbox())
            .await
            .is_err());

        // A container with no networks is refused too.
        let engine = Engine::start(vec![(
            "/containers/cloud/json",
            Reply::ok("{\"Id\":\"c1\",\"NetworkSettings\":{\"Networks\":{}}}"),
        )]);
        let err = EgressNetwork::control_networks(&engine.sandbox())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no Docker network"));

        // An empty HOSTNAME is refused before any call.
        std::env::set_var("HOSTNAME", "  ");
        let err = EgressNetwork::control_networks(&engine.sandbox())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("HOSTNAME is empty"));

        match saved {
            Some(value) => std::env::set_var("HOSTNAME", value),
            None => std::env::remove_var("HOSTNAME"),
        }
    }

    // --- the remaining branches of the bring-up and the decorator ---------

    #[test]
    fn a_session_label_is_trimmed_and_a_blank_one_is_dropped() {
        let with = |session: &str| {
            let mut labels = HashMap::new();
            labels.insert(TENANT_LABEL.to_string(), "tenant-a".to_string());
            labels.insert(SESSION_LABEL.to_string(), session.to_string());
            ContainerInspectResponse {
                config: Some(ContainerConfig {
                    labels: Some(labels),
                    ..Default::default()
                }),
                ..Default::default()
            }
        };
        assert_eq!(
            PeerIdentity::from_inspect("sbx", &with("  s-1  "))
                .unwrap()
                .session_id
                .as_deref(),
            Some("s-1")
        );
        assert!(PeerIdentity::from_inspect("sbx", &with("   "))
            .unwrap()
            .session_id
            .is_none());
    }

    #[test]
    fn an_empty_or_missing_address_is_not_a_peer() {
        let mut networks = HashMap::new();
        networks.insert(
            DEFAULT_NETWORK.to_string(),
            EndpointSettings {
                ip_address: Some("   ".into()),
                ..Default::default()
            },
        );
        let container = ContainerInspectResponse {
            network_settings: Some(NetworkSettings {
                networks: Some(networks),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(PeerIdentity::ip_on(&container, DEFAULT_NETWORK).is_none());
        // No network settings at all is the same answer.
        assert!(PeerIdentity::ip_on(&ContainerInspectResponse::default(), "any").is_none());
    }

    static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_config_comes_from_the_environment_or_a_lookup() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let keys = [
            "EGRESS_PROXY_IMAGE",
            "CONTROL_PLANE_URL",
            "EGRESS_PROXY_TOKEN",
            "AGENT_EGRESS_NETWORK",
            "EGRESS_PROXY_CONTAINER",
        ];
        // Pre-seed one key so the restore loop sees both of its arms.
        std::env::set_var("AGENT_EGRESS_NETWORK", "aria-before-test");
        let saved: Vec<Option<String>> = keys.iter().map(|k| std::env::var(k).ok()).collect();
        std::env::set_var("EGRESS_PROXY_IMAGE", "img:test");
        std::env::set_var("CONTROL_PLANE_URL", "http://cp:8080");
        std::env::set_var("EGRESS_PROXY_TOKEN", "tok");
        std::env::set_var("AGENT_EGRESS_NETWORK", "  ");
        let cfg = EgressConfig::from_env().expect("three required keys are set");
        assert_eq!(cfg.proxy_image, "img:test");
        assert_eq!(
            cfg.network, DEFAULT_NETWORK,
            "blank falls back to the default"
        );
        assert_eq!(cfg.proxy_container, DEFAULT_PROXY_CONTAINER);
        for (key, value) in keys.iter().zip(saved) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }

        // A missing required key is a configuration error, not a default.
        let err = EgressConfig::from_lookup(|key| match key {
            "EGRESS_PROXY_IMAGE" => Some("img".into()),
            _ => None,
        })
        .unwrap_err();
        assert!(format!("{err}").contains("CONTROL_PLANE_URL must be set"));

        // An explicit lookup supplies every value, blank optional ones too.
        let values: HashMap<&str, &str> = HashMap::from([
            ("EGRESS_PROXY_IMAGE", "  img  "),
            ("CONTROL_PLANE_URL", "http://cp"),
            ("EGRESS_PROXY_TOKEN", "tok"),
            ("AGENT_EGRESS_NETWORK", "  "),
            ("EGRESS_PROXY_CONTAINER", ""),
        ]);
        let cfg = EgressConfig::from_lookup(|key| values.get(key).map(|v| v.to_string()))
            .expect("lookup");
        assert_eq!(cfg.proxy_image, "img");
        assert_eq!(cfg.network, DEFAULT_NETWORK);
        assert_eq!(cfg.proxy_container, DEFAULT_PROXY_CONTAINER);
        assert!(cfg.control_networks.is_empty());
    }

    #[tokio::test]
    async fn ensure_reconnects_an_existing_proxy_and_starts_it() {
        let engine = Engine::start(vec![
            (
                "/networks",
                Reply::ok("[{\"Name\":\"aria-egress\",\"Id\":\"n1\"}]"),
            ),
            (
                "/aria-egress-proxy/json",
                Reply::ok(inspect_json(false, &[])),
            ),
            ("/aria_default/connect", Reply::ok("{}")),
            ("/aria-egress/connect", Reply::ok("{}")),
            ("/aria-egress-proxy/start", Reply::no_content()),
            (
                "/aria-egress-proxy/json",
                Reply::ok(inspect_json(true, &["aria_default", "aria-egress"])),
            ),
        ]);
        EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .expect("reconnected and started");
        let calls = engine.seen();
        assert_eq!(calls.iter().filter(|c| c.contains("/connect")).count(), 2);
        assert!(calls.iter().any(|c| c.contains("/start")));
    }

    #[tokio::test]
    async fn ensure_reports_a_failed_reconnect() {
        // Control-plane reconnect refused.
        let engine = Engine::start(vec![
            (
                "/networks",
                Reply::ok("[{\"Name\":\"aria-egress\",\"Id\":\"n1\"}]"),
            ),
            (
                "/aria-egress-proxy/json",
                Reply::ok(inspect_json(true, &[])),
            ),
            ("/aria_default/connect", Reply::status(500)),
        ]);
        let err = EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("attach proxy to control network"));

        // Egress reconnect refused.
        let engine = Engine::start(vec![
            (
                "/networks",
                Reply::ok("[{\"Name\":\"aria-egress\",\"Id\":\"n1\"}]"),
            ),
            (
                "/aria-egress-proxy/json",
                Reply::ok(inspect_json(true, &["aria_default"])),
            ),
            ("/aria-egress/connect", Reply::status(500)),
        ]);
        let err = EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("attach proxy to aria-egress"));
    }

    #[tokio::test]
    async fn ensure_reports_a_listing_or_start_failure() {
        let engine = Engine::start(vec![("/networks", Reply::status(500))]);
        let err = EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("list docker networks"));

        // A stopped proxy is (re)started; the engine refuses the start.
        let stopped = inspect_json(false, &["aria_default", "aria-egress"]);
        let engine = Engine::start(vec![
            (
                "/networks",
                Reply::ok("[{\"Name\":\"aria-egress\",\"Id\":\"n1\"}]"),
            ),
            ("/aria-egress-proxy/json", Reply::ok(stopped)),
            ("/aria-egress-proxy/start", Reply::status(500)),
        ]);
        let err = EgressNetwork::ensure(&engine.sandbox(), &governed())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("start proxy container"));
    }

    #[tokio::test]
    async fn resolve_peer_skips_unnamed_and_unreadable_containers() {
        let engine = Engine::start(vec![
            (
                "/containers/json",
                Reply::ok("[{\"Id\":\"c1\",\"Names\":[\"/ghost\"]},{\"Id\":\"c2\"}]"),
            ),
            ("/containers/ghost/json", Reply::status(500)),
        ]);
        assert!(
            EgressNetwork::resolve_peer(&engine.sandbox(), "aria-egress", "10.0.0.9")
                .await
                .unwrap()
                .is_none(),
            "an unreadable container and an unnamed one are both skipped"
        );
    }

    #[tokio::test]
    async fn the_decorator_forwards_every_lifecycle_call() {
        let capture = std::sync::Arc::new(Capture::default());
        let sandbox = NetworkingSandbox::new(
            capture,
            "aria-egress",
            vec![(TENANT_LABEL.into(), "tenant-a".into())],
            Vec::new(),
        );
        let handle = crate::SandboxHandle { id: "sbx".into() };
        // `Capture` implements only spawn/exec/destroy; the rest come from the
        // trait's defaults. What matters here is that the decorator forwards.
        assert!(sandbox.exec(&handle, &["true".into()]).await.is_err());
        sandbox.destroy(handle.clone()).await.unwrap();
        let _ = sandbox.capabilities();
        let _ = sandbox.status(&handle).await;
        let _ = sandbox.pause(&handle).await;
        let _ = sandbox.resume(&handle).await;
        let _ = sandbox.suspend(&handle).await;
        let _ = sandbox.checkpoint(&handle).await;
        let _ = sandbox
            .restore(
                &crate::CheckpointRef::new("img", "sbx", 1),
                &crate::ExecSpec::command(vec!["true".into()]),
            )
            .await;
    }

    /// The stub's own defensive arms: a peer that closes before sending a whole
    /// request, and a body that arrives after the headers.
    #[tokio::test]
    async fn the_stub_handles_a_truncated_request_and_a_split_body() {
        use std::io::{Read, Write};
        let engine = Engine::start(vec![("/containers/create", Reply::ok("{}"))]);
        let addr = engine.endpoint.trim_start_matches("http://").to_string();

        let mut truncated = std::net::TcpStream::connect(&addr).unwrap();
        truncated.write_all(b"GET /containers/json").unwrap();
        truncated.flush().unwrap();
        drop(truncated);

        let mut split = std::net::TcpStream::connect(&addr).unwrap();
        split
            .write_all(
                b"POST /containers/create HTTP/1.1\r\nHost: stub\r\nContent-Length: 5\r\n\r\n",
            )
            .unwrap();
        split.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        split.write_all(b"hello").unwrap();
        let mut answer = Vec::new();
        let _ = split.read_to_end(&mut answer);
        assert!(String::from_utf8_lossy(&answer).contains("200 OK"));

        // A body that never arrives ends the drain loop instead of hanging.
        let mut short = std::net::TcpStream::connect(&addr).unwrap();
        short
            .write_all(
                b"POST /containers/create HTTP/1.1\r\nHost: stub\r\nContent-Length: 9\r\n\r\nhi",
            )
            .unwrap();
        short.flush().unwrap();
        drop(short);

        // A call nobody scripted answers 404 rather than panicking.
        let empty = Engine::start(Vec::new());
        let mut unmatched =
            std::net::TcpStream::connect(empty.endpoint.trim_start_matches("http://").to_string())
                .unwrap();
        unmatched
            .write_all(b"GET /networks HTTP/1.1\r\nHost: stub\r\n\r\n")
            .unwrap();
        let mut answer = Vec::new();
        let _ = unmatched.read_to_end(&mut answer);
        assert!(String::from_utf8_lossy(&answer).contains("404"));
    }
}
