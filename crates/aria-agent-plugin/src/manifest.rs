//! Seam keys and plugin manifests.
//!
//! dsh models every capability as one of four roles (ADR-0014, dsh
//! `docs/capability-seams.md`):
//!
//! * [`SeamRole::Core`] — a single authoritative owner (state + lifecycle);
//! * [`SeamRole::Seam`] — a contract with a swappable provider;
//! * [`SeamRole::Bundle`] — a composition point that consumes other seams and
//!   promises no interface of its own;
//! * [`SeamRole::Service`] — a service a plugin declares and owns (telemetry,
//!   accounts, plugin manager); it is **not** a swappable seam.
//!
//! Keys are `&'static str` newtypes so a manifest typo is a compile error at the
//! definition site and a [`PluginError::UnknownSeam`](crate::PluginError) at
//! deserialize time, never a silent miss at run time.
//!
//! The canonical key names follow this crate's historical spellings; dsh's own
//! spelling is accepted through [`ALIASES`] so a manifest copied from a dsh
//! profile resolves to the same seam instead of failing as unknown.

use crate::error::{PluginError, PluginResult};
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

/// A capability seam identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeamKey(pub &'static str);

impl SeamKey {
    /// The role this seam plays in the architecture.
    ///
    /// `agent_loop` is a **seam** here even though dsh classifies `ctx.agentLoop`
    /// as its only bundle: we assemble statically, so "replaceable runtime" only
    /// works if the loop is a contract (ADR-0019 §deviations).
    pub fn role(self) -> SeamRole {
        match self.0 {
            "sandbox_policy" | "session" | "system_prompt" | "working_directory" | "agents"
            | "teams" | "schedule" | "tools" | "token_meter" | "plugin_manager" => SeamRole::Core,
            "otel" | "product_telemetry" | "deepseek_auth" => SeamRole::Service,
            _ => SeamRole::Seam,
        }
    }

    pub fn is_core(self) -> bool {
        self.role() == SeamRole::Core
    }

    pub fn is_seam(self) -> bool {
        self.role() == SeamRole::Seam
    }

    pub fn is_service(self) -> bool {
        self.role() == SeamRole::Service
    }

    pub fn as_str(self) -> &'static str {
        self.0
    }

    /// Intern a `&str` into one of the declared keys (canonical or alias).
    pub fn parse(s: &str) -> Option<Self> {
        ALL.iter().copied().find(|k| k.0 == s).or_else(|| {
            ALIASES
                .iter()
                .find(|(alias, _)| *alias == s)
                .map(|(_, k)| *k)
        })
    }
}

impl std::fmt::Display for SeamKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl Serialize for SeamKey {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for SeamKey {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(de)?;
        SeamKey::parse(&raw).ok_or_else(|| de::Error::custom(format!("unknown seam: {raw}")))
    }
}

