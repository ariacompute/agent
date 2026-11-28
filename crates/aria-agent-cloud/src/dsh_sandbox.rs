//! Boot-time wiring for the **sandboxed** dsh runtime (ADR-0027).
//!
//! The runtime itself lives in `aria-agent-dsh`; this module owns the parts that
//! are deployment policy: which image, how the session workspace is backed, what
//! the container is allowed to do, and the two preflight checks that must fail
//! **before** the first turn instead of during it:
//!
//! * the sandbox image must already exist locally (this repo never pulls);
//! * a containerised harness cannot reach the control plane's `127.0.0.1`, so an
//!   enabled thinking filter needs an explicitly reachable URL.
//!
//! Everything here is pure configuration plus one `Sandbox` call, so it is unit
//! tested with a stub backend instead of a daemon.

use agent_sandbox::{parse_size, proxy_env, Sandbox, SandboxError, SandboxResourceLimits};
use aria_agent_dsh::{DshSandbox, SandboxMount, CONTAINER_PATH, DEFAULT_BIN, DEFAULT_ROOT};

/// Default image built by `docker build --target sandbox`.
pub const DEFAULT_IMAGE: &str = "aria-dsh-sandbox:latest";
/// Non-root uid/gid of that image.
pub const DEFAULT_USER: &str = "10001:10001";
/// Default volume-name prefix (`aria-dsh-<tenant>-<session>`).
pub const DEFAULT_VOLUME_PREFIX: &str = "aria-dsh";

/// Deployment policy for one sandboxed harness container.
#[derive(Debug, Clone, PartialEq)]
pub struct SandboxedDshConfig {
    pub image: String,
    pub mount: SandboxMount,
    pub root: String,
    pub bin: String,
    pub volume_prefix: String,
    pub resources: SandboxResourceLimits,
    pub user: String,
    /// Drop every capability and set `no-new-privileges`.
    pub harden: bool,
}

impl Default for SandboxedDshConfig {
    fn default() -> Self {
        Self {
            image: DEFAULT_IMAGE.to_string(),
            mount: SandboxMount::Volume,
            root: DEFAULT_ROOT.to_string(),
            bin: DEFAULT_BIN.to_string(),
            volume_prefix: DEFAULT_VOLUME_PREFIX.to_string(),
            resources: SandboxResourceLimits {
                cpus: 1.0,
                // The harness is heavier than a shell command: budget a GiB and
                // let the operator lower it after the first real measurement.
                memory_bytes: 1024 * 1024 * 1024,
                pids_limit: 512,
            },
            user: DEFAULT_USER.to_string(),
            harden: true,
        }
    }
}

