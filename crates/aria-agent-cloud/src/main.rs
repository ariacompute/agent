//! `aria-agent` binary — CLI parsing, process startup and the codex sandbox
//! backend. All request handling, storage and upgrade logic lives in the
//! library (`aria_agent_cloud`).
//!
//! This file is **excluded from the coverage gate**: it is the process entry
//! point (`main` / `run` / `cmd_*`), signal/stdio interaction and the
//! external-process sandbox seam, none of which can be exercised in-process.

mod codex_sandbox {
    use agent_sandbox::{ExecOutput, ExecSpec, Sandbox, SandboxError, SandboxHandle};
    use async_trait::async_trait;
    use codex_protocol::config_types::WindowsSandboxLevel;
    use codex_protocol::models::PermissionProfile;
    use codex_sandboxing::spawn_process;
    use codex_sandboxing::SandboxCommand;
    use codex_sandboxing::SandboxManager;
    use codex_sandboxing::SandboxTransformRequest;
    use codex_sandboxing::SandboxType;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;
    use codex_utils_pty::ChildFds;
    use std::collections::HashMap;
    use std::convert::TryFrom;
    use std::ffi::OsString;

    /// Deep codex integration (ADR-0005 §Decision): the codex-backed sandbox
    /// backend. Lives in this `publish = false` runtime on purpose — it depends
    /// on codex's git-only crates, which crates.io cannot resolve.
    pub struct CodexSandbox;

    impl CodexSandbox {
        pub fn new() -> Self {
            Self
        }
    }

    impl Default for CodexSandbox {
        fn default() -> Self {
            Self::new()
        }
    }

    #[async_trait]
    impl Sandbox for CodexSandbox {
        async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
            Ok(SandboxHandle { id: "codex".into() })
        }

        async fn exec(
            &self,
            _handle: &SandboxHandle,
            cmd: &[String],
        ) -> Result<ExecOutput, SandboxError> {
            if cmd.is_empty() {
                return Err(SandboxError::Exec("empty command".into()));
            }
            run_via_codex(cmd).await
        }