/// Model / streaming adapter (`ctx.llm`).
pub const LLM: SeamKey = SeamKey("llm");
/// Process isolation backend (`ctx.sandbox`).
pub const SANDBOX: SeamKey = SeamKey("sandbox");
/// Deployment-wide sandbox mode + single root directory (`ctx.sandboxPolicy`).
pub const SANDBOX_POLICY: SeamKey = SeamKey("sandbox_policy");
/// Filesystem primitives (`ctx.fs`).
pub const FS: SeamKey = SeamKey("fs");
/// Shell execution (`ctx.shell`).
pub const SHELL: SeamKey = SeamKey("shell");
/// Background jobs (`ctx.jobs`).
pub const JOBS: SeamKey = SeamKey("jobs");
/// Interactive terminals (`ctx.terminal`).
pub const TERMINAL: SeamKey = SeamKey("terminal");
/// Key/value + typed domain storage (`ctx.storage`).
pub const STORAGE: SeamKey = SeamKey("storage");
/// Append-only session event stream (`ctx.sessions`).
pub const SESSION: SeamKey = SeamKey("session");
/// Session event persistence (`ctx.sessionPersistence`).
pub const SESSION_PERSISTENCE: SeamKey = SeamKey("session_persistence");
/// Session query / filtering (`ctx.sessionQuery`).
pub const SESSION_QUERY: SeamKey = SeamKey("session_query");
/// Session title provider (`ctx.sessionTitle`).
pub const SESSION_TITLE: SeamKey = SeamKey("session_title");
/// Context compaction strategy (`ctx.compaction`).
pub const COMPACTION: SeamKey = SeamKey("compaction");
/// System prompt assembly (`ctx.systemPrompt`).
pub const SYSTEM_PROMPT: SeamKey = SeamKey("system_prompt");
/// One-shot approval decisions (`ctx.approvals`).
pub const APPROVALS: SeamKey = SeamKey("approvals");
/// Human question answering (`ctx.userQuestions`).
pub const USER_QUESTIONS: SeamKey = SeamKey("user_questions");
/// Skill catalog (`ctx.skills`).
pub const SKILLS: SeamKey = SeamKey("skills");
/// Web search / fetch (`ctx.web`).
pub const WEB: SeamKey = SeamKey("web");
/// Browser automation (`ctx.browserUse`).
pub const BROWSER_USE: SeamKey = SeamKey("browser_use");
/// Sub-agent spawning (`ctx.subagents`).
pub const SUBAGENTS: SeamKey = SeamKey("subagents");
/// Live agent handles (`ctx.agents`).
pub const AGENTS: SeamKey = SeamKey("agents");
/// Shared task DAG (`ctx.agentTeams`).
pub const TEAMS: SeamKey = SeamKey("teams");
/// Scheduled jobs (`ctx.schedule`).
pub const SCHEDULE: SeamKey = SeamKey("schedule");
/// The effective execution directory (`ctx.workingDirectory`).
pub const WORKING_DIRECTORY: SeamKey = SeamKey("working_directory");
/// The concrete agent loop (`ctx.agentLoop`).
pub const AGENT_LOOP: SeamKey = SeamKey("agent_loop");
/// Model-facing tool registry (`ctx.tools`).
pub const TOOLS: SeamKey = SeamKey("tools");
/// Token accounting (`ctx.tokenMeter`).
pub const TOKEN_METER: SeamKey = SeamKey("token_meter");
/// Plugin install/enable state (`ctx.pluginManager`).
pub const PLUGIN_MANAGER: SeamKey = SeamKey("plugin_manager");
/// Connection credential verifiers (`ctx.connection.authentication`).
pub const CONNECTION_AUTHENTICATION: SeamKey = SeamKey("connection_authentication");
/// Remote / change-request providers (`ctx.forge`).
pub const FORGE: SeamKey = SeamKey("forge");
/// Explicit worktree creation (`ctx.worktrees`).
pub const WORKTREES: SeamKey = SeamKey("worktrees");
/// Shared resources supplied by an MCP connection (`ctx.mcpResources`).
pub const MCP_RESOURCES: SeamKey = SeamKey("mcp_resources");
/// Desktop / computer automation (`ctx.computerUse`).
pub const COMPUTER_USE: SeamKey = SeamKey("computer_use");
/// Accepted images referenced by a session (`ctx.attachments`).
pub const ATTACHMENTS: SeamKey = SeamKey("attachments");
/// Provider-specific top-level LLM wire fields (`ctx.deepseekLlmApiExtensions`).
pub const DEEPSEEK_LLM_API_EXTENSIONS: SeamKey = SeamKey("deepseek_llm_api_extensions");
/// Credential references and resolved values (`ctx.credentials`).
pub const CREDENTIALS: SeamKey = SeamKey("credentials");
/// Interactive authorization flows (`ctx.authorization`).
pub const AUTHORIZATION: SeamKey = SeamKey("authorization");
/// Session recording sink (`ctx.sessionTelemetry`).
pub const SESSION_TELEMETRY: SeamKey = SeamKey("session_telemetry");
/// Path-only completion candidates (`ctx.fileReferences`).
pub const FILE_REFERENCES: SeamKey = SeamKey("file_references");
/// Process coordinates and lifecycle (`ctx.subprocess`).
pub const SUBPROCESS: SeamKey = SeamKey("subprocess");
/// Programmatic tool-call runtime (`ctx.ptcRuntime`).
pub const PTC_RUNTIME: SeamKey = SeamKey("ptc_runtime");
/// Speech recognition routing (`ctx.speechToText`).
pub const SPEECH_TO_TEXT: SeamKey = SeamKey("speech_to_text");
/// Oversized tool output parking (`ctx.spillStore`).
pub const SPILL_STORE: SeamKey = SeamKey("spill_store");
/// Interactive directory selection (`ctx.directoryPicker`).
pub const DIRECTORY_PICKER: SeamKey = SeamKey("directory_picker");
/// Workflow engine (one per context) (`ctx.workflowEngine`).
pub const WORKFLOW_ENGINE: SeamKey = SeamKey("workflow_engine");
/// Language-server queries (`ctx.lsp`).
pub const LSP: SeamKey = SeamKey("lsp");
/// Outbound network policy (`ctx.egress`). Deny-by-default (ADR-0020).
pub const EGRESS: SeamKey = SeamKey("egress");
/// Audit sink for actions that actually happened (`ctx.audit`, ADR-0021).
pub const AUDIT: SeamKey = SeamKey("audit");
/// Resource usage accounting by lifecycle state (`ctx.metering`, ADR-0022).
pub const METERING: SeamKey = SeamKey("metering");