impl SandboxedDshConfig {
    /// Resolve from the process environment.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Resolve from an arbitrary lookup (tests inject a map).
    ///
    /// `DSH_SANDBOX` defaults to on: a deployment that runs `harness = dsh`
    /// intends the sandbox, and silently keeping the host process would make
    /// "sandboxed" an unchecked claim. `DSH_SANDBOX=0` is the explicit escape
    /// hatch and is logged by the caller.
    pub fn from_lookup<F>(lookup: F) -> Result<Option<Self>, String>
    where
        F: Fn(&str) -> Option<String>,
    {
        let enabled = match lookup("DSH_SANDBOX") {
            Some(raw) => parse_flag(&raw)?,
            None => true,
        };
        if !enabled {
            return Ok(None);
        }
        let mut cfg = Self::default();
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_IMAGE") {
            cfg.image = raw;
        }
        if cfg.image.trim().is_empty() {
            return Err("DSH_SANDBOX_IMAGE must not be empty".into());
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_MOUNT") {
            cfg.mount = SandboxMount::parse(&raw).ok_or_else(|| {
                format!("unknown DSH_SANDBOX_MOUNT `{raw}` (expected volume|bind)")
            })?;
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_ROOT") {
            cfg.root = raw;
        }
        if !cfg.root.starts_with('/') {
            return Err(format!(
                "DSH_SANDBOX_ROOT must be absolute, got `{}`",
                cfg.root
            ));
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_BIN") {
            cfg.bin = raw;
        }
        if cfg.bin.trim().is_empty() {
            return Err("DSH_SANDBOX_BIN must not be empty".into());
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_VOLUME_PREFIX") {
            cfg.volume_prefix = raw;
        }
        if cfg.volume_prefix.trim().is_empty() {
            return Err("DSH_SANDBOX_VOLUME_PREFIX must not be empty".into());
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_USER") {
            cfg.user = raw;
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_CPUS") {
            cfg.resources.cpus = parse_cpus(&raw)?;
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_MEMORY") {
            cfg.resources.memory_bytes = parse_size(&raw)
                .ok_or_else(|| format!("invalid DSH_SANDBOX_MEMORY `{raw}` (e.g. 1g, 768m)"))?;
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_PIDS") {
            cfg.resources.pids_limit = raw.trim().parse::<i64>().map_err(|_| {
                format!("invalid DSH_SANDBOX_PIDS `{raw}` (expected a positive integer)")
            })?;
            if cfg.resources.pids_limit <= 0 {
                return Err(format!("invalid DSH_SANDBOX_PIDS `{raw}` (must be > 0)"));
            }
        }
        if let Some(raw) = value(&lookup, "DSH_SANDBOX_HARDEN") {
            cfg.harden = parse_flag(&raw)?;
        }
        Ok(Some(cfg))
    }

    /// The runtime-side configuration for one tenant.
    ///
    /// `session_root` only contributes the tenant component of the volume name
    /// (and the host directory in bind mode).
    pub fn to_dsh_sandbox(
        &self,
        backend: std::sync::Arc<dyn Sandbox>,
        network: Option<String>,
        labels: Vec<(String, String)>,
    ) -> DshSandbox {
        let hardening = if self.harden {
            let hard = agent_sandbox::SandboxHardening::locked_down();
            match self.user.trim() {
                "" => hard,
                user => hard.with_user(user),
            }
        } else {
            agent_sandbox::SandboxHardening::default()
        };
        DshSandbox {
            sandbox: backend,
            image: self.image.clone(),
            mount: self.mount,
            root: self.root.clone(),
            bin: self.bin.clone(),
            volume_prefix: self.volume_prefix.clone(),
            resources: self.resources,
            hardening,
            network,
            labels,
        }
    }

    /// Environment for the harness container.
    ///
    /// `proxy_env` is the single source of the proxy variable set (ADR-0024/0025);
    /// the reachable thinking-filter host is added to `NO_PROXY` because routing
    /// the filter through the egress proxy would deny it (the proxy governs
    /// outbound traffic, not control-plane services).
    pub fn container_proxy_env(
        &self,
        proxy_url: &str,
        filter_url: Option<&str>,
    ) -> Vec<(String, String)> {
        if proxy_url.trim().is_empty() {
            return Vec::new();
        }
        let mut env = proxy_env(proxy_url.trim());
        if let Some(host) = filter_url.and_then(host_of) {
            for (name, value) in env.iter_mut() {
                if name == "NO_PROXY" && !value.split(',').any(|part| part == host) {
                    value.push(',');
                    value.push_str(host);
                }
            }
        }
        env
    }
}

/// Fail boot when the named image is absent: this repo has no registry client, so
/// "pull it later" is a 500 on the first turn instead of an actionable startup
/// error.
pub async fn preflight(backend: &dyn Sandbox, cfg: &SandboxedDshConfig) -> Result<(), String> {
    match backend.has_image(&cfg.image).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "sandbox image `{}` is not present locally; build it with \
             `docker build --target sandbox -t {} .` (this deployment never pulls images)",
            cfg.image, cfg.image
        )),
        Err(SandboxError::Unsupported(_)) => Err(format!(
            "sandbox backend cannot verify image `{}`; refusing to start a \
             sandboxed harness blind",
            cfg.image
        )),
        Err(e) => Err(format!("cannot inspect sandbox image `{}`: {e}", cfg.image)),
    }
}

