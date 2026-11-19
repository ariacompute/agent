//! `aria-agent-tools` — the built-in tool families.
//!
//! Every family is routed by [`agent_core::ToolHandler`] *before* the shell
//! fallback, and every family talks to a capability seam rather than a concrete
//! backend (ADR-0014 / ADR-0016). A family whose seam is absent is **not
//! registered**: the model never sees a tool the deployment cannot serve, and a
//! handler that is registered but unusable returns `ERROR [unavailable]` instead
//! of a fake success.

pub mod ask;
pub mod fs;
pub mod jobs;
pub mod memo;
pub mod plan;
pub mod shell;
pub mod skills;
pub mod terminal;
pub mod todo;
pub mod web;

use agent_core::seams::{
    EgressSeam, FilesystemSkills, FsSeam, JobsSeam, LocalFs, LocalJobs, SandboxPolicy,
    SandboxShell, ShellSeam, SkillsSeam, UserQuestions, WebSeam,
};
use agent_core::{Tool, ToolCall, ToolHandler, ToolResult};
use serde_json::Value;
use std::sync::Arc;

pub use ask::AskToolHandler;
pub use fs::FsToolHandler;
pub use jobs::JobsToolHandler;
pub use memo::{memo_tools, MemoToolHandler};
pub use plan::PlanToolHandler;
pub use shell::ShellToolHandler;
pub use skills::SkillsToolHandler;
pub use terminal::TerminalToolHandler;
pub use todo::TodoToolHandler;
#[cfg(feature = "web")]
pub use web::HttpWeb;
pub use web::WebToolHandler;

/// Every seam a tool family may consume. Missing entries simply disable the
/// corresponding family.
#[derive(Default, Clone)]
pub struct ToolSet {
    pub fs: Option<Arc<dyn FsSeam>>,
    pub shell: Option<Arc<dyn ShellSeam>>,
    pub jobs: Option<Arc<dyn JobsSeam>>,
    pub skills: Option<Arc<dyn SkillsSeam>>,
    pub web: Option<Arc<dyn WebSeam>>,
    pub questions: Option<Arc<dyn UserQuestions>>,
    /// Store used by `plan_*`, `todo_*` and `memo_*`.
    pub store: Option<Arc<dyn agent_core::context::ContextStore>>,
    /// Whether the `term_*` family is registered.
    pub terminal: bool,
    /// Outbound policy. `None` keeps the old behaviour for embedded callers;
    /// deployments must set it (deny-by-default is the deployment default).
    pub egress: Option<Arc<dyn EgressSeam>>,
    /// Sandbox handle used to rebuild the shell when network enforcement is on.
    pub sandbox: Option<Arc<dyn agent_sandbox::Sandbox>>,
    /// Policy root, kept so the shell can be rebuilt with extra transport fields.
    pub root: std::path::PathBuf,
    /// Governed network (ADR-0024). `None` = default bridge.
    pub sandbox_network: Option<String>,
    /// Labels identifying this sandbox to the egress proxy.
    pub sandbox_labels: Vec<(String, String)>,
    /// Proxy environment handed to every command.
    pub sandbox_env: Vec<(String, String)>,
}

impl ToolSet {
    /// Local development set: filesystem + shell + jobs + skills rooted at
    /// `policy.root()`, plan/todo/memo backed by `store`.
    pub fn local(
        policy: Arc<SandboxPolicy>,
        sandbox: Arc<dyn agent_sandbox::Sandbox>,
        store: Arc<dyn agent_core::context::ContextStore>,
    ) -> Self {
        let root = policy.root().to_path_buf();
        let shell: Arc<dyn ShellSeam> =
            Arc::new(SandboxShell::new(sandbox.clone()).with_cwd(root.to_string_lossy()));
        Self {
            fs: Some(Arc::new(LocalFs::new(policy))),
            jobs: Some(Arc::new(LocalJobs::new(shell.clone()))),
            shell: Some(shell),
            skills: Some(Arc::new(FilesystemSkills::new([root.join(".aria/skills")]))),
            sandbox: Some(sandbox),
            root,
            web: None,
            questions: None,
            store: Some(store),
            terminal: cfg!(feature = "pty"),
            egress: None,
            sandbox_network: None,
            sandbox_labels: Vec::new(),
            sandbox_env: Vec::new(),
        }
    }