/// OpenTelemetry export (`ctx.otel`) — a **service**, not a swappable seam.
pub const OTEL: SeamKey = SeamKey("otel");
/// Product analytics (`ctx.productTelemetry`) — a service.
pub const PRODUCT_TELEMETRY: SeamKey = SeamKey("product_telemetry");
/// Upstream DeepSeek account auth (`ctx.deepseekAuth`) — a service.
pub const DEEPSEEK_AUTH: SeamKey = SeamKey("deepseek_auth");

/// Every declared seam key (the interning table).
pub const ALL: &[SeamKey] = &[
    LLM,
    SANDBOX,
    SANDBOX_POLICY,
    FS,
    SHELL,
    JOBS,
    TERMINAL,
    STORAGE,
    SESSION,
    SESSION_PERSISTENCE,
    SESSION_QUERY,
    SESSION_TITLE,
    COMPACTION,
    SYSTEM_PROMPT,
    APPROVALS,
    USER_QUESTIONS,
    SKILLS,
    WEB,
    BROWSER_USE,
    SUBAGENTS,
    AGENTS,
    TEAMS,
    SCHEDULE,
    WORKING_DIRECTORY,
    AGENT_LOOP,
    TOOLS,
    TOKEN_METER,
    PLUGIN_MANAGER,
    CONNECTION_AUTHENTICATION,
    FORGE,
    WORKTREES,
    MCP_RESOURCES,
    COMPUTER_USE,
    ATTACHMENTS,
    DEEPSEEK_LLM_API_EXTENSIONS,
    CREDENTIALS,
    AUTHORIZATION,
    SESSION_TELEMETRY,
    FILE_REFERENCES,
    SUBPROCESS,
    PTC_RUNTIME,
    SPEECH_TO_TEXT,
    SPILL_STORE,
    DIRECTORY_PICKER,
    WORKFLOW_ENGINE,
    LSP,
    EGRESS,
    AUDIT,
    METERING,
    OTEL,
    PRODUCT_TELEMETRY,
    DEEPSEEK_AUTH,
];