        async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    /// Run `cmd` through codex's `SandboxManager` and capture its output.
    async fn run_via_codex(cmd: &[String]) -> Result<ExecOutput, SandboxError> {
        let cwd = std::env::current_dir().map_err(|e| SandboxError::Exec(e.to_string()))?;
        let abs = AbsolutePathBuf::try_from(cwd.as_path())
            .map_err(|_| SandboxError::Exec("current dir is not absolute".into()))?;
        let uri = PathUri::from_abs_path(&abs);

        let (program, args) = cmd
            .split_first()
            .expect("non-empty command checked by caller");
        let command = SandboxCommand {
            program: OsString::from(program.as_str()),
            args: args.to_vec(),
            cwd: uri.clone(),
            env: HashMap::new(),
            managed_network: None,
            additional_permissions: None,
        };

        let request = SandboxManager::new()
            .transform(SandboxTransformRequest {
                command,
                permissions: &PermissionProfile::default(),
                // Default profile has no symbolic `:workspace_roots` rules to bind.
                workspace_roots: None,
                sandbox: SandboxType::None,
                enforce_managed_network: false,
                environment_id: None,
                network: None,
                sandbox_policy_cwd: &uri,
                sandbox_exe: None,
                use_legacy_landlock: false,
                windows_sandbox_level: WindowsSandboxLevel::default(),
            })
            .map_err(|e| SandboxError::Exec(e.to_string()))?;

        let mut spawned = spawn_process(codex_sandboxing::SpawnRequest {
            command: &request.command,
            cwd: request
                .cwd
                .to_abs_path()
                .map_err(|e| SandboxError::Exec(e.to_string()))?
                .as_path(),
            env: &request.env,
            arg0: &request.arg0,
            sandbox: request.sandbox,
            windows_sandbox: None,
            tty: false,
            stdin_open: false,
            inherited_fds: ChildFds::Inherited(&[]),
        })
        .await
        .map_err(|e| SandboxError::Exec(e.to_string()))?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(chunk) = spawned.stdout_rx.recv().await {
            stdout.extend_from_slice(&chunk);
        }
        while let Some(chunk) = spawned.stderr_rx.recv().await {
            stderr.extend_from_slice(&chunk);
        }
        let exit_code = spawned.exit_rx.await.unwrap_or(-1);
        Ok(ExecOutput {
            exit_code,
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[tokio::test]
        async fn codex_sandbox_exec_runs_command_through_codex_manager() {
            // Exercises the deep codex integration (ADR-0005): the command is run
            // via codex's `SandboxManager`, not a bespoke CLI seam.
            let sandbox = CodexSandbox::new();
            let handle = sandbox
                .spawn(&ExecSpec::command(vec!["echo".into(), "hi".into()]))
                .await
                .unwrap();
            let out = sandbox
                .exec(&handle, &["echo".into(), "codex-sandbox".into()])
                .await
                .expect("codex SandboxManager spawn must succeed");
            assert!(out.stdout.contains("codex-sandbox"));
            assert_eq!(out.exit_code, 0);
            sandbox.destroy(handle).await.unwrap();
        }

        #[tokio::test]
        async fn codex_sandbox_rejects_empty_command() {
            let sandbox = CodexSandbox::new();
            let handle = sandbox
                .spawn(&ExecSpec::command(vec!["true".into()]))
                .await
                .unwrap();
            let res = sandbox.exec(&handle, &[]).await;
            assert!(matches!(res, Err(SandboxError::Exec(_))));
        }
    }
}

use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::Duration;

use agent_core::{ModelClient, OpenAiModel};
use agent_sandbox::Sandbox;
use aria_agent_cloud::app::{AppState, DEFAULT_MODEL};
use aria_agent_cloud::cli_config;
use aria_agent_cloud::context::PgContextStore;
use aria_agent_cloud::store::{ensure_schema, PgStore};
use aria_agent_cloud::{build_router, upgrade, KeyCache};
use clap::{ArgAction, Args, Parser, Subcommand};
use sqlx::postgres::PgPoolOptions;

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "aria-agent",
    about = "Agents Cloud API + CLI",
    version = AGENT_VERSION,
    arg_required_else_help = false,
    disable_version_flag = true
)]
struct Cli {
    /// Print version
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    _version: (),
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Write CLI config (~/.ariacompute/agent-cli.yml)
    Setup(SetupArgs),
    /// Replace this CLI + libaria-agent_ffi from Releases
    Upgrade {
        /// Target version (default: latest stable)
        version: Option<String>,
        /// Override the upgrade_url from config
        #[arg(long)]
        url: Option<String>,
    },
    /// Start the cloud HTTP server
    Serve {
        /// Listen port (default: CLOUD_PORT env or 3000)
        #[arg(long)]
        port: Option<u16>,
    },
    /// Print version
    Version,
}

#[derive(Args)]
struct SetupArgs {
    /// Show CLI config status
    #[arg(long)]
    status: bool,
    /// Remove the CLI config file
    #[arg(long)]
    clear: bool,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Setup(args)) => cmd_setup(args)?,
        Some(Command::Upgrade { version, url }) => {
            let cfg = cli_config::load_cli_config()?;
            let upgrade_url = upgrade::resolve_upgrade_url(url.as_deref(), &cfg.upgrade_url)?;
            let targets = upgrade::InstallTargets {
                staging_root: cli_config::ensure_aria_home()?,
                cli_dest: std::env::current_exe()?,
                lib_dir: cli_config::lib_dir()?,
            };
            upgrade::upgrade_into(
                &targets,
                &upgrade_url,
                None,
                version.as_deref(),
                AGENT_VERSION,
            )
            .await?;
        }
        Some(Command::Serve { port }) => cmd_serve(resolve_port(port)).await?,
        None => cmd_serve(resolve_port(None)).await?,
        Some(Command::Version) => println!("aria-agent {AGENT_VERSION}"),
    }
    Ok(())
}

fn cmd_setup(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.status {
        return cmd_setup_status();
    }
    if args.clear {
        return cmd_setup_clear();
    }

    // The Releases source is configured only via interactive mode.
    let upgrade_url = if std::io::stdin().is_terminal() {
        let ans = prompt("Releases source (1=github, 2=gitee) [1]: ")?;
        match ans.trim() {
            "" | "1" | "github" | "gh" => cli_config::upgrade_url_for_site("github"),
            "2" | "gitee" => cli_config::upgrade_url_for_site("gitee"),
            other => {
                return Err(format!("unknown source: {other} (expected github or gitee)").into())
            }
        }
    } else {
        // Non-interactive: fall back to the GitHub default.
        cli_config::upgrade_url_for_site("github")
    };

    let path = cli_config::save_cli_config(&cli_config::AgentCliConfig {
        upgrade_url: upgrade_url.clone(),
    })?;
    println!("wrote {} (upgrade_url={upgrade_url})", path.display());
    Ok(())
}