    /// Put every shell session on the governed network, carry its identity
    /// labels, and hand it the proxy environment.
    ///
    /// Rebuilds the shell so the transport fields apply to `jobs` as well: a
    /// background job must not be a way out of the network.
    pub fn with_sandbox_transport(
        mut self,
        network: Option<String>,
        labels: Vec<(String, String)>,
        env: Vec<(String, String)>,
    ) -> Self {
        self.sandbox_network = network;
        self.sandbox_labels = labels;
        self.sandbox_env = env;
        // `term_*` drives a PTY on the *host process*, not in the sandbox, so the
        // proxy cannot cover it. Under network enforcement the family is not
        // registered at all: a host terminal would be an ungoverned way out
        // (ADR-0024). The shell/jobs families are sandbox-backed and stay.
        if self.sandbox_network.is_some() && self.terminal {
            let reason = "term_* tools disabled: network enforcement cannot proxy host PTYs";
            tracing::warn!("{reason}");
            self.terminal = false;
        }
        let shell: Option<Arc<dyn ShellSeam>> = self
            .sandbox
            .clone()
            .map(|sandbox| self.governed_shell(sandbox));
        self.jobs = shell
            .clone()
            .map(|shell| Arc::new(LocalJobs::new(shell)) as Arc<dyn JobsSeam>);
        self.shell = shell;
        self
    }

    /// The shell seam every sandbox-backed family shares: same backend, same
    /// working directory, same governed network and identity labels.
    fn governed_shell(&self, sandbox: Arc<dyn agent_sandbox::Sandbox>) -> Arc<dyn ShellSeam> {
        let mut shell = SandboxShell::new(sandbox).with_cwd(self.root.to_string_lossy());
        if !self.sandbox_env.is_empty() {
            shell = shell.with_env(self.sandbox_env.clone());
        }
        Arc::new(
            shell
                .with_network(self.sandbox_network.clone())
                .with_labels(self.sandbox_labels.clone()),
        )
    }

    /// Attach the outbound policy. `web_*` decisions then name the policy
    /// revision, and a denial reaches the model as an actionable error.
    pub fn with_egress(mut self, egress: Arc<dyn EgressSeam>) -> Self {
        self.egress = Some(egress);
        self
    }

    /// Enable the `web_*` family with the `reqwest` provider (`--features web`).
    ///
    /// Search stays `Unavailable` unless `ARIA_WEB_SEARCH_URL` is set, so a
    /// deployment enables the fetch half without opting into search.
    #[cfg(feature = "web")]
    pub fn with_http_web(mut self) -> Result<Self, agent_core::seams::SeamError> {
        self.web = Some(Arc::new(web::HttpWeb::from_env()?));
        Ok(self)
    }

    /// Tool schemas for the enabled families (same order as [`ToolSet::handlers`]).
    pub fn tools(&self) -> Vec<Tool> {
        let mut out = Vec::new();
        if self.fs.is_some() {
            out.extend(fs::fs_tools());
        }
        if self.shell.is_some() {
            out.extend(shell::shell_tools());
        }
        if self.jobs.is_some() {
            out.extend(jobs::jobs_tools());
        }
        if self.terminal {
            out.extend(terminal::terminal_tools());
        }
        if self.skills.is_some() {
            out.extend(skills::skills_tools());
        }
        if self.web.is_some() {
            out.extend(web::web_tools());
        }
        if self.questions.is_some() {
            out.extend(ask::ask_tools());
        }
        if self.store.is_some() {
            out.extend(plan::plan_tools());
            out.extend(todo::todo_tools());
            out.extend(memo::memo_tools());
        }
        out
    }

    /// Handlers for the enabled families.
    pub fn handlers(&self) -> Vec<Arc<dyn ToolHandler>> {
        let mut out: Vec<Arc<dyn ToolHandler>> = Vec::new();
        if let Some(fs) = &self.fs {
            out.push(Arc::new(FsToolHandler::new(fs.clone())));
        }
        if let Some(shell) = &self.shell {
            out.push(Arc::new(ShellToolHandler::new(shell.clone())));
        }
        if let Some(jobs) = &self.jobs {
            out.push(Arc::new(JobsToolHandler::new(jobs.clone())));
        }
        if self.terminal {
            out.push(Arc::new(TerminalToolHandler::new(self.shell.clone())));
        }
        if let Some(skills) = &self.skills {
            out.push(Arc::new(SkillsToolHandler::new(skills.clone())));
        }
        if let Some(web) = &self.web {
            let mut handler = WebToolHandler::new(web.clone());
            if let Some(egress) = &self.egress {
                handler = handler.with_egress(egress.clone());
            }
            out.push(Arc::new(handler));
        }
        if let Some(questions) = &self.questions {
            out.push(Arc::new(AskToolHandler::new(questions.clone())));
        }
        if let Some(store) = &self.store {
            out.push(Arc::new(PlanToolHandler::new(store.clone())));
            out.push(Arc::new(TodoToolHandler::new(store.clone())));
            out.push(Arc::new(MemoToolHandler::new(store.clone())));
        }
        out
    }
}

