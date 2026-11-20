//! `aria-agent` binary — CLI parsing and process startup. All request handling,
//! storage, plugin assembly and upgrade logic lives in the library
//! (`aria_agent_cloud`).
//!
//! This file is **excluded from the coverage gate**: it is the process entry
//! point (`main` / `run` / `cmd_*`) and signal/stdio interaction, neither of
//! which can be exercised in-process.

use std::collections::HashMap;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_core::seams::{ApprovalBroker, AutoAllow, DenyAll, NoQuestions, PermissionPreset};
use agent_core::{gateway_api_key, gateway_base_url, gateway_model, ModelClient, OpenAiModel};
use agent_sandbox::Sandbox;
use aria_agent_cloud::app::{AppState, DEFAULT_MODEL};
use aria_agent_cloud::cli_config;
use aria_agent_cloud::context::PgContextStore;
use aria_agent_cloud::store::{ensure_schema, PgStore};
use aria_agent_cloud::{build_router, upgrade, KeyCache};
use aria_agent_dsh::{DshAgentLoop, DshConfig, DshPool, SeamApprovalBridge};
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

/// Parse an unsigned environment value, falling back to `default`.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Map the shared permission preset onto dsh's `sdk` profile presets.
fn dsh_permission_mode(preset: PermissionPreset) -> &'static str {
    match preset {
        PermissionPreset::ReadOnly => "read-only",
        PermissionPreset::WorkspaceWrite => "workspace-write",
        PermissionPreset::FullAccess => "danger-full-access",
    }
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
    // Tool execution goes through the pluggable sandbox seam. Docker is the
    // default backend (ADR-0003); the run body may still select kata/cube/browser.
    let make_sandbox: aria_agent_cloud::MakeSandbox =
        Arc::new(|| Arc::new(agent_sandbox::DockerSandbox::new()) as Arc<dyn Sandbox>);
    // Tenant-scoped Postgres context stores.
    let context_pool = pool.clone();
    let make_context: aria_agent_cloud::MakeContext = Arc::new(move |p| {
        PgContextStore::new(context_pool.clone(), p.id.clone())
            as Arc<dyn agent_core::context::ContextStore>
    });

    // --- Turn runtime: DeepSeek Harness by default (ADR-0019) ---
    // The operator may pin `AGENT_HARNESS=native`; anything else is validated by
    // `default_agent_loop` at construction time (never a silent fallback).
    if std::env::var("AGENT_HARNESS").is_err() {
        std::env::set_var("AGENT_HARNESS", "dsh");
    }
    let harness_name = aria_agent_cloud::app::harness_from_env();
    let make_harness: Option<aria_agent_cloud::app::MakeHarness> = if harness_name == "native" {
        None
    } else {
        let dsh_root = std::env::var("DSH_HOME_ROOT").unwrap_or_else(|_| ".aria/dsh".into());
        let pool_max = env_u64("DSH_POOL_MAX", 8) as usize;
        let idle = Duration::from_secs(env_u64("DSH_IDLE_TTL_SEC", 900));
        let bin = std::env::var("DSH_BIN").ok();
        // Both harnesses are configured from the same GATEWAY_* values.
        let model_override = gateway_model();
        let gateway_base = gateway_base_url();
        let gateway_key = gateway_api_key();
        // One pool per tenant: session homes never cross tenants (AGENTS rule 7).
        let pools: Arc<std::sync::Mutex<HashMap<String, Arc<DshPool>>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        Some(Arc::new(move |principal, cfg| {
            let tenant = principal.id.clone();
            let mut registry = pools.lock().expect("dsh pool registry poisoned");
            let pool = registry.entry(tenant.clone()).or_insert_with(|| {
                let preset = PermissionPreset::parse(&cfg.permission_preset)
                    .unwrap_or(PermissionPreset::WorkspaceWrite);
                let approvals: Arc<dyn ApprovalBroker> = match cfg.approval_mode.as_str() {
                    "auto" => Arc::new(AutoAllow),
                    _ => Arc::new(DenyAll),
                };
                let bridge = Arc::new(SeamApprovalBridge::new(
                    approvals,
                    Arc::new(NoQuestions),
                    cfg.session.clone(),
                    preset,
                ));
                let dsh_model = model_override
                    .as_deref()
                    .unwrap_or(cfg.model.as_str())
                    .to_string();
                let mut dsh = DshConfig::for_session(
                    &PathBuf::from(&dsh_root).join(&tenant),
                    &cfg.session,
                    &dsh_model,
                )
                .with_permission_mode(dsh_permission_mode(preset));
                if let Some(bin) = &bin {
                    dsh = dsh.with_bin(bin.clone());
                }
                // The child env is an allow-list: bridge the deployment's
                // GATEWAY_* values into the names dsh reads, never inheriting
                // the parent environment.
                dsh = dsh.with_gateway_env(
                    gateway_base.as_deref().unwrap_or_default(),
                    gateway_key.as_str(),
                );
                Arc::new(DshPool::new(dsh, pool_max, idle, bridge))
            });
            Arc::new(DshAgentLoop::new(pool.clone()))
        }))
    };

    // One Postgres store implements the metadata, team and schedule seams; each
    // table is `principal_id`-scoped, so no extra wiring is needed here.
    // Governance state lives in Postgres too: policy revisions, audit events,
    // usage intervals and checkpoint records must survive a restart and stay
    // `principal_id`-scoped (ADR-0021/0022).
    aria_agent_cloud::governance::install_store(Arc::new(
        aria_agent_cloud::store::PgGovernanceStore::new(pool.clone()),
    ));
    // Network enforcement is brought up before anything can run: an internal
    // network, the proxy container, and the token shared with it. Any failure is
    // fatal (ADR-0024) — starting without it would be a silent bypass.
    match aria_agent_cloud::internal_egress::bootstrap().await {
        Ok(Some(info)) => {
            aria_agent_cloud::governance::install_proxy_info(info);
        }
        Ok(None) => {}
        Err(e) => {
            return Err(format!(
                "AGENT_EGRESS_ENFORCEMENT=network is not usable: {e}. \
                 Fix the Docker network/proxy or set AGENT_EGRESS_ENFORCEMENT=proxy"
            )
            .into());
        }
    }
    let pg = Arc::new(PgStore::new(pool));
    let teams: Arc<dyn aria_agent_orchestrator::teams::TeamStore> = pg.clone();
    let schedule: Arc<dyn agent_core::seams::ScheduleSeam> = pg.clone();
    let titles: Arc<dyn aria_agent_cloud::store::TitleStore> = pg.clone();
    let state = AppState {
        agents: pg.clone(),
        sessions: pg.clone(),
        keys: pg,
        teams,
        schedule,
        titles,
        key_cache,
        make_model,
        make_sandbox,
        make_context,
        harness: make_harness,
    };

    let app = build_router(state);

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("agent-cloud listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