/// Read one line from stdin (used by interactive `setup`).
fn prompt(q: &str) -> io::Result<String> {
    use std::io::Write;
    print!("{q}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// `aria-agent setup --status`: print where the CLI config lives + its values.
fn cmd_setup_status() -> Result<(), Box<dyn std::error::Error>> {
    let path = cli_config::cli_config_path()?;
    if path.exists() {
        println!("config: {}", path.display());
        let cli = cli_config::load_cli_config()?;
        if cli.upgrade_url.is_empty() {
            println!("upgrade_url: (not set)");
        } else {
            println!("upgrade_url: {}", cli.upgrade_url);
        }
    } else {
        println!(
            "config: {} (missing; run `aria-agent setup`)",
            path.display()
        );
    }
    println!("lib: {}", cli_config::lib_dir()?.display());
    Ok(())
}

/// `aria-agent setup --clear`: delete the CLI config file.
fn cmd_setup_clear() -> Result<(), Box<dyn std::error::Error>> {
    match cli_config::clear_cli_config()? {
        Some(path) => println!("cleared {}", path.display()),
        None => println!("(nothing to clear)"),
    }
    Ok(())
}

/// Resolve the cloud listen port: explicit `--port`, else `CLOUD_PORT` env (from
/// `.env`), else 3000.
fn resolve_port(explicit: Option<u16>) -> u16 {
    if let Some(p) = explicit {
        return p;
    }
    std::env::var("CLOUD_PORT")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(3000)
}

async fn cmd_serve(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Prefer CLI config from ~/.ariacompute/agent-cli.yml.
    let _ = cli_config::ensure_aria_home();
    let cli = cli_config::load_cli_config()?;
    if cli.upgrade_url.is_empty() {
        tracing::info!("agent-cli: no upgrade_url configured (run `aria-agent setup`)");
    } else {
        tracing::info!("agent-cli: upgrade_url={}", cli.upgrade_url);
    }
    // Expose the installed libaria-agent_ffi to native SDKs unless overridden.
    if std::env::var("ARIA_AGENT_FFI_LIB").is_err() {
        if let Ok(lib) = cli_config::lib_dir() {
            std::env::set_var("ARIA_AGENT_FFI_LIB", lib);
        }
    }

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/agent".into());
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await?;
    ensure_schema(&pool).await?;

    // Context memory is Postgres-backed (`context_fragments`, pgvector) and
    // sharded per principal — no local/embedded store is opened here.
    tracing::info!("context: postgres + pgvector (per-principal scoping)");

    // In-memory API-key resolution cache (TTL-gated; revoked keys are purged).
    let key_cache_ttl = std::env::var("API_KEY_CACHE_TTL_SEC")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(60);
    let key_cache = KeyCache::new(Duration::from_secs(key_cache_ttl));
    // Real model for production; tests inject a stub via `make_model`.
    let make_model: aria_agent_cloud::MakeModel =
        Arc::new(|| Box::new(OpenAiModel::new(DEFAULT_MODEL)) as Box<dyn ModelClient>);
    // Tool execution goes through codex's real `SandboxManager` (ADR-0005).
    let make_sandbox: aria_agent_cloud::MakeSandbox =
        Arc::new(|| Arc::new(codex_sandbox::CodexSandbox::new()) as Arc<dyn Sandbox>);
    // Tenant-scoped Postgres context stores.
    let context_pool = pool.clone();
    let make_context: aria_agent_cloud::MakeContext = Arc::new(move |p| {
        PgContextStore::new(context_pool.clone(), p.id.clone())
            as Arc<dyn agent_core::context::ContextStore>
    });

    let pg = Arc::new(PgStore::new(pool));
    let state = AppState {
        agents: pg.clone(),
        sessions: pg.clone(),
        keys: pg,
        key_cache,
        make_model,
        make_sandbox,
        make_context,
    };

    let app = build_router(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("agent-cloud listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