/// Convenience: schemas + handlers in one call.
pub fn builtin(set: &ToolSet) -> (Vec<Tool>, Vec<Arc<dyn ToolHandler>>) {
    (set.tools(), set.handlers())
}

// ---------------------------------------------------------------- arg helpers

/// Read a required string argument.
pub(crate) fn arg_str(call: &ToolCall, key: &str) -> Result<String, agent_core::seams::SeamError> {
    call.arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            agent_core::seams::SeamError::Invalid(format!(
                "tool `{}` requires string argument `{key}`",
                call.name
            ))
        })
}

/// Read an optional string argument.
pub(crate) fn arg_str_opt(call: &ToolCall, key: &str) -> Option<String> {
    call.arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Read an optional unsigned argument.
pub(crate) fn arg_usize(call: &ToolCall, key: &str) -> Option<usize> {
    call.arguments
        .get(key)
        .and_then(Value::as_u64)
        .map(|n| n as usize)
}

/// A successful tool result.
pub(crate) fn ok(call: &ToolCall, content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        content: content.into(),
        is_error: false,
    }
}

/// A failed tool result carrying the seam error code so the model can react to
/// the *class* of failure (`unavailable` vs `denied` vs `invalid`).
pub(crate) fn fail(call: &ToolCall, err: agent_core::seams::SeamError) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        content: format!("ERROR [{}]: {err}", err.code()),
        is_error: true,
    }
}

/// Persist a tool result as a context fragment so later turns can recall it.
pub(crate) async fn remember(
    ctx: &Arc<dyn agent_core::context::ContextStore>,
    session: &str,
    content: &str,
) {
    use agent_core::context::{ContextFragment, FragmentKind};
    let _ = ctx
        .memorize(ContextFragment::new(
            session,
            FragmentKind::ToolResult,
            content,
        ))
        .await;
}

/// Shared dispatch skeleton: parse → call seam → map error.
pub(crate) trait Family {
    fn prefix(&self) -> &'static str;
    async fn dispatch(
        &self,
        call: &ToolCall,
        session: &str,
    ) -> Result<String, agent_core::seams::SeamError>;
}