/// dsh spellings accepted by [`SeamKey::parse`] in addition to [`ALL`].
///
/// Renaming a key would break every existing manifest, so dsh's spelling is an
/// inbound alias instead. Aliases are deliberately **not** in [`ALL`]: the
/// interning table stays canonical.
pub const ALIASES: &[(&str, SeamKey)] = &[
    ("approval", APPROVALS),
    ("terminals", TERMINAL),
    ("sandboxPolicy", SANDBOX_POLICY),
    ("workingDirectory", WORKING_DIRECTORY),
    ("systemPrompt", SYSTEM_PROMPT),
    ("agentLoop", AGENT_LOOP),
    ("agentTeams", TEAMS),
    ("sessionPersistence", SESSION_PERSISTENCE),
    ("sessionQuery", SESSION_QUERY),
    ("sessionTitle", SESSION_TITLE),
    ("sessionTelemetry", SESSION_TELEMETRY),
    ("userQuestions", USER_QUESTIONS),
    ("browserUse", BROWSER_USE),
    ("computerUse", COMPUTER_USE),
    ("mcpResources", MCP_RESOURCES),
    ("deepseekLlmApiExtensions", DEEPSEEK_LLM_API_EXTENSIONS),
    ("fileReferences", FILE_REFERENCES),
    ("ptcRuntime", PTC_RUNTIME),
    ("speechToText", SPEECH_TO_TEXT),
    ("spillStore", SPILL_STORE),
    ("directoryPicker", DIRECTORY_PICKER),
    ("workflowEngine", WORKFLOW_ENGINE),
    ("connection.authentication", CONNECTION_AUTHENTICATION),
    ("productTelemetry", PRODUCT_TELEMETRY),
    ("deepseekAuth", DEEPSEEK_AUTH),
    ("tokenMeter", TOKEN_METER),
    ("pluginManager", PLUGIN_MANAGER),
];

/// The four roles a plugin can play.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeamRole {
    /// Single authoritative implementation; never replaced at run time.
    Core,
    /// Contract with swappable providers.
    Seam,
    /// Composition point that only wires other seams together.
    Bundle,
    /// A service a plugin declares and owns; **not** a swappable provider slot.
    Service,
}

/// Declarative plugin metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub role: SeamRole,
    /// Seams this plugin installs a provider for.
    #[serde(default)]
    pub provides: Vec<SeamKey>,
    /// Seams this plugin needs at install time.
    #[serde(default)]
    pub requires: Vec<SeamKey>,
}

impl PluginManifest {
    pub fn new(id: impl Into<String>, role: SeamRole) -> Self {
        Self {
            id: id.into(),
            role,
            provides: Vec::new(),
            requires: Vec::new(),
        }
    }

    pub fn provides(mut self, keys: impl IntoIterator<Item = SeamKey>) -> Self {
        self.provides.extend(keys);
        self
    }

    pub fn requires(mut self, keys: impl IntoIterator<Item = SeamKey>) -> Self {
        self.requires.extend(keys);
        self
    }

    /// Parse a `plugin.json` manifest.
    pub fn from_json(raw: &str) -> PluginResult<Self> {
        let m: Self =
            serde_json::from_str(raw).map_err(|e| PluginError::Manifest(e.to_string()))?;
        m.validate()?;
        Ok(m)
    }

    pub fn to_json(&self) -> PluginResult<String> {
        serde_json::to_string(self).map_err(|e| PluginError::Manifest(e.to_string()))
    }