/// Base URL the harness must use for the thinking filter.
///
/// In host mode the loopback URL works. In sandbox mode `127.0.0.1` is the
/// container itself, so an operator-supplied, container-reachable URL is
/// mandatory — and it must not be loopback, or every second turn would fail with
/// `UNSUPPORTED_CONTENT` after the filter silently stopped filtering.
pub fn thinking_filter_base(
    enabled: bool,
    sandboxed: bool,
    local: Option<&str>,
    configured: Option<&str>,
) -> Result<Option<String>, String> {
    if !enabled {
        return Ok(None);
    }
    let configured = configured.map(str::trim).filter(|v| !v.is_empty());
    if sandboxed {
        let Some(url) = configured else {
            return Err("DSH_THINKING_FILTER=enable with a sandboxed harness needs \
                 DSH_THINKING_FILTER_URL (a container-reachable address; \
                 127.0.0.1 inside the container is the container itself). \
                 Set it, or set DSH_THINKING_FILTER=disable."
                .into());
        };
        if is_loopback(url) {
            return Err(format!(
                "DSH_THINKING_FILTER_URL `{url}` is loopback; a sandboxed \
                 harness cannot reach the control plane's loopback"
            ));
        }
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(format!(
                "DSH_THINKING_FILTER_URL `{url}` must be an http(s) URL"
            ));
        }
        return Ok(Some(url.to_string()));
    }
    Ok(configured
        .map(str::to_string)
        .or_else(|| local.map(str::to_string)))
}

/// Host component of an `http(s)://host[:port]` URL, without the port.
pub fn host_of(url: &str) -> Option<&str> {
    let rest = url.trim();
    let rest = rest.split("://").nth(1).unwrap_or(rest);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?;
    if authority.is_empty() {
        return None;
    }
    if let Some(end) = authority
        .strip_prefix('[')
        .and_then(|v| v.split(']').next())
    {
        return Some(&authority[..end.len() + 2]);
    }
    Some(authority.split(':').next().unwrap_or(authority))
}

/// Whether a URL points at loopback (unreachable from inside a container).
pub fn is_loopback(url: &str) -> bool {
    match host_of(url) {
        Some(host) => {
            let host = host.trim_matches(['[', ']']);
            host.eq_ignore_ascii_case("localhost") || host == "::1" || host.starts_with("127.")
        }
        None => true,
    }
}

/// Read one setting.
///
/// An explicitly empty value is **not** "unset": `DSH_SANDBOX_IMAGE=` must fail
/// loudly instead of silently selecting a different image than the operator
/// wrote (fail-closed policy).
fn value<F>(lookup: &F, key: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key).map(|v| v.trim().to_string())
}

fn parse_flag(raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(format!(
            "invalid boolean `{other}` (expected 1/0, true/false, yes/no, on/off)"
        )),
    }
}

fn parse_cpus(raw: &str) -> Result<f64, String> {
    let value = raw
        .trim()
        .parse::<f64>()
        .map_err(|_| format!("invalid DSH_SANDBOX_CPUS `{raw}`"))?;
    if value <= 0.0 {
        return Err(format!("invalid DSH_SANDBOX_CPUS `{raw}` (must be > 0)"));
    }
    Ok(value)
}