/// Implement [`ToolHandler`] for a [`Family`] in one line per family.
#[macro_export]
macro_rules! family_handler {
    ($ty:ty) => {
        #[async_trait::async_trait]
        impl agent_core::ToolHandler for $ty {
            fn handles(&self, name: &str) -> bool {
                name.starts_with(self.prefix())
            }

            async fn run(
                &self,
                call: &agent_core::ToolCall,
                ctx: &std::sync::Arc<dyn agent_core::context::ContextStore>,
                session: &str,
            ) -> agent_core::ToolResult {
                let result = match self.dispatch(call, session).await {
                    Ok(content) => $crate::ok(call, content),
                    Err(e) => $crate::fail(call, e),
                };
                $crate::remember(ctx, session, &result.content).await;
                result
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::{NoQuestions, NoWeb};
    use agent_sandbox::{ExecSpec, SandboxError, SandboxHandle};

    struct NoopSandbox;

    #[async_trait::async_trait]
    impl agent_sandbox::Sandbox for NoopSandbox {
        async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
            Err(SandboxError::NotConfigured("noop".into()))
        }
        async fn exec(
            &self,
            _handle: &SandboxHandle,
            _cmd: &[String],
        ) -> Result<agent_sandbox::ExecOutput, SandboxError> {
            Err(SandboxError::NotConfigured("noop".into()))
        }
        async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn noop_sandbox_is_inert() {
        use agent_sandbox::Sandbox;
        let s = NoopSandbox;
        let spec = ExecSpec::command(vec!["echo".into()]);
        assert!(s.spawn(&spec).await.is_err());
        let handle = SandboxHandle { id: "noop".into() };
        assert!(s.exec(&handle, &["echo".into()]).await.is_err());
        assert!(s.destroy(handle).await.is_ok());
    }

    #[test]
    fn empty_set_registers_nothing() {
        let set = ToolSet::default();
        assert!(set.tools().is_empty());
        assert!(set.handlers().is_empty());
    }

    #[test]
    fn families_are_registered_only_when_their_seam_exists() {
        let set = ToolSet {
            web: Some(Arc::new(NoWeb)),
            ..ToolSet::default()
        };
        let tools = set.tools();
        assert!(tools.iter().any(|t| t.name == "web_fetch"));
        assert!(!tools.iter().any(|t| t.name == "fs_read"));
        assert_eq!(set.handlers().len(), 1);
    }

    #[cfg(feature = "web")]
    #[test]
    fn http_web_registers_the_web_family() {
        let set = ToolSet::default().with_http_web().unwrap();
        assert!(set.tools().iter().any(|t| t.name == "web_fetch"));
        assert_eq!(set.handlers().len(), 1);
    }

    #[test]
    fn tool_names_are_unique() {
        let set = ToolSet {
            web: Some(Arc::new(NoWeb)),
            terminal: false,
            ..ToolSet::default()
        };
        let names: Vec<String> = set.tools().into_iter().map(|t| t.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names.len(), sorted.len(), "duplicate tool names: {names:?}");
    }

    #[test]
    fn every_family_is_registered_when_every_seam_exists() {
        let set = ToolSet::local(
            Arc::new(agent_core::seams::SandboxPolicy::default()),
            Arc::new(NoopSandbox),
            agent_core::in_memory_context(),
        );
        let mut set = set;
        set.web = Some(Arc::new(NoWeb));
        set.questions = Some(Arc::new(NoQuestions));
        set.terminal = true;
        let tools = set.tools();
        for name in [
            "fs_read",
            "shell",
            "job_list",
            "term_open",
            "skill_list",
            "web_fetch",
            "ask_user",
            "plan_set",
            "todo_write",
            "memo_store",
        ] {
            assert!(tools.iter().any(|t| t.name == name), "missing {name}");
        }
        assert_eq!(set.handlers().len(), 10);
        let (built_tools, built_handlers) = builtin(&set);
        assert_eq!(built_tools.len(), tools.len());
        assert_eq!(built_handlers.len(), 10);
    }

    #[test]
    fn web_and_egress_are_wired_together() {
        let mut set = ToolSet::local(
            Arc::new(agent_core::seams::SandboxPolicy::default()),
            Arc::new(NoopSandbox),
            agent_core::in_memory_context(),
        );
        set.web = Some(Arc::new(NoWeb));
        let set = set.with_egress(Arc::new(agent_core::seams::DenyAllEgress));
        let handlers = set.handlers();
        assert!(handlers.iter().any(|h| h.handles("web_fetch")));
    }

    #[test]
    fn network_enforcement_drops_the_host_terminal_family() {
        let build = || {
            let mut set = ToolSet::local(
                Arc::new(agent_core::seams::SandboxPolicy::default()),
                Arc::new(NoopSandbox),
                agent_core::in_memory_context(),
            );
            // The `pty` feature may be off; force the flag so the assertion is
            // about the enforcement rule, not the build profile.
            set.terminal = true;
            set
        };
        let governed = build().with_sandbox_transport(
            Some("aria-egress".into()),
            vec![("aria.tenant_id".into(), "tenant".into())],
            vec![("HTTP_PROXY".into(), "http://aria-egress-proxy:8080".into())],
        );
        assert!(!governed.terminal, "host PTYs cannot be proxied");
        let names: Vec<String> = governed.tools().into_iter().map(|t| t.name).collect();
        assert!(!names.iter().any(|n| n.starts_with("term_")), "{names:?}");
        // Sandbox-backed families stay: they *are* on the governed network.
        assert!(names.iter().any(|n| n == "shell"));
        assert!(names.iter().any(|n| n.starts_with("job_")));

        let proxy_mode = build().with_sandbox_transport(None, Vec::new(), Vec::new());
        assert!(proxy_mode.terminal, "proxy mode keeps the terminal");
    }

    #[tokio::test]
    async fn remember_persists_a_tool_result_fragment() {
        use agent_core::context::{FragmentKind, RecallQuery};
        let store = agent_core::in_memory_context();
        remember(&store, "s", "captured output").await;
        let frags = store
            .recall(&RecallQuery::new("s", "captured output"))
            .await
            .unwrap();
        assert!(frags.iter().any(|f| f.kind == FragmentKind::ToolResult));
    }
}