    /// Reject malformed manifests before they reach the registry.
    pub fn validate(&self) -> PluginResult<()> {
        if self.id.trim().is_empty() {
            return Err(PluginError::EmptyPluginId);
        }
        let mut seen = std::collections::HashSet::new();
        for key in &self.provides {
            if !seen.insert(*key) {
                return Err(PluginError::DuplicateManifestSeam {
                    id: self.id.clone(),
                    seam: key.0,
                });
            }
        }
        let mut seen = std::collections::HashSet::new();
        for key in &self.requires {
            if !seen.insert(*key) {
                return Err(PluginError::DuplicateManifestSeam {
                    id: self.id.clone(),
                    seam: key.0,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_are_assigned_by_key() {
        assert_eq!(SESSION.role(), SeamRole::Core);
        assert_eq!(SANDBOX_POLICY.role(), SeamRole::Core);
        assert_eq!(LLM.role(), SeamRole::Seam);
        // Deliberate deviation from dsh (which calls it a bundle): we must be
        // able to swap the loop implementation at assembly time.
        assert_eq!(AGENT_LOOP.role(), SeamRole::Seam);
        assert_eq!(OTEL.role(), SeamRole::Service);
        assert!(SESSION.is_core());
        assert!(!FS.is_core());
        assert!(LLM.is_seam());
        assert!(!LLM.is_service());
        assert!(OTEL.is_service());
        assert!(!OTEL.is_seam());
    }

    #[test]
    fn service_role_roundtrips_through_json() {
        let m = PluginManifest::from_json(r#"{"id":"telemetry","role":"service"}"#).unwrap();
        assert_eq!(m.role, SeamRole::Service);
        assert!(m.to_json().unwrap().contains("\"service\""));
    }

    #[test]
    fn dsh_spellings_are_accepted_as_aliases() {
        assert_eq!(SeamKey::parse("approval"), Some(APPROVALS));
        assert_eq!(SeamKey::parse("terminals"), Some(TERMINAL));
        assert_eq!(SeamKey::parse("agentLoop"), Some(AGENT_LOOP));
        assert_eq!(
            SeamKey::parse("connection.authentication"),
            Some(CONNECTION_AUTHENTICATION)
        );
        assert_eq!(SeamKey::parse("spillStore"), Some(SPILL_STORE));
        // Canonical spellings still win and aliases never enter the table.
        assert_eq!(SeamKey::parse("approvals"), Some(APPROVALS));
        assert!(!ALL.iter().any(|k| k.0 == "approval"));
    }

    #[test]
    fn every_key_is_interned_by_parse() {
        for key in ALL {
            assert_eq!(SeamKey::parse(key.0), Some(*key));
        }
        assert_eq!(SeamKey::parse("nope"), None);
        assert_eq!(SeamKey::parse(""), None);
    }

    #[test]
    fn manifest_json_roundtrip() {
        let raw = r#"{"id":"aria-memo","role":"seam","provides":["storage"],"requires":["sandbox_policy"]}"#;
        let m = PluginManifest::from_json(raw).unwrap();
        assert_eq!(m.id, "aria-memo");
        assert_eq!(m.role, SeamRole::Seam);
        assert_eq!(m.provides, vec![STORAGE]);
        assert_eq!(m.requires, vec![SANDBOX_POLICY]);
        let again = PluginManifest::from_json(&m.to_json().unwrap()).unwrap();
        assert_eq!(again, m);
    }

    #[test]
    fn unknown_seam_in_json_is_rejected() {
        let raw = r#"{"id":"x","role":"seam","provides":["nope"]}"#;
        let err = PluginManifest::from_json(raw).unwrap_err();
        assert!(matches!(err, PluginError::Manifest(_)), "{err:?}");
    }

    #[test]
    fn empty_id_is_rejected() {
        let m = PluginManifest::new("  ", SeamRole::Seam);
        assert_eq!(m.validate().unwrap_err(), PluginError::EmptyPluginId);
    }

    #[test]
    fn seam_key_as_str_and_display_agree() {
        assert_eq!(FS.as_str(), "fs");
        assert_eq!(format!("{FS}"), "fs");
    }

    #[test]
    fn duplicate_provides_is_rejected() {
        let m = PluginManifest::new("x", SeamRole::Seam).provides([FS, FS]);
        assert!(matches!(
            m.validate().unwrap_err(),
            PluginError::DuplicateManifestSeam { .. }
        ));
    }

    #[test]
    fn duplicate_requires_is_rejected() {
        let m = PluginManifest::new("x", SeamRole::Seam).requires([WEB, WEB]);
        assert!(matches!(
            m.validate().unwrap_err(),
            PluginError::DuplicateManifestSeam { .. }
        ));
    }
}