/// The image's `PATH`, re-exported so callers can assert on the container env.
pub const SANDBOX_PATH: &str = CONTAINER_PATH;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_sandbox::{ExecOutput, ExecSpec, SandboxHandle};
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |key: &str| map.get(key).cloned()
    }

    #[test]
    fn sandboxing_is_on_by_default() {
        let cfg = SandboxedDshConfig::from_lookup(lookup(&[]))
            .unwrap()
            .expect("sandboxed by default");
        assert_eq!(cfg.image, DEFAULT_IMAGE);
        assert_eq!(cfg.mount, SandboxMount::Volume);
        assert_eq!(cfg.root, DEFAULT_ROOT);
        assert_eq!(cfg.resources.memory_bytes, 1024 * 1024 * 1024);
        assert!(cfg.harden);
    }

    #[test]
    fn the_escape_hatch_is_explicit() {
        for raw in ["0", "false", "NO", "off"] {
            let cfg = SandboxedDshConfig::from_lookup(lookup(&[("DSH_SANDBOX", raw)]))
                .expect("valid flag");
            assert!(cfg.is_none(), "{raw}");
        }
    }

    #[test]
    fn unknown_flags_are_refused_instead_of_guessed() {
        let err = SandboxedDshConfig::from_lookup(lookup(&[("DSH_SANDBOX", "maybe")])).unwrap_err();
        assert!(err.contains("invalid boolean"), "{err}");
        let err = SandboxedDshConfig::from_lookup(lookup(&[
            ("DSH_SANDBOX", "1"),
            ("DSH_SANDBOX_HARDEN", "sure"),
        ]))
        .unwrap_err();
        assert!(err.contains("invalid boolean"), "{err}");
    }

    #[test]
    fn overrides_are_parsed_and_validated() {
        let cfg = SandboxedDshConfig::from_lookup(lookup(&[
            ("DSH_SANDBOX_IMAGE", "custom:1"),
            ("DSH_SANDBOX_MOUNT", "BIND"),
            ("DSH_SANDBOX_ROOT", "/sessions"),
            ("DSH_SANDBOX_BIN", "/opt/dsh"),
            ("DSH_SANDBOX_VOLUME_PREFIX", "aria-x"),
            ("DSH_SANDBOX_USER", "2000:2000"),
            ("DSH_SANDBOX_CPUS", "2.5"),
            ("DSH_SANDBOX_MEMORY", "768m"),
            ("DSH_SANDBOX_PIDS", "300"),
            ("DSH_SANDBOX_HARDEN", "0"),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(cfg.image, "custom:1");
        assert_eq!(cfg.mount, SandboxMount::Bind);
        assert_eq!(cfg.root, "/sessions");
        assert_eq!(cfg.bin, "/opt/dsh");
        assert_eq!(cfg.volume_prefix, "aria-x");
        assert_eq!(cfg.user, "2000:2000");
        assert_eq!(cfg.resources.cpus, 2.5);
        assert_eq!(cfg.resources.memory_bytes, 768 * 1024 * 1024);
        assert_eq!(cfg.resources.pids_limit, 300);
        assert!(!cfg.harden);
    }

    #[test]
    fn bad_overrides_fail_loudly() {
        for (key, raw, needle) in [
            ("DSH_SANDBOX_MOUNT", "nfs", "DSH_SANDBOX_MOUNT"),
            ("DSH_SANDBOX_ROOT", "relative/path", "must be absolute"),
            ("DSH_SANDBOX_IMAGE", "  ", "not be empty"),
            ("DSH_SANDBOX_BIN", "", "not be empty"),
            ("DSH_SANDBOX_CPUS", "0", "must be > 0"),
            ("DSH_SANDBOX_CPUS", "many", "DSH_SANDBOX_CPUS"),
            ("DSH_SANDBOX_MEMORY", "1zb", "DSH_SANDBOX_MEMORY"),
            ("DSH_SANDBOX_PIDS", "0", "must be > 0"),
            ("DSH_SANDBOX_PIDS", "x", "DSH_SANDBOX_PIDS"),
        ] {
            let err = SandboxedDshConfig::from_lookup(lookup(&[("DSH_SANDBOX", "1"), (key, raw)]))
                .unwrap_err();
            assert!(err.contains(needle), "{key}={raw}: {err}");
        }
    }

    #[test]
    fn a_blank_volume_prefix_is_refused() {
        let err = SandboxedDshConfig::from_lookup(lookup(&[("DSH_SANDBOX_VOLUME_PREFIX", " ")]))
            .unwrap_err();
        assert!(err.contains("VOLUME_PREFIX"), "{err}");
    }

    #[test]
    fn hardening_defaults_to_the_image_user() {
        let cfg = SandboxedDshConfig::default();
        let sandbox = cfg.to_dsh_sandbox(
            Arc::new(StubImage::present()),
            Some("aria-egress".into()),
            vec![("aria.tenant_id".into(), "tenant-a".into())],
        );
        assert_eq!(sandbox.hardening.cap_drop, vec!["ALL".to_string()]);
        assert!(sandbox.hardening.no_new_privileges);
        assert_eq!(sandbox.hardening.user.as_deref(), Some(DEFAULT_USER));
        assert_eq!(sandbox.network.as_deref(), Some("aria-egress"));
        assert_eq!(sandbox.labels.len(), 1);
    }

    #[test]
    fn hardening_can_be_turned_off_or_left_on_the_image_default() {
        let cfg = SandboxedDshConfig {
            harden: false,
            ..Default::default()
        };
        let sandbox = cfg.to_dsh_sandbox(Arc::new(StubImage::present()), None, Vec::new());
        assert!(sandbox.hardening.cap_drop.is_empty());
        assert!(sandbox.hardening.user.is_none());

        // Hardened but with no explicit user: the image's own user wins.
        let cfg = SandboxedDshConfig {
            user: String::new(),
            ..Default::default()
        };
        let sandbox = cfg.to_dsh_sandbox(Arc::new(StubImage::present()), None, Vec::new());
        assert!(sandbox.hardening.user.is_none());
        assert_eq!(sandbox.hardening.cap_drop, vec!["ALL".to_string()]);
    }

    #[tokio::test]
    async fn preflight_accepts_a_present_image() {
        let cfg = SandboxedDshConfig::default();
        preflight(&StubImage::present(), &cfg).await.unwrap();
    }

    #[tokio::test]
    async fn preflight_refuses_a_missing_image_with_the_build_command() {
        let cfg = SandboxedDshConfig::default();
        let err = preflight(&StubImage::missing(), &cfg).await.unwrap_err();
        assert!(err.contains("not present locally"), "{err}");
        assert!(err.contains("--target sandbox"), "{err}");
    }

    #[tokio::test]
    async fn preflight_refuses_a_backend_that_cannot_answer() {
        let cfg = SandboxedDshConfig::default();
        let err = preflight(&StubImage::opaque(), &cfg).await.unwrap_err();
        assert!(err.contains("cannot verify"), "{err}");
    }

    #[tokio::test]
    async fn preflight_reports_a_backend_error() {
        let cfg = SandboxedDshConfig::default();
        let err = preflight(&StubImage::broken(), &cfg).await.unwrap_err();
        assert!(err.contains("cannot inspect"), "{err}");
    }

    #[test]
    fn the_filter_is_optional_in_host_mode() {
        assert_eq!(
            thinking_filter_base(true, false, Some("http://127.0.0.1:9"), None).unwrap(),
            Some("http://127.0.0.1:9".to_string())
        );
        assert_eq!(thinking_filter_base(true, false, None, None).unwrap(), None);
        assert_eq!(
            thinking_filter_base(false, false, Some("http://x"), Some("http://y")).unwrap(),
            None
        );
    }

    #[test]
    fn host_mode_prefers_an_explicit_url() {
        assert_eq!(
            thinking_filter_base(
                true,
                false,
                Some("http://127.0.0.1:9"),
                Some("http://10.0.0.5:8787")
            )
            .unwrap(),
            Some("http://10.0.0.5:8787".to_string())
        );
    }

    #[test]
    fn sandboxed_mode_requires_a_reachable_url() {
        let err = thinking_filter_base(true, true, Some("http://127.0.0.1:9"), None).unwrap_err();
        assert!(err.contains("DSH_THINKING_FILTER_URL"), "{err}");
        for url in [
            "http://127.0.0.1:8787",
            "http://localhost:8787",
            "http://[::1]:8787",
            "not-a-url",
        ] {
            let err = thinking_filter_base(true, true, None, Some(url)).unwrap_err();
            assert!(
                err.contains("loopback") || err.contains("DSH_THINKING_FILTER_URL"),
                "{url}: {err}"
            );
        }
        assert_eq!(
            thinking_filter_base(true, true, None, Some("http://172.17.0.1:8787")).unwrap(),
            Some("http://172.17.0.1:8787".to_string())
        );
    }

    #[test]
    fn disabled_filter_needs_no_url_even_when_sandboxed() {
        assert_eq!(thinking_filter_base(false, true, None, None).unwrap(), None);
    }

    #[test]
    fn host_parsing_handles_ports_brackets_and_paths() {
        assert_eq!(host_of("http://172.17.0.1:8787/v1"), Some("172.17.0.1"));
        assert_eq!(host_of("https://gw.example"), Some("gw.example"));
        assert_eq!(host_of("http://user:pw@host:1/x"), Some("host"));
        assert_eq!(host_of("http://[::1]:80"), Some("[::1]"));
        assert_eq!(host_of(""), None);
        assert!(is_loopback("http://127.0.0.1:1"));
        assert!(is_loopback("http://[::1]:1"));
        assert!(!is_loopback("http://172.17.0.1:1"));
        // No host at all cannot be reached from a container either.
        assert!(is_loopback(""));
    }

    /// Put a variable back the way it was (`None` = it was not set).
    fn restore_var(key: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn from_env_reads_the_process_environment() {
        // `0` returns before any other key is read, so the assertion does not
        // depend on whatever else the runner exported.
        let saved = std::env::var("DSH_SANDBOX").ok();
        std::env::set_var("DSH_SANDBOX", "0");
        let resolved = SandboxedDshConfig::from_env().expect("the environment resolves");
        assert!(resolved.is_none(), "0 disables the sandbox");
        restore_var("DSH_SANDBOX", saved);
    }

    #[test]
    fn restoring_a_variable_handles_both_presence_and_absence() {
        std::env::set_var("ARIA_DSH_TEST_VAR", "original");
        restore_var("ARIA_DSH_TEST_VAR", Some("restored".into()));
        assert_eq!(std::env::var("ARIA_DSH_TEST_VAR").unwrap(), "restored");
        restore_var("ARIA_DSH_TEST_VAR", None);
        assert!(std::env::var("ARIA_DSH_TEST_VAR").is_err());
    }

    #[test]
    fn proxy_env_is_empty_without_a_proxy() {
        let cfg = SandboxedDshConfig::default();
        assert!(cfg
            .container_proxy_env("", Some("http://172.17.0.1:1"))
            .is_empty());
    }

    #[test]
    fn the_filter_host_is_exempted_from_the_proxy() {
        let cfg = SandboxedDshConfig::default();
        let env = cfg.container_proxy_env(
            "http://aria-egress-proxy:8080",
            Some("http://172.17.0.1:8787"),
        );
        let no_proxy = env
            .iter()
            .find(|(k, _)| k == "NO_PROXY")
            .map(|(_, v)| v.clone())
            .expect("NO_PROXY");
        assert!(no_proxy.contains("172.17.0.1"), "{no_proxy}");
        assert!(no_proxy.contains("localhost"), "{no_proxy}");
        // Appending twice would be noise; the helper is idempotent.
        let again = cfg.container_proxy_env(
            "http://aria-egress-proxy:8080",
            Some("http://172.17.0.1:8787"),
        );
        assert_eq!(env, again);
    }

    #[test]
    fn the_proxy_env_survives_a_url_without_a_host() {
        let cfg = SandboxedDshConfig::default();
        let env = cfg.container_proxy_env("http://p:8080", Some(""));
        assert!(env.iter().any(|(k, _)| k == "HTTP_PROXY"));
    }

    /// Minimal `Sandbox` that only answers `has_image`.
    struct StubImage {
        answer: Option<Result<bool, SandboxError>>,
    }

    impl StubImage {
        fn present() -> Self {
            Self {
                answer: Some(Ok(true)),
            }
        }
        fn missing() -> Self {
            Self {
                answer: Some(Ok(false)),
            }
        }
        fn opaque() -> Self {
            Self {
                answer: Some(Err(SandboxError::Unsupported("nope".into()))),
            }
        }
        fn broken() -> Self {
            Self {
                answer: Some(Err(SandboxError::Exec("daemon down".into()))),
            }
        }
    }

    #[async_trait]
    impl Sandbox for StubImage {
        async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
            Err(SandboxError::Unsupported("spawn".into()))
        }
        async fn exec(
            &self,
            _handle: &SandboxHandle,
            _cmd: &[String],
        ) -> Result<ExecOutput, SandboxError> {
            Err(SandboxError::Unsupported("exec".into()))
        }
        async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
            Err(SandboxError::Unsupported("destroy".into()))
        }
        async fn has_image(&self, _image: &str) -> Result<bool, SandboxError> {
            match &self.answer {
                Some(Ok(value)) => Ok(*value),
                Some(Err(SandboxError::Unsupported(message))) => {
                    Err(SandboxError::Unsupported(message.clone()))
                }
                Some(Err(_)) => Err(SandboxError::Exec("daemon down".into())),
                None => Err(SandboxError::Unsupported("unknown".into())),
            }
        }
    }

    /// The stub refuses everything except `has_image`; the required methods are
    /// exercised here so the failure shape is asserted, not just compiled.
    #[tokio::test]
    async fn the_image_stub_refuses_execution() {
        let stub = StubImage::present();
        let handle = SandboxHandle { id: "sbx".into() };
        assert!(stub
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .is_err());
        assert!(stub.exec(&handle, &["true".into()]).await.is_err());
        assert!(stub.destroy(handle).await.is_err());
        assert!(stub.has_image("x").await.unwrap());
    }
}
