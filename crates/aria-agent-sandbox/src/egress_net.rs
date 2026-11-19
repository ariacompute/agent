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
        steps.push(Step::CreateProxy);
    }
    if !proxy_running {
        steps.push(Step::StartProxy);
    }
    if !proxy_on_control {
        steps.push(Step::ConnectProxyToControl);
    }
    if !proxy_on_egress {
        steps.push(Step::ConnectProxyToEgress);
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
}
