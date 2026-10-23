//! Clawblox CLI - init and run worlds from anywhere

use axum::{
    extract::{
        ws::{rejection::WebSocketUpgradeRejection, Message, WebSocket, WebSocketUpgrade},
        OriginalUri, Path as AxumPath, Query, State,
    },
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use chrono::{SecondsFormat, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use dashmap::DashMap;
use flate2::{write::GzEncoder, Compression};
use futures_util::{SinkExt, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::OpenOptions;
use std::io::{BufRead, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::interval;
use tokio_tungstenite::tungstenite;
use tower_http::cors::{Any, CorsLayer};
use uuid::Uuid;

#[cfg(unix)]
use libc;

use include_dir::{include_dir, Dir};
use tower_http::services::ServeDir;

use clawblox::{
    agent_runner::{start_agent_tmux_for_local_world, AgentRunnerOptions, ClaudePacing},
    capture::{CaptureAttach, CaptureManager},
    config::{PhysicsConfig, SpeechConfig, SpeechMode},
    db,
    game::{
        self,
        async_bridge::AsyncBridge,
        constants::audio::{
            DEFAULT_BYTES_PER_SAMPLE, DEFAULT_CHANNELS, DEFAULT_PLAYBACK_SPEED,
            DEFAULT_SAMPLE_RATE_HZ, MIN_PLAYBACK_SPEED, MIN_POSITIVE_U16, MIN_POSITIVE_U32,
            SERVER_PREDICTED_END_REASON, STREAM_TTL_SECS,
        },
        find_or_create_instance,
        instance::{
            ErrorMode, GameInstance, PlayerObservation, SpectatorEntity, SpectatorObservation,
            SpectatorPlayerInfo, SpectatorRender,
        },
        script_bundle::load_world_and_entry_script,
        GameManager, GameManagerHandle,
    },
    manifest, paths as clawblox_paths, recording, replay,
    replay_agent_log::{
        load_agent_timeline, ReplayAgentMessage, ReplayAgentTimeline, ReplayTickAnchor,
    },
    replay_metadata::{self, ReplayMetadata},
    snapshot::GameInstanceSnapshot,
    world_runner::{run_external_world_blocking, ExternalWorldCommand},
    world_source::{self, WorldSourceReporter},
};

static DOCS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/docs");
static LOCAL_RUNTIME_UI_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/frontend_runtime/dist");
const DEFAULT_AGENT_API_DOC: &str = "API.md";
const LEGACY_AGENT_SKILL_DOC: &str = "SKILL.md";

#[path = "../app_browser.rs"]
mod app_browser;

struct CliWorldSourceReporter {
    spinner: ProgressBar,
    files: AtomicUsize,
}

impl CliWorldSourceReporter {
    fn new() -> Self {
        let spinner = ProgressBar::new_spinner();
        spinner.set_style(
            ProgressStyle::with_template("{spinner:.orange} {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_spinner()),
        );
        spinner.enable_steady_tick(Duration::from_millis(80));
        Self {
            spinner,
            files: AtomicUsize::new(0),
        }
    }
}

impl WorldSourceReporter for CliWorldSourceReporter {
    fn resolving(&self, source: &str) {
        self.spinner
            .set_message(format!("Resolving world {source}"));
    }

    fn using_cached_world(&self, dir: &Path) {
        self.spinner
            .set_message(format!("Using cached world {}", dir.display()));
    }

    fn downloading_github_world(&self, source: &str, dir: &Path) {
        self.files.store(0, Ordering::Relaxed);
        self.spinner.set_message(format!("Downloading {source}"));
        eprintln!("Caching world in {}", dir.display());
    }

    fn downloading_archive(&self, path: &str) {
        self.spinner
            .set_message(format!("Downloading GitHub archive {path}"));
    }

    fn downloaded_file(&self, path: &str, _bytes: u64) {
        let count = self.files.fetch_add(1, Ordering::Relaxed) + 1;
        self.spinner
            .set_message(format!("Downloaded {count} files ({path})"));
    }

    fn ready(&self, dir: &Path) {
        let count = self.files.load(Ordering::Relaxed);
        if count > 0 {
            self.spinner
                .finish_with_message(format!("World ready: {} ({count} files)", dir.display()));
        } else {
            self.spinner
                .finish_with_message(format!("World ready: {}", dir.display()));
        }
    }
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum LocalDatastore {
    Sqlite,
    Postgres,
    #[value(name = "none")]
    NoDb,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum CliAgentKind {
    Claude,
    Codex,
    Kimi,
}

impl LocalDatastore {
    fn as_arg(self) -> &'static str {
        match self {
            Self::Sqlite => "sqlite",
            Self::Postgres => "postgres",
            Self::NoDb => "none",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ClaudePlan {
    Pro,
    Max5,
    Max20,
}

impl ClaudePlan {
    /// Rough model-request budgets per 5-hour window and per week. Anthropic
    /// does not publish exact numbers, so these are conservative estimates for
    /// Sonnet; tune them with --rate-limit after watching `/status`.
    fn rate_limits(self) -> Vec<String> {
        let (window, week) = match self {
            Self::Pro => (250, 3000),
            Self::Max5 => (1250, 15000),
            Self::Max20 => (5000, 60000),
        };
        vec![format!("{window}/5h"), format!("{week}/week")]
    }
}

/// Writes the live pacing override that the agent's pace hook re-reads before
/// every model request, so budgets change without restarting the agent.
fn update_agent_pace(
    agent_dir: &std::path::Path,
    rate_limits: Vec<String>,
    min_request_interval: Option<f64>,
    burst: Option<bool>,
    reset: bool,
) {
    let runtime_dir = agent_dir.join("runtime");
    if !runtime_dir.is_dir() {
        eprintln!(
            "Error: {} is not an agent directory (no runtime/ folder)",
            agent_dir.display()
        );
        std::process::exit(2);
    }
    let path = runtime_dir.join("pace.json");
    if reset {
        let _ = std::fs::remove_file(&path);
        println!(
            "Removed {}; the agent is back to its startup budget.",
            path.display()
        );
        return;
    }
    let mut config: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let changed = !rate_limits.is_empty() || min_request_interval.is_some() || burst.is_some();
    if !rate_limits.is_empty() {
        config.insert("rate_limits".into(), rate_limits.into());
    }
    if let Some(seconds) = min_request_interval {
        config.insert("min_interval_seconds".into(), seconds.into());
    }
    if let Some(burst) = burst {
        config.insert("pace".into(), if burst { "burst" } else { "even" }.into());
    }
    if changed {
        let text = serde_json::to_string_pretty(&config).unwrap_or_default();
        if let Err(err) = std::fs::write(&path, format!("{text}\n")) {
            eprintln!("Error: failed to write {}: {err}", path.display());
            std::process::exit(1);
        }
        println!(
            "Updated {}; the agent picks it up within a few seconds.",
            path.display()
        );
    }
    if config.is_empty() {
        println!("No live override; the agent uses the budget it started with.");
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&config).unwrap_or_default()
        );
    }
    if let Ok(log) = std::fs::read_to_string(agent_dir.join("logs").join("pace.log")) {
        if let Some(last) = log.lines().last() {
            println!("Last pace.log entry: {last}");
        }
    }
}

fn parse_rate_limit_arg(text: &str) -> Result<String, String> {
    clawblox::agent_runner::parse_rate_limit(text)?;
    Ok(text.trim().to_string())
}

impl CliAgentKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
        }
    }
}

#[derive(Parser)]
#[command(name = "clawblox")]
#[command(about = "Clawblox world engine CLI", long_about = None, version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a new world
    Init {
        /// Name for the new world (creates subdirectory). Omit to init in current directory.
        name: Option<String>,
    },
    /// Run a world locally
    Run {
        /// World directory or GitHub folder URL (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Port to run the server on
        #[arg(short, long, default_value = "8080", env = "WORLD_PORT")]
        port: u16,
        /// URL base path to mount the runtime under
        #[arg(long, default_value = "/", env = "WORLD_BASE_PATH")]
        base_path: String,
        /// Run as daemon (internal use only)
        #[arg(long, hide = true)]
        daemon: bool,
        /// Datastore backend for local DataStoreService
        #[arg(long, value_enum, default_value_t = LocalDatastore::Sqlite)]
        datastore: LocalDatastore,
        /// Record session to a .clawrec file for later replay (on by default
        /// when --agent is set)
        #[arg(long, env = "WORLD_RECORD")]
        record: bool,
        /// Don't record, even when --agent is set
        #[arg(long, conflicts_with = "record")]
        no_record: bool,
        /// Directory to write recording output into (default: <world>/recordings)
        #[arg(long, env = "CLAWBLOX_RECORD_DIR")]
        record_dir: Option<PathBuf>,
        /// Resume a previously snapshotted local run from this file
        #[arg(long, env = "WORLD_RESUME_PATH")]
        resume: Option<PathBuf>,
        /// Skip world.toml [run].command delegation and run the built-in engine directly
        #[arg(long, hide = true, env = "CLAWBLOX_SKIP_RUN_COMMAND")]
        skip_run_command: bool,
        /// Start an agent in tmux after the local world starts
        #[arg(long, value_enum)]
        agent: Option<CliAgentKind>,
        /// Display name passed to the agent
        #[arg(long, default_value = "agent")]
        agent_name: String,
        /// Agent model to use
        #[arg(long)]
        model: Option<String>,
        /// Agent permission/approval mode
        #[arg(long, default_value = "bypassPermissions")]
        permission_mode: String,
        /// tmux session name for the agent
        #[arg(long)]
        tmux_session: Option<String>,
        /// Directory for agent state, logs, transcripts, and workspace
        #[arg(long)]
        agent_dir: Option<PathBuf>,
        /// System/developer prompt file for the agent
        #[arg(long)]
        system_prompt: Option<PathBuf>,
        /// Enable agent sandbox mode
        #[arg(long)]
        sandbox: bool,
        /// Pace a Claude agent to fit a Claude subscription's usage limits
        /// (rough request budgets; see docs/agents.md). Pro also defaults the
        /// model to Sonnet.
        #[arg(long, value_enum)]
        plan: Option<ClaudePlan>,
        /// Claude agent request budget like 250/5h or 3000/week (repeatable;
        /// replaces the --plan budget)
        #[arg(long = "rate-limit", value_parser = parse_rate_limit_arg)]
        rate_limits: Vec<String>,
        /// Minimum seconds between a Claude agent's model requests
        #[arg(long)]
        min_request_interval: Option<f64>,
        /// Spend each budget window as fast as allowed instead of spreading
        /// requests evenly across it
        #[arg(long)]
        burst: bool,
    },
    /// Show or change a running Claude agent's request budget without
    /// restarting it (writes <agent-dir>/runtime/pace.json)
    Pace {
        /// The agent's directory (default for `clawblox run`:
        /// <world>/.clawblox/agents/<name>)
        agent_dir: PathBuf,
        /// New request budget like 300/5h or 4500/week (repeatable)
        #[arg(long = "rate-limit", value_parser = parse_rate_limit_arg)]
        rate_limits: Vec<String>,
        /// New minimum seconds between model requests
        #[arg(long)]
        min_request_interval: Option<f64>,
        /// Spend each window as fast as allowed
        #[arg(long, conflicts_with = "even")]
        burst: bool,
        /// Spread requests evenly across each window
        #[arg(long)]
        even: bool,
        /// Remove pace.json and go back to the budget the agent started with
        #[arg(long, conflicts_with_all = ["rate_limits", "min_request_interval", "burst", "even"])]
        reset: bool,
    },
    /// Replay a recorded session
    Replay {
        /// Path to .clawrec recording file
        file: PathBuf,
        /// Playback speed multiplier
        #[arg(long, default_value = "1.0")]
        speed: f32,
        /// Port to run the replay server on
        #[arg(short, long, default_value = "8080")]
        port: u16,
        /// URL base path to mount the replay runtime under
        #[arg(long, default_value = "/")]
        base_path: String,
        /// URL base path to use when resolving replay assets
        #[arg(long, hide = true)]
        asset_base_path: Option<String>,
        /// Override the world directory (for serving assets)
        #[arg(long)]
        world_dir: Option<PathBuf>,
        /// Path to agent JSONL log file (from agent/logs/*/*.jsonl)
        #[arg(long)]
        agent: Option<PathBuf>,
    },
    /// Serve .clawrec data through the backend-agnostic replay adapter protocol
    ReplayAdapter,
    /// Browse world recordings and open them in the replay UI
    App {
        /// Path to world directory
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Port to run the app server on
        #[arg(short, long, default_value = "8080")]
        port: u16,
        /// URL base path to mount the app under
        #[arg(long, default_value = "/")]
        base_path: String,
    },
    /// Start a world locally in background (daemon mode)
    Start {
        /// World directory or GitHub folder URL (default: current directory)
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Port to run the server on
        #[arg(short, long, default_value = "8080")]
        port: u16,
        /// URL base path to mount the runtime under
        #[arg(long, default_value = "/")]
        base_path: String,
        /// Datastore backend for local DataStoreService
        #[arg(long, value_enum, default_value_t = LocalDatastore::Sqlite)]
        datastore: LocalDatastore,
    },
    /// Stop a background world server started with `clawblox start`
    Stop {
        /// Port of the server to stop
        #[arg(short, long, default_value = "8080")]
        port: u16,
    },
    /// Log in or register an account on clawblox.com
    Login {
        /// Your name (registers a new account)
        name: Option<String>,
        /// Use an existing API key instead of registering
        #[arg(long)]
        api_key: Option<String>,
        /// Server URL
        #[arg(long, default_value = "https://clawblox.com")]
        server: String,
    },
    /// Deploy a world to clawblox.com
    Deploy {
        /// Path to world directory
        #[arg(default_value = ".")]
        path: PathBuf,
        /// API key (overrides stored credentials)
        #[arg(long, env = "CLAWBLOX_API_KEY")]
        api_key: Option<String>,
        /// Server URL (overrides stored credentials)
        #[arg(long)]
        server: Option<String>,
    },
    /// Install clawblox to system PATH
    Install {
        /// Target version (ignored, for compatibility)
        #[arg(default_value = "latest")]
        target: String,
    },
    /// Fetch latest engine docs from GitHub into ./docs/
    Docs {
        /// Use only bundled local docs (skip GitHub fetch)
        #[arg(long)]
        local: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Init { name } => init_project(name),
        Commands::Run {
            path,
            port,
            base_path,
            daemon,
            datastore,
            record,
            no_record,
            record_dir,
            resume,
            skip_run_command,
            agent,
            agent_name,
            model,
            permission_mode,
            tmux_session,
            agent_dir,
            system_prompt,
            sandbox,
            plan,
            rate_limits,
            min_request_interval,
            burst,
        } => {
            let pacing = ClaudePacing {
                rate_limits: if rate_limits.is_empty() {
                    plan.map(ClaudePlan::rate_limits).unwrap_or_default()
                } else {
                    rate_limits
                },
                min_interval_seconds: min_request_interval,
                burst,
            };
            if pacing.is_enabled() && !matches!(agent, Some(CliAgentKind::Claude)) {
                eprintln!(
                    "Error: --plan, --rate-limit and --min-request-interval require --agent claude"
                );
                std::process::exit(2);
            }
            if pacing.is_enabled() {
                println!(
                    "Claude pacing: budget [{}], about one model request every {:.0}s",
                    pacing.rate_limits.join(", "),
                    pacing.even_interval_seconds()
                );
            }
            let record = record || (agent.is_some() && !no_record);
            let model = model
                .or_else(|| matches!(plan, Some(ClaudePlan::Pro)).then(|| "sonnet".to_string()));
            run_game(
                path,
                port,
                &base_path,
                daemon,
                datastore,
                record,
                record_dir.or_else(|| std::env::var_os("WORLD_RECORD_DIR").map(PathBuf::from)),
                resume,
                skip_run_command,
                agent.map(|kind| AgentRunnerOptions {
                    kind: kind.as_str().to_string(),
                    name: agent_name,
                    model,
                    permission_mode,
                    tmux_session,
                    agent_dir,
                    system_prompt,
                    sandbox,
                    sandbox_config: clawblox::config::SandboxConfig::default(),
                    pacing,
                }),
            )
        }
        Commands::Pace {
            agent_dir,
            rate_limits,
            min_request_interval,
            burst,
            even,
            reset,
        } => update_agent_pace(
            &agent_dir,
            rate_limits,
            min_request_interval,
            burst.then_some(true).or(even.then_some(false)),
            reset,
        ),
        Commands::Replay {
            file,
            speed,
            port,
            base_path,
            asset_base_path,
            world_dir,
            agent,
        } => run_replay(
            file,
            speed,
            port,
            &base_path,
            asset_base_path.as_deref(),
            world_dir,
            agent,
        ),
        Commands::ReplayAdapter => run_replay_adapter(),
        Commands::App {
            path,
            port,
            base_path,
        } => app_browser::run_app(path, port, &base_path),
        Commands::Start {
            path,
            port,
            base_path,
            datastore,
        } => start_game(path, port, &base_path, datastore),
        Commands::Stop { port } => stop_game(port),
        Commands::Login {
            name,
            api_key,
            server,
        } => login(name, api_key, server),
        Commands::Deploy {
            path,
            api_key,
            server,
        } => deploy_game(path, api_key, server),
        Commands::Install { target: _ } => install_cli(),
        Commands::Docs { local } => fetch_docs(local),
    }
}

// =============================================================================
// Install Command
// =============================================================================

fn install_cli() {
    let current_exe = std::env::current_exe().expect("Failed to get current executable path");
    let version = env!("CARGO_PKG_VERSION");

    #[cfg(unix)]
    {
        let home = home::home_dir().expect("Failed to get home directory");

        // Install to ~/.local/share/clawblox/versions/X.X.X
        let versions_dir = home.join(".local/share/clawblox/versions");
        std::fs::create_dir_all(&versions_dir).expect("Failed to create versions directory");
        let version_path = versions_dir.join(version);
        std::fs::copy(&current_exe, &version_path).expect("Failed to copy binary");

        // Make executable
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&version_path, std::fs::Permissions::from_mode(0o755))
            .expect("Failed to set permissions");

        // Create symlink in ~/.local/bin
        let bin_dir = home.join(".local/bin");
        std::fs::create_dir_all(&bin_dir).expect("Failed to create ~/.local/bin");
        let symlink_path = bin_dir.join("clawblox");

        // Remove old symlink if exists
        let _ = std::fs::remove_file(&symlink_path);
        std::os::unix::fs::symlink(&version_path, &symlink_path).expect("Failed to create symlink");

        println!("Installed to: {}", symlink_path.display());

        // Check if in PATH
        let path_env = std::env::var("PATH").unwrap_or_default();
        if !path_env.split(':').any(|p| PathBuf::from(p) == bin_dir) {
            println!();
            println!("Add to your shell profile:");
            println!("  export PATH=\"{}:$PATH\"", bin_dir.display());
        }
    }

    #[cfg(windows)]
    {
        let home = home::home_dir().expect("Failed to get home directory");

        // Install to ~/.local/share/clawblox/versions/X.X.X
        let versions_dir = home
            .join(".local")
            .join("share")
            .join("clawblox")
            .join("versions");
        std::fs::create_dir_all(&versions_dir).expect("Failed to create versions directory");
        let version_path = versions_dir.join(format!("{}.exe", version));
        std::fs::copy(&current_exe, &version_path).expect("Failed to copy binary");

        // Create launcher in ~/.local/bin (same as Claude Code)
        let install_dir = home.join(".local").join("bin");
        std::fs::create_dir_all(&install_dir).expect("Failed to create bin directory");
        let install_path = install_dir.join("clawblox.exe");
        std::fs::copy(&version_path, &install_path).expect("Failed to copy to bin");

        println!("Installed to: {}", install_path.display());

        // Add to user PATH via registry
        let path_env = std::env::var("PATH").unwrap_or_default();
        let install_dir_str = install_dir.to_string_lossy();
        if !path_env.split(';').any(|p| p == install_dir_str) {
            println!();
            println!("Adding to user PATH...");
            let _ = std::process::Command::new("powershell")
                .args([
                    "-Command",
                    &format!(
                        "[Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path', 'User') + ';{}', 'User')",
                        install_dir_str
                    ),
                ])
                .output();
            println!("Restart your terminal to use 'clawblox' command.");
        }
    }
}

// =============================================================================
// Docs Command
// =============================================================================

fn fetch_docs(local: bool) {
    let docs_dir = std::env::current_dir()
        .expect("Failed to get current directory")
        .join("docs");

    if local {
        write_embedded_dir(&DOCS_DIR, &docs_dir);
        println!(
            "Docs written from bundled local copy to {}",
            docs_dir.display()
        );
        return;
    }

    let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
    match rt.block_on(fetch_docs_from_github(&docs_dir)) {
        Ok(()) => println!("Docs updated in {}", docs_dir.display()),
        Err(e) => {
            eprintln!("Error: Failed to fetch docs from GitHub: {}", e);
            std::process::exit(1);
        }
    }
}

// =============================================================================
// Init Command
// =============================================================================

fn write_embedded_dir(dir: &Dir, target: &Path) {
    std::fs::create_dir_all(target).expect("Failed to create directory");
    for file in dir.files() {
        let dest = target.join(file.path().file_name().unwrap());
        std::fs::write(&dest, file.contents()).expect("Failed to write file");
    }
    for subdir in dir.dirs() {
        let subdir_name = subdir.path().file_name().unwrap();
        write_embedded_dir(subdir, &target.join(subdir_name));
    }
}

const GITHUB_DOCS_API_URL: &str = "https://api.github.com/repos/nacloos/clawblox/contents/docs";

/// Fetch docs from GitHub and write them to `target_dir`.
/// Returns Ok(()) on success, Err with description on any failure.
async fn fetch_docs_from_github(target_dir: &Path) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .user_agent("clawblox-cli")
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // List files in docs/ via GitHub Contents API
    let resp = client
        .get(GITHUB_DOCS_API_URL)
        .send()
        .await
        .map_err(|e| format!("Failed to reach GitHub API: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("GitHub API returned {}", resp.status()));
    }

    let entries: Vec<serde_json::Value> = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse GitHub API response: {}", e))?;

    std::fs::create_dir_all(target_dir)
        .map_err(|e| format!("Failed to create docs directory: {}", e))?;

    for entry in &entries {
        let entry_type = entry["type"].as_str().unwrap_or("");
        if entry_type != "file" {
            continue;
        }
        let name = entry["name"]
            .as_str()
            .ok_or("Missing 'name' in GitHub API entry")?;
        let download_url = entry["download_url"]
            .as_str()
            .ok_or(format!("Missing 'download_url' for {}", name))?;

        let file_resp = client
            .get(download_url)
            .send()
            .await
            .map_err(|e| format!("Failed to download {}: {}", name, e))?;

        if !file_resp.status().is_success() {
            return Err(format!(
                "Failed to download {} ({})",
                name,
                file_resp.status()
            ));
        }

        let content = file_resp
            .bytes()
            .await
            .map_err(|e| format!("Failed to read {}: {}", name, e))?;

        std::fs::write(target_dir.join(name), &content)
            .map_err(|e| format!("Failed to write {}: {}", name, e))?;
    }

    Ok(())
}

fn init_project(name: Option<String>) {
    let (project_name, project_dir) = match name {
        Some(n) => {
            let dir = PathBuf::from(&n);
            if dir.exists() {
                eprintln!("Error: Directory '{}' already exists", n);
                std::process::exit(1);
            }
            (n, dir)
        }
        None => {
            let cwd = std::env::current_dir().expect("Failed to get current directory");
            let dir_name = cwd
                .file_name()
                .expect("Failed to get directory name")
                .to_string_lossy()
                .to_string();
            if cwd.join("world.toml").exists() {
                eprintln!("Error: world.toml already exists in current directory");
                std::process::exit(1);
            }
            (dir_name, cwd)
        }
    };

    std::fs::create_dir_all(&project_dir).expect("Failed to create project directory");

    // Create world.toml
    let world_toml = format!(
        r#"# {} - World Configuration
name = "{}"
description = "A new Clawblox world"
max_players = 8

[scripts]
main = "main.lua"
api = "API.md"

[renderer]
name = "Default World Renderer"
mode = "module"
api_version = 1
entry = "dist/renderer.bundle.js"
capabilities = []

[observation]
radius = 100
"#,
        project_name, project_name
    );
    std::fs::write(project_dir.join("world.toml"), world_toml)
        .expect("Failed to create world.toml");

    // Create main.lua
    let main_lua = r#"-- World entry point
local RunService = game:GetService("RunService")
local Players = game:GetService("Players")
local AgentInputService = game:GetService("AgentInputService")

--------------------------------------------------------------------------------
-- CONFIGURATION
--------------------------------------------------------------------------------

local MAP_SIZE = 100

--------------------------------------------------------------------------------
-- WORLD STATE
--------------------------------------------------------------------------------

local playerData = {}

--------------------------------------------------------------------------------
-- HELPER FUNCTIONS
--------------------------------------------------------------------------------

local function getHumanoid(player)
    local character = player.Character
    if character then
        return character:FindFirstChild("Humanoid")
    end
    return nil
end

--------------------------------------------------------------------------------
-- MAP CREATION
--------------------------------------------------------------------------------

local function createMap()
    -- Floor
    local floor = Workspace:FindFirstChild("Floor") or Instance.new("Part")
    floor.Name = "Floor"
    floor.Size = Vector3.new(MAP_SIZE, 2, MAP_SIZE)
    floor.Position = Vector3.new(0, -1, 0)
    floor.Anchored = true
    floor.Color = Color3.fromRGB(100, 150, 100)
    floor.Parent = Workspace

    print("Map created: " .. MAP_SIZE .. "x" .. MAP_SIZE .. " studs")
end

--------------------------------------------------------------------------------
-- PLAYER MANAGEMENT
--------------------------------------------------------------------------------

local function setupPlayer(player)
    playerData[player.UserId] = {
        name = player.Name,
    }

    -- Move to spawn
    local character = player.Character
    if character then
        local hrp = character:FindFirstChild("HumanoidRootPart")
        if hrp then
            hrp.Position = Vector3.new(0, 3, 0)
        end
    end

    print("Player joined: " .. player.Name)
end

local function cleanupPlayer(player)
    playerData[player.UserId] = nil
    print("Player left: " .. player.Name)
end

--------------------------------------------------------------------------------
-- INPUT HANDLING
--------------------------------------------------------------------------------

if AgentInputService then
    AgentInputService.InputReceived:Connect(function(player, inputType, inputData)
        if inputType == "MoveTo" and inputData and inputData.position then
            local humanoid = getHumanoid(player)
            if humanoid then
                local pos = inputData.position
                local x = type(pos) == "table" and tonumber(pos[1]) or nil
                local y = type(pos) == "table" and tonumber(pos[2]) or nil
                local z = type(pos) == "table" and tonumber(pos[3]) or nil
                if x and y and z and x == x and y == y and z == z then
                    humanoid:MoveTo(Vector3.new(x, y, z))
                end
            end
        end
    end)
end

--------------------------------------------------------------------------------
-- WORLD LOOP
--------------------------------------------------------------------------------

Players.PlayerAdded:Connect(setupPlayer)
Players.PlayerRemoving:Connect(cleanupPlayer)

for _, player in ipairs(Players:GetPlayers()) do
    setupPlayer(player)
end

createMap()

RunService.Heartbeat:Connect(function(dt)
    -- World logic here
end)

print("World initialized")
"#;
    std::fs::write(project_dir.join("main.lua"), main_lua).expect("Failed to create main.lua");

    // Create API.md
    let api_md = format!(
        r#"# {}

A new Clawblox world.

## Actions

### MoveTo
Move to a position on the map.

```json
{{"type": "MoveTo", "data": {{"position": [x, y, z]}}}}
```

## Map
- {}x{} stud floor
- Center at (0, 0, 0)
"#,
        project_name, "100", "100"
    );
    std::fs::write(project_dir.join(DEFAULT_AGENT_API_DOC), api_md)
        .expect("Failed to create API.md");

    // Fetch latest docs from GitHub, fall back to embedded copy
    {
        let docs_dir = project_dir.join("docs");
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        match rt.block_on(fetch_docs_from_github(&docs_dir)) {
            Ok(()) => {}
            Err(e) => {
                eprintln!(
                    "Warning: Could not fetch docs from GitHub ({}), using bundled copy",
                    e
                );
                write_embedded_dir(&DOCS_DIR, &docs_dir);
            }
        }
    }

    // Create assets/ directory for world assets (GLB models, images, audio)
    std::fs::create_dir_all(project_dir.join("assets")).expect("Failed to create assets directory");

    // Create renderer/ package for world-specific frontend modules
    let renderer_dir = project_dir.join("renderer");
    std::fs::create_dir_all(renderer_dir.join("src")).expect("Failed to create renderer directory");
    let renderer_package_json = format!(
        r#"{{
  "name": "@clawblox-renderers/{project_name}",
  "private": true,
  "type": "module",
  "clawbloxRenderer": {{
    "apiVersion": 1,
    "entry": "./src/index.js",
    "outFile": "./dist/renderer.bundle.js",
    "capabilities": []
  }}
}}
"#
    );
    std::fs::write(renderer_dir.join("package.json"), renderer_package_json)
        .expect("Failed to create renderer/package.json");
    let renderer_js = r#"import * as THREE from 'https://esm.sh/three@0.160.0'

export function createRenderer(ctx) {
  const scene = new THREE.Scene()
  scene.background = new THREE.Color(0x0b1020)

  const camera = new THREE.PerspectiveCamera(75, 1, 0.1, 500)
  const webgl = new THREE.WebGLRenderer({ canvas: ctx.canvas, antialias: true })
  webgl.shadowMap.enabled = true
  webgl.shadowMap.type = THREE.PCFSoftShadowMap

  const ambient = new THREE.AmbientLight(0xffffff, 0.7)
  const dir = new THREE.DirectionalLight(0xffffff, 0.9)
  dir.position.set(20, 30, 10)
  dir.castShadow = true
  scene.add(ambient, dir)

  const stateBuffer = ctx.runtime.state.createSnapshotBuffer({
    maxSnapshots: 12,
    interpolationDelayMs: 90,
  })
  const cameraModes = ctx.runtime.three.createCameraModeController(THREE, camera, {
    follow: { followDistance: 9, followHeight: 2, shoulderOffset: 1.0 },
    firstPersonHeight: 1.7,
  })

  const presetLib = ctx.runtime.three.createPresetMaterialLibrary({
    'example/floor': { roughness: 0.85, metalness: 0.1 },
    'example/wall': { roughness: 0.7, metalness: 0.2 },
    'example/spawn': { roughness: 0.2, metalness: 0.25, emissiveIntensity: 0.5 },
  })

  const entities = ctx.runtime.three.createEntityStore(scene)
  const tmpForward = new THREE.Vector3(0, 0, 1)
  let inputBinding = null
  let inputClient = null
  let localControlPlayer = null

  return {
    mount() {
      // Optional local control bridge for quick prototyping:
      // W -> MoveTo ahead, Space -> Jump
      inputClient = ctx.runtime.input.createLocalInputClient({ playerName: 'renderer-local' })
      inputBinding = ctx.runtime.input.bindKeyboardActions(inputClient, {
        Space: { mode: 'tap', type: 'Jump', data: {} },
      })
    },

    onResize({ width, height }) {
      camera.aspect = width / Math.max(1, height)
      camera.updateProjectionMatrix()
      webgl.setSize(width, height, false)
    },

    onState(state) {
      stateBuffer.push(state)
      const obs = stateBuffer.interpolated() || state
      const activeIds = new Set()

      for (const entity of obs.entities || []) {
        activeIds.add(entity.id)
        entities.upsert(
          entity,
          (next) => ctx.runtime.three.buildEntityMesh(THREE, next, presetLib),
          (mesh, next) => ctx.runtime.three.applyEntityTransform(THREE, mesh, next),
        )
      }
      entities.prune(activeIds)

      // Follow first available player for now.
      const target = (obs.players || [])[0]
      if (target && Array.isArray(target.position)) {
        const trackState = ctx.runtime.three.classifyAnimationTracks(target)
        const mode = trackState.reload ? 'spectator' : 'follow'
        cameraModes.update({
          mode,
          targetPosition: new THREE.Vector3(...target.position),
          targetForward: tmpForward,
          dt: 1 / 60,
        })

        // Move local bound player a bit ahead when holding W.
        // This is intentionally simplistic as a scaffold example.
        if (inputClient && localControlPlayer !== target.id && inputClient.session()) {
          localControlPlayer = target.id
        }
      }

      webgl.render(scene, camera)
    },

    unmount() {
      if (inputBinding) inputBinding.dispose()
      entities.clear()
      webgl.dispose()
    },
  }
}
"#;
    std::fs::write(renderer_dir.join("src/index.js"), renderer_js)
        .expect("Failed to create renderer/src/index.js");

    // Create .gitignore
    let gitignore = ".clawblox/\n.clawblox-*.log\n";
    let gitignore_path = project_dir.join(".gitignore");
    if !gitignore_path.exists() {
        std::fs::write(&gitignore_path, gitignore).expect("Failed to create .gitignore");
    }

    println!("Created new world: {}", project_name);
    println!();
    if project_dir != std::env::current_dir().unwrap_or_default() {
        println!("  cd {}", project_name);
    }
    println!("  clawblox run");
}

fn read_agent_api_doc(world_dir: &Path, config: &clawblox::config::WorldConfig) -> Option<String> {
    let mut candidates: Vec<&str> = Vec::new();

    match config.scripts.api.as_deref().map(str::trim) {
        Some("") => {}
        Some(DEFAULT_AGENT_API_DOC) => {
            candidates.push(DEFAULT_AGENT_API_DOC);
            candidates.push(LEGACY_AGENT_SKILL_DOC);
        }
        Some(LEGACY_AGENT_SKILL_DOC) => {
            candidates.push(LEGACY_AGENT_SKILL_DOC);
            candidates.push(DEFAULT_AGENT_API_DOC);
        }
        Some(skill_file) => candidates.push(skill_file),
        None => {
            candidates.push(DEFAULT_AGENT_API_DOC);
            candidates.push(LEGACY_AGENT_SKILL_DOC);
        }
    }

    candidates
        .into_iter()
        .find_map(|skill_file| std::fs::read_to_string(world_dir.join(skill_file)).ok())
}

// =============================================================================
// Credentials
// =============================================================================

#[derive(Serialize, Deserialize)]
struct Credentials {
    api_key: String,
    server: String,
}

fn credentials_path() -> PathBuf {
    clawblox_paths::credentials_path().unwrap_or_else(|err| {
        eprintln!("Error: {err}");
        std::process::exit(1);
    })
}

fn load_credentials() -> Option<Credentials> {
    let path = credentials_path();
    let content = std::fs::read_to_string(&path).ok()?;
    toml::from_str(&content).ok()
}

fn save_credentials(creds: &Credentials) {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("Failed to create ~/.clawblox directory");
    }
    let content = toml::to_string_pretty(creds).expect("Failed to serialize credentials");
    std::fs::write(&path, content).expect("Failed to write credentials file");
}

// =============================================================================
// Login Command
// =============================================================================

fn login(name: Option<String>, api_key: Option<String>, server: String) {
    if let Some(key) = api_key {
        save_credentials(&Credentials {
            api_key: key,
            server: server.clone(),
        });
        println!("API key saved to {}", credentials_path().display());
        return;
    }

    let name = name.unwrap_or_else(|| {
        eprintln!("Error: Provide a name to register, or use --api-key to store an existing key");
        eprintln!("  clawblox login my-name");
        eprintln!("  clawblox login --api-key clawblox_xxx");
        std::process::exit(1);
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let client = reqwest::Client::new();
        let url = format!("{}/api/v1/agents/register", server);

        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "name": name,
                "description": format!("{}'s world developer account", name),
            }))
            .send()
            .await
            .unwrap_or_else(|e| {
                eprintln!("Error connecting to {}: {}", server, e);
                std::process::exit(1);
            });

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();

            // Idempotent login behavior: if name already exists, reuse existing credentials
            // for this server when possible.
            if status == reqwest::StatusCode::CONFLICT {
                if let Some(stored) = load_credentials() {
                    if stored.server == server {
                        let me_url = format!("{}/api/v1/agents/me", server);
                        let me_resp = client
                            .get(&me_url)
                            .header("Authorization", format!("Bearer {}", stored.api_key))
                            .send()
                            .await;

                        if let Ok(me_resp) = me_resp {
                            if me_resp.status().is_success() {
                                let me_body: serde_json::Value =
                                    me_resp.json().await.unwrap_or_default();
                                let stored_name =
                                    me_body["name"].as_str().unwrap_or_default().to_string();
                                if stored_name == name {
                                    println!("Already registered as: {}", name);
                                    println!(
                                        "Using existing API key from {}",
                                        credentials_path().display()
                                    );
                                    return;
                                }
                            }
                        }
                    }
                }

                eprintln!("Registration failed ({}): {}", status, body);
                eprintln!(
                    "Name '{}' already exists. Re-run with --api-key or use a different name.",
                    name
                );
                std::process::exit(1);
            }

            eprintln!("Registration failed ({}): {}", status, body);
            std::process::exit(1);
        }

        let body: serde_json::Value = resp.json().await.unwrap_or_else(|e| {
            eprintln!("Error parsing response: {}", e);
            std::process::exit(1);
        });

        let key = body["agent"]["api_key"]
            .as_str()
            .expect("Missing api_key in response");
        let claim_url = body["agent"]["claim_url"]
            .as_str()
            .expect("Missing claim_url in response");

        save_credentials(&Credentials {
            api_key: key.to_string(),
            server: server.clone(),
        });

        println!("Registered as: {}", name);
        println!("API key saved to {}", credentials_path().display());
        println!();
        println!("Claim your account: {}", claim_url);
        if let Some(code) = body["agent"]["verification_code"].as_str() {
            println!("Verification code: {}", code);
        }
    });
}

// =============================================================================
// Deploy Command
// =============================================================================

fn deploy_game(path: PathBuf, api_key_override: Option<String>, server_override: Option<String>) {
    let path = std::fs::canonicalize(&path).unwrap_or_else(|_| {
        eprintln!("Error: Path '{}' does not exist", path.display());
        std::process::exit(1);
    });

    // Resolve credentials: flag > env > stored
    let stored = load_credentials();
    let api_key = api_key_override
        .or_else(|| stored.as_ref().map(|c| c.api_key.clone()))
        .unwrap_or_else(|| {
            eprintln!("Error: No API key found. Run `clawblox login` first, or pass --api-key");
            std::process::exit(1);
        });
    let server = server_override
        .or_else(|| stored.as_ref().map(|c| c.server.clone()))
        .unwrap_or_else(|| "https://clawblox.com".to_string());

    // Load world config + bundled runtime script (main.lua + optional scripts/ tree)
    let (config, script_code) = load_world_and_entry_script(&path).unwrap_or_else(|e| {
        eprintln!("{}", e);
        std::process::exit(1);
    });

    let skill_md = read_agent_api_doc(&path, &config);

    // Check for existing game_id
    let game_id_path = path.join(".clawblox/game_id");
    let existing_game_id = std::fs::read_to_string(&game_id_path)
        .ok()
        .and_then(|s| s.trim().parse::<Uuid>().ok());

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let client = reqwest::Client::new();

        let game_id = if let Some(id) = existing_game_id {
            // Update existing world.
            println!("Updating world {}...", id);
            let url = format!("{}/api/v1/games/{}", server, id);
            let resp = client
                .put(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&serde_json::json!({
                    "name": config.name,
                    "description": config.description,
                    "game_type": "lua",
                    "script_code": script_code,
                    "skill_md": skill_md,
                    "max_players": config.max_players as i32,
                    "observation_radius": config.observation.radius,
                }))
                .send()
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Error connecting to {}: {}", server, e);
                    std::process::exit(1);
                });

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("Update failed ({}): {}", status, body);
                std::process::exit(1);
            }

            id
        } else {
            // Create new world.
            println!("Creating new world...");
            let url = format!("{}/api/v1/games", server);
            let resp = client
                .post(&url)
                .header("Authorization", format!("Bearer {}", api_key))
                .json(&serde_json::json!({
                    "name": config.name,
                    "description": config.description.unwrap_or_default(),
                    "game_type": "lua",
                    "script_code": script_code,
                    "skill_md": skill_md,
                    "max_players": config.max_players as i32,
                    "observation_radius": config.observation.radius,
                }))
                .send()
                .await
                .unwrap_or_else(|e| {
                    eprintln!("Error connecting to {}: {}", server, e);
                    std::process::exit(1);
                });

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                eprintln!("Deploy failed ({}): {}", status, body);
                std::process::exit(1);
            }

            let body: serde_json::Value = resp.json().await.unwrap_or_else(|e| {
                eprintln!("Error parsing response: {}", e);
                std::process::exit(1);
            });

            let id_str = body["game_id"]
                .as_str()
                .expect("Missing game_id in response");
            id_str.parse::<Uuid>().expect("Invalid game_id in response")
        };

        // Save game_id for future deploys
        let clawblox_dir = path.join(".clawblox");
        std::fs::create_dir_all(&clawblox_dir).expect("Failed to create .clawblox directory");
        std::fs::write(game_id_path, game_id.to_string()).expect("Failed to save game_id");

        // Upload assets if assets/ directory exists and has files
        let assets_dir = path.join("assets");
        if assets_dir.is_dir() {
            let entries: Vec<_> = std::fs::read_dir(&assets_dir)
                .map(|rd| rd.filter_map(|e| e.ok()).collect())
                .unwrap_or_default();

            if !entries.is_empty() {
                println!("Uploading assets...");

                let enc = GzEncoder::new(Vec::new(), Compression::default());
                let mut tar_builder = tar::Builder::new(enc);
                tar_builder
                    .append_dir_all(".", &assets_dir)
                    .unwrap_or_else(|e| {
                        eprintln!("Error creating asset archive: {}", e);
                        std::process::exit(1);
                    });
                let enc = tar_builder.into_inner().unwrap_or_else(|e| {
                    eprintln!("Error finalizing tar: {}", e);
                    std::process::exit(1);
                });
                let targz_bytes = enc.finish().unwrap_or_else(|e| {
                    eprintln!("Error finalizing gzip: {}", e);
                    std::process::exit(1);
                });

                let url = format!("{}/api/v1/games/{}/assets", server, game_id);
                let resp = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {}", api_key))
                    .header("Content-Type", "application/gzip")
                    .body(targz_bytes)
                    .send()
                    .await
                    .unwrap_or_else(|e| {
                        eprintln!("Error uploading assets: {}", e);
                        std::process::exit(1);
                    });

                if resp.status().is_success() {
                    let body: serde_json::Value = resp.json().await.unwrap_or_default();
                    let count = body["uploaded"].as_u64().unwrap_or(0);
                    let version = body["version"].as_i64().unwrap_or(0);
                    println!("Uploaded {} asset(s) (v{})", count, version);
                } else {
                    let status = resp.status();
                    let body = resp.text().await.unwrap_or_default();
                    eprintln!("Warning: Asset upload failed ({}): {}", status, body);
                }
            }
        }

        println!("World deployed: {}/game/{}", server, game_id);
    });
}

// =============================================================================
// File Locking
// =============================================================================

/// Acquire an exclusive advisory lock (blocking) on an open file.
#[cfg(unix)]
fn flock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn flock_exclusive(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

// =============================================================================
// PID File Management
// =============================================================================

fn pid_dir() -> PathBuf {
    clawblox_paths::pid_dir().unwrap_or_else(|err| {
        eprintln!("Error: {err}");
        std::process::exit(1);
    })
}

fn pid_path(port: u16) -> PathBuf {
    pid_dir().join(format!("port-{}.pid", port))
}

fn read_pid_file(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

fn delete_pid_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    // signal 0 checks if process exists without sending a signal
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn is_process_alive(pid: u32) -> bool {
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {}", pid), "/NH"])
        .output()
        .map(|o| {
            let out = String::from_utf8_lossy(&o.stdout);
            out.contains(&pid.to_string())
        })
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_process(pid: u32) {
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(windows)]
fn kill_process(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output();
}

/// If a previous clawblox process is running on this port, kill it and wait for it to exit.
fn kill_previous_instance(port: u16) {
    let path = pid_path(port);
    if let Some(old_pid) = read_pid_file(&path) {
        if is_process_alive(old_pid) {
            eprintln!(
                "Stopping previous clawblox instance (PID {}) on port {}...",
                old_pid, port
            );
            kill_process(old_pid);
            // Wait up to 2 seconds for the process to exit
            for _ in 0..20 {
                if !is_process_alive(old_pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if is_process_alive(old_pid) {
                eprintln!(
                    "Warning: Previous process (PID {}) did not exit in time",
                    old_pid
                );
            }
        }
        // Remove stale PID file
        delete_pid_file(&path);
    }
}

/// Wait for SIGINT (Ctrl+C) or SIGTERM (Unix only).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

// =============================================================================
// Run Command
// =============================================================================

fn write_pid_file_for(path: &Path, pid: u32) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, pid.to_string());
}

#[derive(Debug, Clone, Serialize)]
struct LocalRendererManifest {
    schema_version: u32,
    api_version: u32,
    name: String,
    mode: String,
    entry_url: Option<String>,
    capabilities: Vec<String>,
}

fn default_renderer_manifest() -> LocalRendererManifest {
    LocalRendererManifest {
        schema_version: 1,
        api_version: 1,
        name: String::new(),
        mode: "none".to_string(),
        entry_url: None,
        capabilities: Vec::new(),
    }
}

fn normalize_base_path(base_path: &str) -> String {
    let trimmed = base_path.trim();
    if trimmed.is_empty() || trimmed == "/" {
        return "/".to_string();
    }

    let with_leading = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{}", trimmed)
    };

    let normalized = with_leading.trim_end_matches('/');
    if normalized.is_empty() {
        "/".to_string()
    } else {
        normalized.to_string()
    }
}

fn join_base_path(base_path: &str, route: &str) -> String {
    let normalized = normalize_base_path(base_path);
    let route = if route.starts_with('/') {
        route.to_string()
    } else {
        format!("/{}", route)
    };
    if normalized == "/" {
        route
    } else {
        format!("{}{}", normalized, route)
    }
}

fn mount_router_at_base_path<S>(router: Router<S>, base_path: &str) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let normalized = normalize_base_path(base_path);
    if normalized == "/" {
        router
    } else {
        Router::new().nest(&normalized, router)
    }
}

#[derive(Debug, Deserialize)]
struct RendererPackageJson {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    scripts: HashMap<String, String>,
    #[serde(rename = "clawbloxRenderer", default)]
    clawblox_renderer: Option<RendererPackageMetadata>,
}

#[derive(Debug, Deserialize)]
struct RendererPackageMetadata {
    #[serde(rename = "apiVersion", default)]
    api_version: Option<u32>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(rename = "outFile", default)]
    out_file: Option<String>,
}

fn sanitize_renderer_entry(raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') {
        return None;
    }
    if normalized
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return None;
    }
    Some(normalized)
}

fn normalize_renderer_rel_path(raw: &str) -> Option<String> {
    let mut normalized = raw.replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }
    sanitize_renderer_entry(&normalized)
}

fn read_renderer_package(game_dir: &Path) -> Result<Option<RendererPackageJson>, String> {
    let package_json_path = game_dir.join("renderer").join("package.json");
    if !package_json_path.is_file() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(&package_json_path)
        .map_err(|e| format!("Failed to read {}: {}", package_json_path.display(), e))?;
    let parsed: RendererPackageJson = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {}", package_json_path.display(), e))?;
    Ok(Some(parsed))
}

fn detect_renderer_source_entry(game_dir: &Path) -> Option<String> {
    let candidates = [
        "src/index.ts",
        "src/index.tsx",
        "src/index.js",
        "src/index.jsx",
        "index.ts",
        "index.tsx",
        "index.js",
        "index.jsx",
    ];
    for c in candidates {
        let p = game_dir.join("renderer").join(c);
        if p.is_file() {
            return Some(c.to_string());
        }
    }
    None
}

fn command_exists(bin: &str) -> bool {
    Command::new(bin).arg("--version").output().is_ok()
}

fn bundle_renderer_with_esbuild(
    game_dir: &Path,
    source_rel: &str,
    out_rel: &str,
) -> Result<(), String> {
    let renderer_dir = game_dir.join("renderer");
    let source = renderer_dir.join(source_rel);
    let out = renderer_dir.join(out_rel);

    let out_parent = out
        .parent()
        .ok_or_else(|| format!("Invalid renderer bundle output path: {}", out.display()))?;
    std::fs::create_dir_all(out_parent).map_err(|e| {
        format!(
            "Failed to create renderer build dir {}: {}",
            out_parent.display(),
            e
        )
    })?;

    let mut needs_npm_cache = false;
    let mut cmd = if command_exists("esbuild") {
        let mut c = Command::new("esbuild");
        c.arg(source.as_os_str());
        c
    } else if command_exists("npx") {
        needs_npm_cache = true;
        let mut c = Command::new("npx");
        c.args(["--yes", "esbuild"]);
        c.arg(source.as_os_str());
        c
    } else {
        return Err(
            "Could not find esbuild or npx in PATH. Install esbuild (npm i -D esbuild) or Node.js."
                .to_string(),
        );
    };

    cmd.current_dir(game_dir).args([
        "--bundle",
        "--format=esm",
        "--platform=browser",
        "--target=es2020",
        "--sourcemap",
        "--log-level=warning",
    ]);

    if needs_npm_cache {
        let npm_cache_dir = std::env::temp_dir().join("clawblox-npm-cache");
        std::fs::create_dir_all(&npm_cache_dir).map_err(|e| {
            format!(
                "Failed to create npm cache dir {}: {}",
                npm_cache_dir.display(),
                e
            )
        })?;
        cmd.env("NPM_CONFIG_CACHE", &npm_cache_dir);
    }

    let output = cmd
        .arg(format!("--outfile={}", out.display()))
        .output()
        .map_err(|e| format!("Failed to spawn esbuild: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "esbuild failed bundling {} -> {}.\nstdout:\n{}\nstderr:\n{}",
            source.display(),
            out.display(),
            stdout,
            stderr
        ));
    }

    Ok(())
}

fn bundle_renderer_package_with_esbuild(
    game_dir: &Path,
    source_rel: &str,
    out_rel: &str,
) -> Result<(), String> {
    let renderer_dir = game_dir.join("renderer");
    let source = renderer_dir.join(source_rel);
    if !source.is_file() {
        return Err(format!(
            "Renderer package entry '{}' not found at {}",
            source_rel,
            source.display()
        ));
    }
    bundle_renderer_with_esbuild(game_dir, source_rel, out_rel)
}

fn renderer_output_contains_create_renderer(output_path: &Path) -> Result<bool, String> {
    if !output_path.is_file() {
        return Ok(false);
    }

    let built_source = std::fs::read_to_string(output_path).map_err(|e| {
        format!(
            "Failed reading renderer output {}: {}",
            output_path.display(),
            e
        )
    })?;

    Ok(built_source.contains("createRenderer"))
}

fn maybe_build_renderer_package(
    game_dir: &Path,
    config: &clawblox::config::RendererConfig,
    base_path: &str,
) -> Result<LocalRendererManifest, String> {
    let Some(package) = read_renderer_package(game_dir)? else {
        return Err(
            "Renderer package required: missing renderer/package.json. Run `clawblox init` for package scaffold or add a renderer package.".to_string(),
        );
    };

    let metadata = package
        .clawblox_renderer
        .unwrap_or(RendererPackageMetadata {
            api_version: None,
            name: None,
            entry: None,
            capabilities: vec![],
            out_file: None,
        });

    let entry_rel = metadata
        .entry
        .as_deref()
        .and_then(normalize_renderer_rel_path)
        .or_else(|| detect_renderer_source_entry(game_dir))
        .ok_or_else(|| {
            "Renderer package has no detectable entry. Set renderer/package.json -> clawbloxRenderer.entry (e.g. ./src/index.ts).".to_string()
        })?;

    let out_rel = metadata
        .out_file
        .as_deref()
        .and_then(normalize_renderer_rel_path)
        .unwrap_or_else(|| "dist/renderer.bundle.js".to_string());

    let renderer_dir = game_dir.join("renderer");

    // Acquire an exclusive lock so concurrent instances don't race on the build.
    let lock_path = renderer_dir.join(".build.lock");
    let lock_file = std::fs::File::create(&lock_path)
        .map_err(|e| format!("Failed to create renderer build lock: {}", e))?;
    flock_exclusive(&lock_file)
        .map_err(|e| format!("Failed to acquire renderer build lock: {}", e))?;

    let output_path = renderer_dir.join(&out_rel);
    let build_result = if package.scripts.contains_key("build") {
        if !command_exists("npm") {
            Err(
                "Renderer package declares a build script, but npm is not available in PATH."
                    .to_string(),
            )
        } else {
            let output = Command::new("npm")
                .args(["run", "build"])
                .current_dir(&renderer_dir)
                .output()
                .map_err(|e| format!("Failed to run renderer build script: {}", e))?;
            if output.status.success() {
                Ok(())
            } else {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!(
                    "Renderer package build failed (npm run build)\nstdout:\n{}\nstderr:\n{}",
                    stdout, stderr
                ))
            }
        }
    } else {
        bundle_renderer_package_with_esbuild(game_dir, &entry_rel, &out_rel)
    };

    if let Err(build_error) = build_result {
        if renderer_output_contains_create_renderer(&output_path)? {
            eprintln!(
                "Warning: renderer rebuild failed; using existing bundle at {}",
                output_path.display()
            );
            eprintln!("Reason: {}", build_error);
        } else {
            drop(lock_file);
            return Err(build_error);
        }
    }

    drop(lock_file);

    if !output_path.is_file() {
        return Err(format!(
            "Renderer build did not produce expected output: {}",
            output_path.display()
        ));
    }

    if !renderer_output_contains_create_renderer(&output_path)? {
        return Err(format!(
            "Renderer output {} does not appear to export createRenderer.",
            output_path.display()
        ));
    }

    println!(
        "Renderer package bundle: renderer/{} -> renderer/{}",
        entry_rel, out_rel
    );

    let name = metadata
        .name
        .or(package.name)
        .unwrap_or_else(|| config.name.clone());
    let api_version = metadata.api_version.unwrap_or(config.api_version);
    let capabilities = if metadata.capabilities.is_empty() {
        config.capabilities.clone()
    } else {
        metadata.capabilities
    };

    Ok(LocalRendererManifest {
        schema_version: 1,
        api_version,
        name,
        mode: "module".to_string(),
        entry_url: Some(join_base_path(
            base_path,
            &format!("/renderer-files/{}", out_rel),
        )),
        capabilities,
    })
}

fn run_game(
    path: PathBuf,
    port: u16,
    base_path: &str,
    daemon: bool,
    datastore: LocalDatastore,
    record: bool,
    record_dir: Option<PathBuf>,
    resume: Option<PathBuf>,
    skip_run_command: bool,
    agent_options: Option<AgentRunnerOptions>,
) {
    let base_path = normalize_base_path(base_path);
    let source = path.to_string_lossy().to_string();
    let reporter = CliWorldSourceReporter::new();
    let resolved = world_source::resolve_world_source_with_reporter(&source, &reporter)
        .unwrap_or_else(|err| {
            eprintln!("Error: {err}");
            std::process::exit(1);
        });
    let path = resolved.dir;

    let world_cfg = clawblox::config::WorldConfig::from_game_dir(&path).unwrap_or_else(|e| {
        eprintln!("{}", e);
        std::process::exit(1);
    });

    if !skip_run_command {
        if let Some(command) = world_cfg.run.command.as_ref() {
            if base_path != "/" {
                eprintln!(
                    "Error: --base-path is not supported when world.toml [run].command is configured."
                );
                std::process::exit(1);
            }
            if daemon {
                eprintln!(
                    "Error: world.toml [run].command is not compatible with daemon mode (`clawblox start`). Use `clawblox run` instead."
                );
                std::process::exit(1);
            }
            run_external_world_command(
                &path,
                command,
                port,
                record,
                record_dir.as_ref(),
                resume.as_ref(),
            );
            return;
        }
    }

    if skip_run_command && world_cfg.run.command.is_some() {
        eprintln!("Skipping world.toml [run].command due to CLAWBLOX_SKIP_RUN_COMMAND");
    }

    if world_cfg.runtime.mode == "external" {
        if agent_options.is_some() {
            eprintln!("Error: --agent is not supported for external runtime worlds yet.");
            std::process::exit(1);
        }
        run_external_runtime(path, port, &base_path, daemon, &world_cfg);
        return;
    }

    if daemon {
        let pid_file = pid_path(port);
        write_pid_file_for(&pid_file, std::process::id());
    }

    // Load world config + bundled runtime script (main.lua + optional scripts/ tree)
    let (config, script) = load_world_and_entry_script(&path).unwrap_or_else(|e| {
        eprintln!("{}", e);
        std::process::exit(1);
    });

    let skill_md = read_agent_api_doc(&path, &config);
    let agent_skill_md = skill_md.clone();
    let renderer_manifest = maybe_build_renderer_package(&path, &config.renderer, &base_path)
        .unwrap_or_else(|e| {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        });
    let resume_snapshot = resume.as_ref().map(|resume_path| {
        let resume_path = if resume_path.is_absolute() {
            resume_path.clone()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(resume_path)
        };
        let snapshot_content = std::fs::read_to_string(&resume_path).unwrap_or_else(|e| {
            eprintln!(
                "Error: failed to read resume snapshot {}: {}",
                resume_path.display(),
                e
            );
            std::process::exit(1);
        });
        serde_json::from_str::<LocalRuntimeSnapshot>(&snapshot_content).unwrap_or_else(|e| {
            eprintln!(
                "Error: failed to parse resume snapshot {}: {}",
                resume_path.display(),
                e
            );
            std::process::exit(1);
        })
    });

    let pid_file = pid_path(port);

    println!(
        "Starting {} (max {} players)",
        config.name, config.max_players
    );
    println!("Script: {}", config.scripts.main);
    if let Some(tree) = &config.scripts.tree {
        println!("Script tree: {}", tree);
    } else if path.join("scripts").is_dir() {
        println!("Script tree: scripts (auto-detected)");
    }
    if resume_snapshot.is_some() {
        println!("Resume: enabled");
    }

    // Create game manager with selected datastore backend.
    // Local run keeps GC disabled regardless of datastore mode.
    let (game_manager, game_handle) = match datastore {
        LocalDatastore::NoDb => {
            println!("Datastore: none (DataStoreService disabled)");
            GameManager::new_without_db(60, ErrorMode::Halt)
        }
        LocalDatastore::Postgres => {
            println!("Datastore: postgres (via DATABASE_URL)");
            let pool = tokio::runtime::Runtime::new()
                .expect("Failed to create tokio runtime")
                .block_on(async { db::create_pool().await })
                .unwrap_or_else(|e| {
                    eprintln!("Error: failed to connect to postgres: {}", e);
                    eprintln!("Hint: set DATABASE_URL or use --datastore sqlite");
                    std::process::exit(1);
                });
            let bridge = Arc::new(AsyncBridge::from_postgres(Arc::new(pool)));
            GameManager::new_with_bridge(60, Some(bridge), ErrorMode::Halt, true)
        }
        LocalDatastore::Sqlite => {
            let db_name = format!("local-{}.db", port);
            let db_path = path.join(".clawblox").join(db_name);
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                    eprintln!("Error: failed to create local datastore directory: {}", e);
                    std::process::exit(1);
                });
            }
            println!("Datastore: sqlite ({})", db_path.display());
            let pool = tokio::runtime::Runtime::new()
                .expect("Failed to create tokio runtime")
                .block_on(async {
                    let pool = db::create_sqlite_pool(&db_path).await?;
                    db::ensure_local_datastore_schema(&pool).await?;
                    Ok::<_, sqlx::Error>(pool)
                })
                .unwrap_or_else(|e| {
                    eprintln!("Error: failed to initialize sqlite datastore: {}", e);
                    std::process::exit(1);
                });
            let bridge = Arc::new(AsyncBridge::from_sqlite(Arc::new(pool)));
            GameManager::new_with_bridge(60, Some(bridge), ErrorMode::Halt, true)
        }
    };

    let game_id = resume_snapshot
        .as_ref()
        .map(|snapshot| snapshot.game_id)
        .unwrap_or_else(Uuid::new_v4);

    let instance_id = if let Some(snapshot) = resume_snapshot.as_ref() {
        let mut instance = clawblox::game::instance::GameInstance::new_with_config(
            game_id,
            config.max_players,
            config.observation.radius,
            game_handle.async_bridge.clone(),
            ErrorMode::Halt,
        );
        instance.set_player_object_physics(config.clawblox.player_object_physics);
        instance.set_physics_config(&config.clawblox.physics);
        instance.instance_id = snapshot.instance.instance_id;
        instance.init_runtime();
        clawblox::snapshot::restore_game_instance(&mut instance, &snapshot.instance)
            .unwrap_or_else(|e| {
                eprintln!("Error: failed to restore snapshot state: {}", e);
                std::process::exit(1);
            });
        game::insert_instance(&game_handle, instance)
    } else {
        let result = find_or_create_instance(
            &game_handle,
            game_id,
            config.max_players,
            config.observation.radius,
            Some(&script),
        );
        if let Some(handle) = game_handle.instances.get(&result.instance_id) {
            let mut instance = handle.write();
            instance.set_player_object_physics(config.clawblox.player_object_physics);
            instance.set_physics_config(&config.clawblox.physics);
        }
        result.instance_id
    };

    // Apply AFK timeout from world config (0 = disabled)
    if let Some(handle) = game_handle.instances.get(&instance_id) {
        let mut instance = handle.write();
        apply_afk_timeout(&mut instance, config.afk_timeout);
    }

    let initial_instance_snapshot = {
        let handle = game_handle.instances.get(&instance_id).unwrap_or_else(|| {
            eprintln!("Error: missing instance {} after startup", instance_id);
            std::process::exit(1);
        });
        let instance = handle.read();
        Arc::new(
            clawblox::snapshot::capture_game_instance(&instance).unwrap_or_else(|e| {
                eprintln!("Error: failed to capture initial instance snapshot: {}", e);
                std::process::exit(1);
            }),
        )
    };

    println!("Instance: {}", instance_id);

    // Run game loop in background thread
    thread::spawn(move || {
        game_manager.run();
    });

    // Start HTTP server
    let rt = tokio::runtime::Runtime::new().unwrap();
    let resume_snapshot_for_server = resume_snapshot.clone();
    rt.block_on(async {
        let mut recording_manifest_target: Option<(PathBuf, PathBuf)> = None;
        let recording_writer = if record {
            let rec_dir = if let Some(dir) = record_dir.as_ref() {
                if dir.is_absolute() {
                    dir.clone()
                } else {
                    path.join(dir)
                }
            } else {
                path.join("recordings")
            };
            let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
            let rec_path = rec_dir.join(format!("{}.clawrec", timestamp));
            match recording::RecordingWriter::create(
                &rec_path,
                &config.name,
                &path.to_string_lossy(),
                60,
            )
            .await
            {
                Ok(writer) => {
                    println!("Recording to: {}", rec_path.display());
                    if let Some(state_root) = rec_dir.parent() {
                        let manifest_path = manifest::manifest_path_for_state_root(state_root);
                        let _ = manifest::record_recording(&manifest_path, &rec_path, "clawrec");
                        recording_manifest_target = Some((manifest_path, rec_path.clone()));
                    }
                    // Replay manifest for the actuate run contract: declares
                    // the recording and how to replay it (see actuate's Runs
                    // docs). {recording}/{port} are substituted by the app.
                    let replay_manifest = format!(
                        "version = 1\nformat = \"clawrec\"\nrecording = \"{}\"\n\ncommand = [\"clawblox\", \"replay\", \"{{recording}}\", \"--port\", \"{{port}}\"]\n",
                        rec_path
                            .file_name()
                            .map(|name| name.to_string_lossy().to_string())
                            .unwrap_or_default(),
                    );
                    if let Err(e) =
                        std::fs::write(rec_path.with_extension("replay.toml"), replay_manifest)
                    {
                        eprintln!("Warning: failed to write replay manifest: {}", e);
                    }
                    Some(Arc::new(tokio::sync::Mutex::new(writer)))
                }
                Err(e) => {
                    eprintln!("Warning: failed to start recording: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let (audio_tx, _) = broadcast::channel::<AudioChunk>(64);
        let (speech_tx, _) = broadcast::channel::<SpeechEvent>(64);
        let (speech_progress_tx, _) = broadcast::channel::<SpeechProgressEvent>(128);
        let (playback_done_tx, _) = broadcast::channel::<PlaybackDoneEvent>(64);
        let (chat_tx, _) = broadcast::channel::<LocalChatMessage>(128);
        let spectator_token = resolve_spectator_token();
        let capture = config.agent_api.vision_observation.then(|| {
            Arc::new(CaptureManager::new(format!(
                "http://127.0.0.1:{}{}?viewport=1&spectator_token={}",
                port,
                join_base_path(&base_path, "/"),
                spectator_token
            )))
        });
        let vision_enabled = capture.is_some();
        let state = LocalState {
            game_id,
            instance_id,
            game_handle,
            initial_instance_snapshot,
            world_script_source: Arc::new(script.clone()),
            max_players: config.max_players,
            observation_radius: config.observation.radius,
            player_object_physics: config.clawblox.player_object_physics,
            physics_config: config.clawblox.physics.clone(),
            allow_reset: config.agent_api.allow_reset,
            state_observation: config.agent_api.state_observation,
            action_feedback: config.agent_api.action_feedback,
            afk_timeout_secs: config.afk_timeout,
            error_mode: ErrorMode::Halt,
            started_at: Instant::now(),
            base_path: base_path.clone(),
            spectator_token: spectator_token.clone(),
            skill_md,
            renderer_manifest,
            sessions: Arc::new(DashMap::new()),
            log_file: path.join(format!(".clawblox-{}.log", port)),
            speech_config: config.speech.clone(),
            audio_tx,
            speech_tx,
            speech_progress_tx,
            playback_done_tx,
            chat_tx,
            playback_done_seq: Arc::new(Mutex::new(0)),
            speech_seq: Arc::new(Mutex::new(0)),
            speech_progress_seq: Arc::new(Mutex::new(0)),
            authoritative_speech_event_cursor: Arc::new(Mutex::new(0)),
            audio_streams: Arc::new(Mutex::new(HashMap::new())),
            audio_speech_text: Arc::new(Mutex::new(HashMap::new())),
            chat_state: Arc::new(Mutex::new(LocalChatState::default())),
            recording_writer: recording_writer.clone(),
            recording_timeline: Arc::new(Mutex::new(RecordingTimeline::default())),
            capture,
        };

        if let Some(snapshot) = resume_snapshot_for_server {
            for session in snapshot.sessions {
                state
                    .sessions
                    .insert(session.token, (session.agent_id, session.name));
            }
            let mut chat_state = state.chat_state.lock().unwrap();
            chat_state.next_seq = snapshot.chat.next_seq;
            chat_state.messages = snapshot.chat.messages.into_iter().collect();
        }

        // Spawn recording task if recording is enabled
        let mut recording_shutdown_tx = None;
        let mut recording_task: Option<JoinHandle<()>> = None;
        if let Some(ref writer) = recording_writer {
            let writer = writer.clone();
            let game_handle = state.game_handle.clone();
            let instance_id = state.instance_id;
            let recording_base_path = state.base_path.clone();
            let recording_timeline = state.recording_timeline.clone();
            let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
            recording_shutdown_tx = Some(shutdown_tx);
            recording_task = Some(tokio::spawn(async move {
                let mut tick_interval = interval(Duration::from_millis(16));
                let mut last_obs_key: Option<(u64, u64)> = None;
                let mut base_observation: Option<SpectatorObservation> = None;
                let mut known_entity_ids: HashSet<u32> = HashSet::new();
                let mut replication_revision: u64 = 0;
                let start = Instant::now();
                let mut consecutive_observe_failures: u32 = 0;
                let mut total_observe_failures: u64 = 0;
                loop {
                    tokio::select! {
                        _ = shutdown_rx.changed() => {
                            if *shutdown_rx.borrow() {
                                break;
                            }
                        }
                        _ = tick_interval.tick() => {
                            if base_observation.is_none() {
                                let obs = game::get_spectator_observation_for_instance(&game_handle, instance_id);
                                match obs {
                                    Ok(mut obs) => {
                                        if consecutive_observe_failures > 0 {
                                            eprintln!(
                                                "[recording] spectator observation recovered for instance {} after {} consecutive failures ({} total)",
                                                instance_id,
                                                consecutive_observe_failures,
                                                total_observe_failures,
                                            );
                                            consecutive_observe_failures = 0;
                                        }
                                        let obs_key = (obs.tick, obs.server_time_ms);
                                        if last_obs_key != Some(obs_key) {
                                            resolve_local_spectator_assets(&mut obs, &recording_base_path);
                                            known_entity_ids = obs.entities.iter().map(|entity| entity.id).collect();
                                            replication_revision = game::replication_revision_for_instance(&game_handle, instance_id)
                                                .unwrap_or(replication_revision);
                                            let time_ms = start.elapsed().as_millis() as u64;
                                            let recording_tick = {
                                                let mut timeline = recording_timeline.lock().unwrap();
                                                timeline.note_source_tick(obs.tick)
                                            };
                                            let mut recorded_obs = obs.clone();
                                            recorded_obs.tick = recording_tick;
                                            recorded_obs.server_time_ms = time_ms;
                                            let mut w = writer.lock().await;
                                            w.record_tick(recording_tick, time_ms, &recorded_obs).await;
                                            last_obs_key = Some(obs_key);
                                            base_observation = Some(obs);
                                        }
                                    }
                                    Err(err) => {
                                        consecutive_observe_failures = consecutive_observe_failures.saturating_add(1);
                                        total_observe_failures = total_observe_failures.saturating_add(1);

                                        if consecutive_observe_failures == 1 || consecutive_observe_failures % 60 == 0 {
                                            eprintln!(
                                                "[recording] spectator observation failed for instance {} (consecutive={}, total={}): {}",
                                                instance_id,
                                                consecutive_observe_failures,
                                                total_observe_failures,
                                                err,
                                            );
                                        }

                                        if consecutive_observe_failures >= 1800 {
                                            eprintln!(
                                                "[recording] stopping spectator recorder for instance {} after {} consecutive observation failures",
                                                instance_id,
                                                consecutive_observe_failures,
                                            );
                                            break;
                                        }
                                    },
                                }
                                continue;
                            }

                            let update = game::get_replication_observation_update_for_instance(
                                &game_handle,
                                instance_id,
                                &known_entity_ids,
                                replication_revision,
                            );
                            match update {
                                Ok(mut obs) => {
                                    if consecutive_observe_failures > 0 {
                                        eprintln!(
                                            "[recording] spectator observation recovered for instance {} after {} consecutive failures ({} total)",
                                            instance_id,
                                            consecutive_observe_failures,
                                            total_observe_failures,
                                        );
                                        consecutive_observe_failures = 0;
                                    }
                                    let Some(base) = base_observation.as_mut() else {
                                        continue;
                                    };
                                    if obs.observation.tick < base.tick
                                        || obs.latest_revision < replication_revision
                                    {
                                        base_observation = None;
                                        known_entity_ids.clear();
                                        replication_revision = 0;
                                        last_obs_key = None;
                                        continue;
                                    }
                                    replication_revision = obs.latest_revision;
                                    resolve_local_spectator_assets(&mut obs.observation, &recording_base_path);
                                    let obs_key = (obs.observation.tick, obs.observation.server_time_ms);
                                    if last_obs_key != Some(obs_key) {
                                        for entity in &obs.observation.entities {
                                            known_entity_ids.insert(entity.id);
                                        }
                                        for id in &obs.removed_entity_ids {
                                            known_entity_ids.remove(id);
                                        }
                                        apply_replication_update_to_observation(
                                            base,
                                            obs.observation,
                                            &obs.removed_entity_ids,
                                        );
                                        let time_ms = start.elapsed().as_millis() as u64;
                                        let recording_tick = {
                                            let mut timeline = recording_timeline.lock().unwrap();
                                            timeline.note_source_tick(base.tick)
                                        };
                                        let mut recorded_obs = base.clone();
                                        recorded_obs.tick = recording_tick;
                                        recorded_obs.server_time_ms = time_ms;
                                        let mut w = writer.lock().await;
                                        w.record_tick(recording_tick, time_ms, &recorded_obs).await;
                                        last_obs_key = Some(obs_key);
                                    }
                                }
                                Err(err) => {
                                    consecutive_observe_failures = consecutive_observe_failures.saturating_add(1);
                                    total_observe_failures = total_observe_failures.saturating_add(1);

                                    if consecutive_observe_failures == 1 || consecutive_observe_failures % 60 == 0 {
                                        eprintln!(
                                            "[recording] spectator observation failed for instance {} (consecutive={}, total={}): {}",
                                            instance_id,
                                            consecutive_observe_failures,
                                            total_observe_failures,
                                            err,
                                        );
                                    }

                                    if consecutive_observe_failures >= 1800 {
                                        eprintln!(
                                            "[recording] stopping spectator recorder for instance {} after {} consecutive observation failures",
                                            instance_id,
                                            consecutive_observe_failures,
                                        );
                                        break;
                                    }
                                },
                            }
                        }
                    }
                }
            }));
        }

        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let app = mount_router_at_base_path(Router::new()
            .route("/", get(local_ui_index))
            .route("/ui", get(local_ui_index))
            .route("/ui/{*path}", get(local_ui_file))
            .route("/spectate", get(local_spectate))
            .route("/spectate/ws", get(local_spectate_ws))
            .route("/spectate/chat/messages", get(local_spectate_chat_messages))
            .route("/renderer/manifest", get(local_renderer_manifest))
            .route("/replay/info", get(live_replay_info_probe))
            .route("/join", post(local_join))
            .route("/input", post(local_input))
            .route("/observe", get(local_observe))
            .route("/observe/image", get(local_observe_image))
            .route("/reset", post(local_reset))
            .route("/snapshot", get(local_snapshot))
            .route("/api.md", get(local_skill))
            .route("/skill.md", get(local_skill))
            .route("/audio", post(local_audio))
            .route("/chat", post(local_chat))
            .route("/chat/messages", get(local_chat_messages))
            .nest_service("/renderer-files", ServeDir::new(path.join("renderer")))
            .nest_service("/assets", ServeDir::new(path.join("assets")))
            .nest_service("/static", ServeDir::new(path.join("static")))
            .with_state(state)
            .layer(cors), &base_path);

        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        println!();
        println!("Server running on http://localhost:{}", port);
        println!("Local frontend: http://localhost:{}{}", port, join_base_path(&base_path, "/"));
        println!(
            "Spectator frontend: http://localhost:{}{}?spectator_token={}",
            port,
            join_base_path(&base_path, "/"),
            spectator_token
        );
        if !daemon {
            println!("Mode: foreground (press Ctrl+C to stop)");
        }
        println!();
        println!("Endpoints:");
        println!("  GET  /              - Embedded local frontend");
        println!("  POST /join?name=X  - Join world, returns session token");
        println!("  POST /input        - Send input (requires X-Session header)");
        if config.agent_api.state_observation {
            println!("  GET  /observe      - Player observation (requires X-Session header)");
        } else {
            println!("  GET  /observe      - Disabled by world config");
        }
        if vision_enabled {
            println!("  GET  /observe/image - First-person view as PNG (requires X-Session header)");
        }
        println!("  POST /reset        - Engine-level full reset (requires X-Session header)");
        println!("  POST /chat         - Send chat message (requires X-Session header)");
        println!("  GET  /chat/messages - List local chat messages");
        println!("  GET  /api.md       - Agent API documentation");
        println!("  GET  /skill.md     - Legacy alias for /api.md");

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                delete_pid_file(&pid_file);
                eprintln!("Error: Port {} is already in use. Try --port <PORT>", port);
                eprintln!("Details: {}", e);
                std::process::exit(1);
            });

        if let Some(agent_options) = agent_options.clone() {
            let agent_world_dir = path.clone();
            let agent_base_path = base_path.clone();
            let agent_prompt = agent_skill_md.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                match start_agent_tmux_for_local_world(
                    &agent_world_dir,
                    port,
                    &agent_base_path,
                    agent_options,
                    agent_prompt,
                ) {
                    Ok(started) => {
                        println!("Agent: {} ({})", started.name, started.kind);
                        println!("Agent tmux: {}", started.attach_command);
                        println!("Agent dir: {}", started.agent_dir.display());
                        println!("Agent command: {}", started.command_file.display());
                        println!("Agent events: {}", started.events_file.display());
                    }
                    Err(err) => {
                        eprintln!("Error: {err}");
                        std::process::exit(1);
                    }
                }
            });
        }

        let pid_file_clone = pid_file.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                eprintln!("\nShutting down...");
                delete_pid_file(&pid_file_clone);
            })
            .await
            .unwrap_or_else(|e| {
                eprintln!("Server error: {}", e);
            });

        // Finalize recording on shutdown
        if let Some(shutdown_tx) = recording_shutdown_tx {
            let _ = shutdown_tx.send(true);
        }
        if let Some(task) = recording_task {
            if let Err(err) = task.await {
                eprintln!("Warning: recording task join failed: {}", err);
            }
        }
        if let Some(writer) = recording_writer {
            let mut w = writer.lock().await;
            let info = w.close(0, 0).await;
            if let Some((manifest_path, rec_path)) = recording_manifest_target {
                if let Err(err) = manifest::record_recording_info(
                    &manifest_path,
                    &rec_path,
                    manifest::ManifestRecordingInfo {
                        first_tick: info.first_tick,
                        total_ticks: info.total_ticks,
                        tick_rate: info.tick_rate,
                        duration_ms: info.duration_ms,
                    },
                ) {
                    eprintln!("Warning: failed to update recording manifest info: {}", err);
                }
            }
            eprintln!("Recording saved.");
        }

        // Also clean up here in case shutdown_signal didn't fire
        delete_pid_file(&pid_file);
    });
}

fn run_replay(
    file: PathBuf,
    speed: f32,
    port: u16,
    base_path: &str,
    asset_base_path: Option<&str>,
    world_dir_override: Option<PathBuf>,
    agent_log_file: Option<PathBuf>,
) {
    let base_path = normalize_base_path(base_path);
    let asset_base_path = normalize_base_path(asset_base_path.unwrap_or(&base_path));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let reader = recording::RecordingReader::open(&file).await.unwrap_or_else(|e| {
            eprintln!("Error: failed to open recording: {}", e);
            std::process::exit(1);
        });
        let meta = reader.meta().await.unwrap_or_else(|e| {
            eprintln!("Error: failed to read recording metadata: {}", e);
            std::process::exit(1);
        });
        let active_ranges = reader.active_tick_ranges().await.unwrap_or_else(|e| {
            eprintln!("Error: failed to analyze replay activity: {}", e);
            std::process::exit(1);
        });

        let world_dir = world_dir_override
            .unwrap_or_else(|| PathBuf::from(&meta.world_dir));
        if !world_dir.is_dir() {
            eprintln!(
                "Warning: world directory '{}' not found. Asset serving may fail. Use --world-dir to override.",
                world_dir.display()
            );
        }

        println!("Replaying: {}", file.display());
        println!("World: {} ({})", meta.world_name, world_dir.display());
        println!("Duration: {:.1}s, {} ticks", meta.duration_ms as f64 / 1000.0, meta.total_ticks);

        let first_tick = reader.first_tick().unwrap_or_else(|| {
            eprintln!("Error: recording has no checkpoints to replay");
            std::process::exit(1);
        });
        let playback = replay::ReplayPlayback::new(
            first_tick,
            meta.total_ticks,
            meta.tick_rate,
            meta.duration_ms,
            speed,
            active_ranges,
        )
        .unwrap_or_else(|e| {
            eprintln!("Error: invalid replay playback state: {}", e);
            std::process::exit(1);
        });

        let reader = Arc::new(reader);
        let playback = Arc::new(tokio::sync::Mutex::new(playback));

        // Load renderer manifest from world_dir
        let renderer_manifest = {
            let world_toml_path = world_dir.join("world.toml");
            if world_toml_path.exists() {
                let cfg = clawblox::config::WorldConfig::from_game_dir(&world_dir).ok();
                cfg.map(|c| maybe_build_renderer_package(&world_dir, &c.renderer, &base_path).ok())
                    .flatten()
                    .unwrap_or_else(default_renderer_manifest)
            } else {
                default_renderer_manifest()
            }
        };

        let agent_timeline = if let Some(path) = agent_log_file.as_ref() {
            match load_agent_timeline(path, meta.tick_rate, first_tick) {
                Ok(timeline) => {
                    println!(
                        "Loaded agent log: {} ({} messages, {} anchors)",
                        path.display(),
                        timeline.messages.len(),
                        timeline.anchor_count
                    );
                    Some(Arc::new(timeline))
                }
                Err(err) => {
                    eprintln!("Warning: failed to load agent log '{}': {}", path.display(), err);
                    None
                }
            }
        } else {
            None
        };

        let replay_metadata = match replay_metadata::load_for_recording(&file, &meta) {
            Ok(metadata) => metadata.map(Arc::new),
            Err(err) => {
                eprintln!(
                    "Warning: failed to load replay metadata '{}': {}",
                    file.display(),
                    err
                );
                None
            }
        };

        let replay_state = ReplayState {
            reader: reader.clone(),
            playback: playback.clone(),
            base_path: base_path.clone(),
            asset_base_path: asset_base_path.clone(),
            renderer_manifest,
            agent_timeline,
            replay_metadata,
            world_dir: world_dir.clone(),
            camera: Arc::new(Mutex::new(ReplayCameraBridge::default())),
        };

        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let app = mount_router_at_base_path(Router::new()
            .route("/", get(replay_ui_index))
            .route("/ui", get(replay_ui_index))
            .route("/ui/{*path}", get(local_ui_file))
            .route("/replay/view", get(replay_view_index))
            .route("/spectate/ws", get(replay_spectate_ws))
            .route("/renderer/manifest", get(replay_renderer_manifest))
            .route("/replay/info", get(replay_info))
            .route("/replay/state", get(replay_state_at))
            .route("/replay/agent", get(replay_agent_messages))
            .route("/replay/metadata", get(replay_metadata_route))
            .route("/replay/camera", get(replay_camera_get).put(replay_camera_put))
            .route("/replay/seek", post(replay_seek))
            .route("/replay/speed", post(replay_speed))
            .route("/replay/skip-idle", post(replay_skip_idle))
            .route("/replay/pause", post(replay_pause))
            .route("/replay/play", post(replay_play))
            .route("/replay/play-range", post(replay_play_range))
            .nest_service("/renderer-files", ServeDir::new(world_dir.join("renderer")))
            .nest_service("/assets", ServeDir::new(world_dir.join("assets")))
            .nest_service("/static", ServeDir::new(world_dir.join("static")))
            .with_state(replay_state)
            .layer(cors), &base_path);

        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        println!("Replay server: http://localhost:{}{}", port, join_base_path(&base_path, "/"));

        let listener = tokio::net::TcpListener::bind(addr).await.unwrap_or_else(|e| {
            eprintln!("Error: Port {} is already in use. Try --port <PORT>", port);
            eprintln!("Details: {}", e);
            std::process::exit(1);
        });

        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                shutdown_signal().await;
                eprintln!("\nReplay server stopped.");
            })
            .await
            .unwrap_or_else(|e| {
                eprintln!("Server error: {}", e);
            });
    });
}

const REPLAY_ADAPTER_SCHEMA: &str = "clawblox.spectator.v1";

struct ReplayAdapterRecording {
    reader: recording::RecordingReader,
    meta: recording::RecordingMeta,
    first_tick: u64,
    world_dir: PathBuf,
    scene_id: String,
}

#[derive(Deserialize)]
struct JsonRpcRequest {
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Deserialize)]
struct AdapterOpenParams {
    recording_path: PathBuf,
    #[serde(default)]
    context: AdapterOpenContext,
}

#[derive(Default, Deserialize)]
struct AdapterOpenContext {
    world_dir: Option<PathBuf>,
}

#[derive(Deserialize)]
struct AdapterHandleParams {
    handle: String,
}

#[derive(Deserialize)]
struct AdapterStateAtParams {
    handle: String,
    tick: Option<u64>,
    time_ms: Option<u64>,
}

#[derive(Deserialize)]
struct AdapterDeltaAtParams {
    handle: String,
    from_tick: u64,
    tick: u64,
}

#[derive(Deserialize)]
struct AdapterEventsBetweenParams {
    handle: String,
    start_tick: Option<u64>,
    end_tick: Option<u64>,
    #[serde(default)]
    kinds: Vec<String>,
}

#[derive(Serialize)]
struct AdapterOpenResult {
    handle: String,
    info: AdapterRecordingInfo,
    scene_id: String,
    state_schema: &'static str,
}

#[derive(Serialize)]
struct AdapterRecordingInfo {
    first_tick: u64,
    last_tick: u64,
    tick_rate: u32,
    duration_ms: u64,
}

#[derive(Serialize)]
struct AdapterReplayFrame {
    tick: u64,
    time_ms: u64,
    scene_id: String,
    state: AdapterReplayFrameState,
}

#[derive(Serialize)]
struct AdapterReplayFrameState {
    schema: &'static str,
    payload: SpectatorObservation,
}

#[derive(Serialize)]
struct AdapterEventsResult {
    events: Vec<AdapterReplayEvent>,
}

#[derive(Serialize)]
struct AdapterReplayEvent {
    id: String,
    kind: String,
    tick: u64,
    time_ms: u64,
    payload: serde_json::Value,
}

#[derive(Serialize)]
struct AdapterAssetsResult {
    assets: Vec<AdapterAsset>,
}

#[derive(Serialize)]
struct AdapterAsset {
    id: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scene_id: Option<String>,
}

fn run_replay_adapter() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut recordings: HashMap<String, ReplayAdapterRecording> = HashMap::new();
    let mut next_handle: u64 = 1;

    for line in stdin.lock().lines() {
        let response = match line {
            Ok(line) if line.trim().is_empty() => continue,
            Ok(line) => match serde_json::from_str::<JsonRpcRequest>(line.trim()) {
                Ok(request) => {
                    let id = request.id.clone();
                    match rt.block_on(handle_replay_adapter_request(
                        request,
                        &mut recordings,
                        &mut next_handle,
                    )) {
                        Ok(result) => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": result,
                        }),
                        Err(message) => serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32000,
                                "message": message,
                            },
                        }),
                    }
                }
                Err(err) => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": -32700,
                        "message": format!("invalid JSON-RPC request: {}", err),
                    },
                }),
            },
            Err(err) => {
                eprintln!("replay adapter stdin error: {}", err);
                break;
            }
        };

        if serde_json::to_writer(&mut stdout, &response).is_err() {
            break;
        }
        if stdout.write_all(b"\n").is_err() || stdout.flush().is_err() {
            break;
        }
    }
}

async fn handle_replay_adapter_request(
    request: JsonRpcRequest,
    recordings: &mut HashMap<String, ReplayAdapterRecording>,
    next_handle: &mut u64,
) -> Result<serde_json::Value, String> {
    match request.method.as_str() {
        "open" => {
            let params: AdapterOpenParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid open params: {}", err))?;
            let reader = recording::RecordingReader::open(&params.recording_path)
                .await
                .map_err(|err| format!("failed to open recording: {}", err))?;
            let meta = reader
                .meta()
                .await
                .map_err(|err| format!("failed to read recording metadata: {}", err))?;
            let first_tick = reader
                .first_tick()
                .ok_or_else(|| "recording has no checkpoints".to_string())?;
            let world_dir = params
                .context
                .world_dir
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| PathBuf::from(&meta.world_dir));
            let scene_id = clawblox_replay_scene_id(&meta);
            let handle = format!("clawrec-{}", *next_handle);
            *next_handle += 1;
            let result = AdapterOpenResult {
                handle: handle.clone(),
                info: AdapterRecordingInfo {
                    first_tick,
                    last_tick: meta.total_ticks,
                    tick_rate: meta.tick_rate,
                    duration_ms: meta.duration_ms,
                },
                scene_id: scene_id.clone(),
                state_schema: REPLAY_ADAPTER_SCHEMA,
            };
            recordings.insert(
                handle,
                ReplayAdapterRecording {
                    reader,
                    meta,
                    first_tick,
                    world_dir,
                    scene_id,
                },
            );
            serde_json::to_value(result).map_err(|err| err.to_string())
        }
        "state_at" => {
            let params: AdapterStateAtParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid state_at params: {}", err))?;
            let recording = recordings
                .get(&params.handle)
                .ok_or_else(|| format!("unknown replay adapter handle: {}", params.handle))?;
            let tick = adapter_requested_tick(recording, params.tick, params.time_ms)?;
            let obs = recording
                .reader
                .get_state_at(tick)
                .await
                .map_err(|err| format!("failed to load state at tick {}: {}", tick, err))?;
            let frame = AdapterReplayFrame {
                tick: obs.tick,
                time_ms: obs.server_time_ms,
                scene_id: recording.scene_id.clone(),
                state: AdapterReplayFrameState {
                    schema: REPLAY_ADAPTER_SCHEMA,
                    payload: obs,
                },
            };
            serde_json::to_value(frame).map_err(|err| err.to_string())
        }
        "delta_at" => {
            let params: AdapterDeltaAtParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid delta_at params: {}", err))?;
            let recording = recordings
                .get(&params.handle)
                .ok_or_else(|| format!("unknown replay adapter handle: {}", params.handle))?;
            let tick = params
                .tick
                .clamp(recording.first_tick, recording.meta.total_ticks);
            let from_tick = params.from_tick.clamp(recording.first_tick, tick);
            recording
                .reader
                .get_spectator_delta_between(from_tick, tick)
                .await
                .map_err(|err| format!("failed to load delta {}..{}: {}", from_tick, tick, err))
        }
        "events_between" => {
            let params: AdapterEventsBetweenParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid events_between params: {}", err))?;
            let recording = recordings
                .get(&params.handle)
                .ok_or_else(|| format!("unknown replay adapter handle: {}", params.handle))?;
            let include_chat =
                params.kinds.is_empty() || params.kinds.iter().any(|kind| kind == "chat");
            let mut events = Vec::new();
            if include_chat {
                let start_tick = params.start_tick.unwrap_or(recording.first_tick);
                let end_tick = params.end_tick.unwrap_or(recording.meta.total_ticks);
                let messages = recording
                    .reader
                    .get_chat_messages(start_tick, end_tick)
                    .await
                    .map_err(|err| format!("failed to load chat events: {}", err))?;
                events.extend(messages.into_iter().map(|message| AdapterReplayEvent {
                    id: message.message_id.clone(),
                    kind: "chat".to_string(),
                    tick: message.tick,
                    time_ms: message.time_ms,
                    payload: serde_json::json!({
                        "id": message.message_id,
                        "agent_id": message.agent_id,
                        "agent_name": message.agent_name,
                        "content": message.content,
                        "created_at": message.created_at,
                        "delivery_mode": message.delivery_mode,
                        "hearing_radius": message.hearing_radius,
                        "heard_by": message.heard_by,
                        "audibility": message.audibility,
                    }),
                }));
            }
            serde_json::to_value(AdapterEventsResult { events }).map_err(|err| err.to_string())
        }
        "assets" => {
            let params: AdapterHandleParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid assets params: {}", err))?;
            let recording = recordings
                .get(&params.handle)
                .ok_or_else(|| format!("unknown replay adapter handle: {}", params.handle))?;
            serde_json::to_value(AdapterAssetsResult {
                assets: collect_clawblox_replay_assets(recording),
            })
            .map_err(|err| err.to_string())
        }
        "close" => {
            let params: AdapterHandleParams = serde_json::from_value(request.params)
                .map_err(|err| format!("invalid close params: {}", err))?;
            recordings.remove(&params.handle);
            Ok(serde_json::json!({ "ok": true }))
        }
        other => Err(format!("unsupported replay adapter method: {}", other)),
    }
}

fn adapter_requested_tick(
    recording: &ReplayAdapterRecording,
    tick: Option<u64>,
    time_ms: Option<u64>,
) -> Result<u64, String> {
    if let Some(tick) = tick {
        return Ok(tick.clamp(recording.first_tick, recording.meta.total_ticks));
    }
    if let Some(time_ms) = time_ms {
        let offset = time_ms.saturating_mul(recording.meta.tick_rate as u64) / 1000;
        return Ok(recording
            .first_tick
            .saturating_add(offset)
            .min(recording.meta.total_ticks));
    }
    Ok(recording.first_tick)
}

fn clawblox_replay_scene_id(meta: &recording::RecordingMeta) -> String {
    let name = if meta.world_name.trim().is_empty() {
        meta.world_dir.trim()
    } else {
        meta.world_name.trim()
    };
    format!("clawblox:{}", name)
}

fn collect_clawblox_replay_assets(recording: &ReplayAdapterRecording) -> Vec<AdapterAsset> {
    let mut assets = Vec::new();
    for root_name in ["assets", "static", "renderer"] {
        let root = recording.world_dir.join(root_name);
        collect_clawblox_replay_assets_under(
            &recording.scene_id,
            root_name,
            &root,
            &root,
            &mut assets,
        );
    }
    assets.sort_by(|a, b| a.id.cmp(&b.id));
    assets
}

fn collect_clawblox_replay_assets_under(
    scene_id: &str,
    root_name: &str,
    root: &Path,
    dir: &Path,
    out: &mut Vec<AdapterAsset>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_clawblox_replay_assets_under(scene_id, root_name, root, &path, out);
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        let url_path = format!("/{}/{}", root_name, rel);
        let size_bytes = entry.metadata().ok().map(|metadata| metadata.len());
        out.push(AdapterAsset {
            id: format!("{}:{}", root_name, rel),
            path: path.to_string_lossy().to_string(),
            url_path: Some(url_path),
            content_type: replay_adapter_content_type_for_path(&path).map(str::to_string),
            size_bytes,
            scene_id: Some(scene_id.to_string()),
        });
    }
}

fn replay_adapter_content_type_for_path(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "glb" => Some("model/gltf-binary"),
        "gltf" => Some("model/gltf+json"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "webp" => Some("image/webp"),
        "gif" => Some("image/gif"),
        "js" => Some("text/javascript"),
        "css" => Some("text/css"),
        "json" => Some("application/json"),
        "wasm" => Some("application/wasm"),
        _ => None,
    }
}

#[derive(Clone)]
struct ReplayState {
    reader: Arc<recording::RecordingReader>,
    playback: Arc<tokio::sync::Mutex<replay::ReplayPlayback>>,
    base_path: String,
    asset_base_path: String,
    renderer_manifest: LocalRendererManifest,
    agent_timeline: Option<Arc<ReplayAgentTimeline>>,
    replay_metadata: Option<Arc<ReplayMetadata>>,
    world_dir: PathBuf,
    camera: Arc<Mutex<ReplayCameraBridge>>,
}

#[derive(Serialize)]
struct ReplayMetadataResponse {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    metadata: Option<ReplayMetadata>,
}

#[derive(Default)]
struct ReplayCameraBridge {
    current: Option<serde_json::Value>,
    pending: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ReplayCameraResponse {
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<serde_json::Value>,
}

async fn replay_spectate_ws(
    State(state): State<ReplayState>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_replay_ws(socket, state))
}

async fn handle_replay_ws(socket: WebSocket, state: ReplayState) {
    let (mut sender, mut receiver) = socket.split();
    let mut tick_interval = interval(Duration::from_millis(33));
    let mut last_tick: u64 = 0;

    loop {
        tokio::select! {
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        if sender.send(Message::Pong(data)).await.is_err() { break; }
                    }
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                            if value.get("type").and_then(|v| v.as_str()) == Some("replay_camera_state") {
                                if let Some(camera_state) = value.get("state").cloned() {
                                    state.camera.lock().unwrap().current = Some(camera_state);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ = tick_interval.tick() => {
                let pending_camera = state.camera.lock().unwrap().pending.take();
                if let Some(camera_state) = pending_camera {
                    let payload = serde_json::json!({
                        "type": "replay_camera_set",
                        "state": camera_state,
                    });
                    if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                        break;
                    }
                }

                let tick = {
                    let mut pb = state.playback.lock().await;
                    let tick = pb.current_tick();
                    pb.advance_frame(Duration::from_millis(33));
                    tick
                };

                // Send chat messages that occurred between last_tick and current tick
                if tick > last_tick {
                    if let Ok(messages) = state.reader.get_chat_messages(last_tick + 1, tick).await {
                        for chat in &messages {
                            let payload = serde_json::json!({
                                "type": "chat_message",
                                "id": chat.message_id,
                                "agent_id": chat.agent_id,
                                "agent_name": chat.agent_name,
                                "message_type": "text",
                                "content": chat.content,
                                "created_at": chat.created_at,
                                "delivery_mode": chat.delivery_mode,
                                "hearing_radius": chat.hearing_radius,
                                "heard_by": chat.heard_by,
                                "audibility": chat.audibility,
                            });
                            if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                last_tick = tick;

                match state.reader.get_state_at(tick).await {
                    Ok(mut obs) => {
                        resolve_local_spectator_assets(&mut obs, &state.asset_base_path);
                        if let Ok(json) = serde_json::to_vec(&obs) {
                            let msg = Message::Text(String::from_utf8_lossy(&json).into_owned().into());
                            if sender.send(msg).await.is_err() { break; }
                        }
                    }
                    Err(err) => {
                        let message = format!("Failed to load replay state at tick {}: {}", tick, err);
                        eprintln!("{}", message);
                        let payload = serde_json::json!({
                            "type": "replay_error",
                            "tick": tick,
                            "message": message,
                        });
                        if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn replay_renderer_manifest(State(state): State<ReplayState>) -> Json<LocalRendererManifest> {
    Json(state.renderer_manifest)
}

async fn replay_info(State(state): State<ReplayState>) -> Json<replay::ReplayInfo> {
    let pb = state.playback.lock().await;
    Json(pb.info())
}

async fn live_replay_info_probe() -> Json<serde_json::Value> {
    Json(serde_json::Value::Null)
}

#[derive(Deserialize)]
struct ReplayStateQuery {
    tick: Option<u64>,
}

async fn replay_state_at(
    State(state): State<ReplayState>,
    Query(query): Query<ReplayStateQuery>,
) -> Result<Json<SpectatorObservation>, (axum::http::StatusCode, String)> {
    let tick = if let Some(tick) = query.tick {
        tick
    } else {
        let pb = state.playback.lock().await;
        pb.current_tick()
    };
    match state.reader.get_state_at(tick).await {
        Ok(mut obs) => {
            resolve_local_spectator_assets(&mut obs, &state.asset_base_path);
            Ok(Json(obs))
        }
        Err(err) => Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!("Failed to load replay state at tick {}: {}", tick, err),
        )),
    }
}

#[derive(Deserialize)]
struct ReplayAgentQuery {
    tick: Option<u64>,
}

#[derive(Serialize)]
struct ReplayAgentMessagesResponse {
    enabled: bool,
    current_tick: u64,
    start_tick: u64,
    end_tick: u64,
    messages: Vec<ReplayAgentMessage>,
}

async fn replay_agent_messages(
    State(state): State<ReplayState>,
    Query(query): Query<ReplayAgentQuery>,
) -> Json<ReplayAgentMessagesResponse> {
    let current_tick = if let Some(tick) = query.tick {
        tick
    } else {
        let pb = state.playback.lock().await;
        pb.current_tick()
    };
    let start_tick = 0;
    let end_tick = current_tick;

    let Some(timeline) = state.agent_timeline.as_ref() else {
        return Json(ReplayAgentMessagesResponse {
            enabled: false,
            current_tick,
            start_tick,
            end_tick,
            messages: Vec::new(),
        });
    };

    Json(ReplayAgentMessagesResponse {
        enabled: true,
        current_tick,
        start_tick,
        end_tick,
        messages: timeline.messages.clone(),
    })
}

async fn replay_metadata_route(State(state): State<ReplayState>) -> Json<ReplayMetadataResponse> {
    Json(ReplayMetadataResponse {
        enabled: state.replay_metadata.is_some(),
        metadata: state
            .replay_metadata
            .as_ref()
            .map(|metadata| (**metadata).clone()),
    })
}

async fn replay_camera_get(State(state): State<ReplayState>) -> Json<ReplayCameraResponse> {
    let camera = state.camera.lock().unwrap();
    Json(ReplayCameraResponse {
        enabled: camera.current.is_some(),
        state: camera.current.clone(),
    })
}

async fn replay_camera_put(
    State(state): State<ReplayState>,
    Json(camera_state): Json<serde_json::Value>,
) -> axum::http::StatusCode {
    state.camera.lock().unwrap().pending = Some(camera_state);
    axum::http::StatusCode::OK
}

#[derive(Deserialize)]
struct SeekQuery {
    tick: u64,
}

async fn replay_seek(
    State(state): State<ReplayState>,
    Query(q): Query<SeekQuery>,
) -> axum::http::StatusCode {
    let mut pb = state.playback.lock().await;
    pb.seek(q.tick);
    axum::http::StatusCode::OK
}

#[derive(Deserialize)]
struct SpeedQuery {
    value: f32,
}

async fn replay_speed(
    State(state): State<ReplayState>,
    Query(q): Query<SpeedQuery>,
) -> axum::http::StatusCode {
    let mut pb = state.playback.lock().await;
    pb.set_speed(q.value);
    axum::http::StatusCode::OK
}

#[derive(Deserialize)]
struct SkipIdleQuery {
    enabled: bool,
}

async fn replay_skip_idle(
    State(state): State<ReplayState>,
    Query(q): Query<SkipIdleQuery>,
) -> axum::http::StatusCode {
    let mut pb = state.playback.lock().await;
    pb.set_skip_idle(q.enabled);
    axum::http::StatusCode::OK
}

async fn replay_pause(State(state): State<ReplayState>) -> axum::http::StatusCode {
    let mut pb = state.playback.lock().await;
    pb.set_paused(true);
    axum::http::StatusCode::OK
}

async fn replay_play(State(state): State<ReplayState>) -> axum::http::StatusCode {
    let mut pb = state.playback.lock().await;
    pb.play();
    axum::http::StatusCode::OK
}

#[derive(Deserialize)]
struct PlayRangeQuery {
    start_tick: u64,
    end_tick: u64,
}

async fn replay_play_range(
    State(state): State<ReplayState>,
    Query(q): Query<PlayRangeQuery>,
) -> axum::http::StatusCode {
    let mut pb = state.playback.lock().await;
    pb.play_range(q.start_tick, q.end_tick);
    axum::http::StatusCode::OK
}

fn run_external_runtime(
    path: PathBuf,
    port: u16,
    base_path: &str,
    daemon: bool,
    config: &clawblox::config::WorldConfig,
) {
    let base_path = normalize_base_path(base_path);
    if daemon {
        let pid_file = pid_path(port);
        write_pid_file_for(&pid_file, std::process::id());
    }

    let skill_md = read_agent_api_doc(&path, config);
    let renderer_manifest = maybe_build_renderer_package(&path, &config.renderer, &base_path)
        .unwrap_or_else(|e| {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        });
    let pid_file = pid_path(port);

    println!("Starting {} (external runtime mode)", config.name);

    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let spectator_token = resolve_spectator_token();
        let state = ExternalLocalState {
            base_path: base_path.clone(),
            spectator_token: spectator_token.clone(),
            renderer_manifest,
            skill_md,
            started_at: Instant::now(),
        };

        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);

        let app = mount_router_at_base_path(
            Router::new()
                .route("/", get(external_ui_index))
                .route("/ui", get(external_ui_index))
                .route("/ui/{*path}", get(local_ui_file))
                .route("/renderer/manifest", get(external_renderer_manifest))
                .route("/replay/info", get(live_replay_info_probe))
                .route("/spectate", get(external_spectate))
                .route("/spectate/ws", get(external_spectate_ws))
                .route("/api.md", get(external_skill))
                .route("/skill.md", get(external_skill))
                .nest_service("/renderer-files", ServeDir::new(path.join("renderer")))
                .nest_service("/assets", ServeDir::new(path.join("assets")))
                .nest_service("/static", ServeDir::new(path.join("static")))
                .with_state(state)
                .layer(cors),
            &base_path,
        );

        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        println!();
        println!("Server running on http://localhost:{}", port);
        println!(
            "Local frontend: http://localhost:{}{}",
            port,
            join_base_path(&base_path, "/")
        );
        println!(
            "Spectator frontend: http://localhost:{}{}?spectator_token={}",
            port,
            join_base_path(&base_path, "/"),
            spectator_token
        );
        if !daemon {
            println!("Mode: foreground (press Ctrl+C to stop)");
        }
        println!();
        println!("Runtime: external (no Lua world, no /join /input /observe)");

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|e| {
                delete_pid_file(&pid_file);
                eprintln!("Error: Port {} is already in use. Try --port <PORT>", port);
                eprintln!("Details: {}", e);
                std::process::exit(1);
            });

        let pid_file_clone = pid_file.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                shutdown_signal().await;
                eprintln!("\nShutting down...");
                delete_pid_file(&pid_file_clone);
            })
            .await
            .unwrap_or_else(|e| {
                eprintln!("Server error: {}", e);
            });

        delete_pid_file(&pid_file);
    });
}

fn run_external_world_command(
    path: &Path,
    command: &[String],
    port: u16,
    record: bool,
    record_dir: Option<&PathBuf>,
    resume: Option<&PathBuf>,
) {
    println!("Delegating run to world.toml [run].command");
    println!("Command: {}", command.join(" "));
    println!(
        "Environment: WORLD_PORT={} (current dir: {})",
        port,
        path.display()
    );

    let current_exe = std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "clawblox".to_string());

    let spec = ExternalWorldCommand {
        world_dir: path.to_path_buf(),
        command: command.to_vec(),
        port,
        base_path: "/".to_string(),
        record,
        record_dir: record_dir.cloned(),
        manifest_path: None,
        resume: resume.cloned(),
        operator_token: std::env::var("WORLD_OPERATOR_TOKEN").ok(),
        clawblox_bin: Some(PathBuf::from(current_exe)),
        shm_dir: None,
    };

    let status = run_external_world_blocking(&spec).unwrap_or_else(|err| {
        eprintln!("Error: {err}");
        std::process::exit(1);
    });

    if !status.success() {
        if let Some(code) = status.code() {
            std::process::exit(code);
        }
        std::process::exit(1);
    }
}

fn start_game(path: PathBuf, port: u16, base_path: &str, datastore: LocalDatastore) {
    let base_path = normalize_base_path(base_path);
    let source = path.to_string_lossy().to_string();
    let reporter = CliWorldSourceReporter::new();
    let resolved = world_source::resolve_world_source_with_reporter(&source, &reporter)
        .unwrap_or_else(|err| {
            eprintln!("Error: {err}");
            std::process::exit(1);
        });
    let path = resolved.dir;

    let world_cfg = clawblox::config::WorldConfig::from_game_dir(&path).unwrap_or_else(|e| {
        eprintln!("{}", e);
        std::process::exit(1);
    });
    if world_cfg.run.command.is_some() {
        eprintln!(
            "Error: world.toml [run].command is not supported by `clawblox start` (daemon mode). Use `clawblox run`."
        );
        std::process::exit(1);
    }

    kill_previous_instance(port);

    let log_file_path = path.join(format!(".clawblox-{}.log", port));
    let log_file = std::fs::File::create(&log_file_path).unwrap_or_else(|e| {
        eprintln!("Error creating log file {}: {}", log_file_path.display(), e);
        std::process::exit(1);
    });
    let log_file_stderr = log_file.try_clone().unwrap_or_else(|e| {
        eprintln!("Error cloning log file handle: {}", e);
        std::process::exit(1);
    });

    let exe = std::env::current_exe().expect("Failed to get current executable path");
    let child = std::process::Command::new(exe)
        .args([
            "run",
            &path.to_string_lossy(),
            "--port",
            &port.to_string(),
            "--base-path",
            &base_path,
            "--datastore",
            datastore.as_arg(),
            "--daemon",
        ])
        .stdout(log_file)
        .stderr(log_file_stderr)
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("Error spawning daemon: {}", e);
            std::process::exit(1);
        });

    // Write the child PID to the PID file
    let pid_file = pid_path(port);
    write_pid_file_for(&pid_file, child.id());

    println!(
        "Server starting in background on http://localhost:{}{}",
        port,
        join_base_path(&base_path, "/")
    );
    println!("Logs: {}", log_file_path.display());
}

fn stop_game(port: u16) {
    let path = pid_path(port);

    let Some(pid) = read_pid_file(&path) else {
        println!("No running clawblox instance found on port {}", port);
        return;
    };

    if !is_process_alive(pid) {
        delete_pid_file(&path);
        println!(
            "No running clawblox instance found on port {} (cleaned stale PID file)",
            port
        );
        return;
    }

    println!(
        "Stopping clawblox instance (PID {}) on port {}...",
        pid, port
    );
    kill_process(pid);

    for _ in 0..20 {
        if !is_process_alive(pid) {
            delete_pid_file(&path);
            println!("Stopped.");
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    if is_process_alive(pid) {
        eprintln!("Warning: process {} did not exit in time", pid);
    } else {
        delete_pid_file(&path);
        println!("Stopped.");
    }
}

// =============================================================================
// Local Server State & Handlers
// =============================================================================

#[derive(Clone, Debug, Serialize)]
struct AudioChunk {
    speaker: String,
    stream_id: String,
    seq: u32,
    data: String, // base64 PCM
    done: bool,
}

#[derive(Clone, Debug, Serialize)]
struct SpeechEvent {
    speaker: String,
    text: String,
    seq: u64,
}

#[derive(Clone, Debug, Serialize)]
struct SpeechProgressEvent {
    speaker: String,
    stream_id: String,
    text: String,
    seq: u64,
}

#[derive(Clone, Debug, Serialize)]
struct PlaybackDoneEvent {
    speaker: String,
    stream_id: String,
    seq: u64,
}

#[derive(Deserialize)]
struct ChatPostRequest {
    content: String,
}

#[derive(Serialize)]
struct ChatPostResponse {
    id: String,
    created_at: String,
}

#[derive(Deserialize)]
struct ChatMessagesQuery {
    instance_id: Option<Uuid>,
    after: Option<String>,
    limit: Option<usize>,
    spectator_token: Option<String>,
}

#[derive(Serialize)]
struct ChatMessagesResponse {
    messages: Vec<LocalChatMessage>,
}

#[derive(Clone, Debug)]
struct AudioStreamState {
    agent_id: Uuid,
    speaker: String,
    first_seen: Instant,
    last_seen: Instant,
    total_audio_bytes: u64,
    done_received: bool,
    done_at: Option<Instant>,
    expected_end_at: Option<Instant>,
    completion_scheduled: bool,
    sample_rate_hz: u32,
    channels: u16,
    bytes_per_sample: u16,
    playback_speed: f32,
}

const LOCAL_CHAT_MAX_MESSAGES: usize = 1000;
const LOCAL_CHAT_DEFAULT_LIMIT: usize = 50;
const LOCAL_CHAT_MAX_LIMIT: usize = 100;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChatAudibilityEntry {
    agent_id: Uuid,
    agent_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    distance: Option<f32>,
    heard: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LocalChatMessage {
    id: String,
    instance_id: Uuid,
    agent_id: Uuid,
    agent_name: String,
    message_type: String,
    content: String,
    created_at: String,
    delivery_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    hearing_radius: Option<f32>,
    heard_by: Vec<Uuid>,
    audibility: Vec<ChatAudibilityEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionSnapshot {
    token: String,
    agent_id: Uuid,
    name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ChatStateSnapshot {
    next_seq: u64,
    messages: Vec<LocalChatMessage>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LocalRuntimeSnapshot {
    schema_version: u32,
    game_id: Uuid,
    instance: clawblox::snapshot::GameInstanceSnapshot,
    sessions: Vec<SessionSnapshot>,
    chat: ChatStateSnapshot,
}

#[derive(Debug, Default)]
struct LocalChatState {
    next_seq: u64,
    messages: VecDeque<LocalChatMessage>,
}

impl LocalChatState {
    fn push(
        &mut self,
        instance_id: Uuid,
        agent_id: Uuid,
        agent_name: String,
        content: String,
        delivery_mode: String,
        hearing_radius: Option<f32>,
        heard_by: Vec<Uuid>,
        audibility: Vec<ChatAudibilityEntry>,
    ) -> LocalChatMessage {
        self.next_seq = self.next_seq.saturating_add(1);
        let msg = LocalChatMessage {
            id: format!("local_{}", self.next_seq),
            instance_id,
            agent_id,
            agent_name,
            message_type: "text".to_string(),
            content,
            created_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            delivery_mode,
            hearing_radius,
            heard_by,
            audibility,
        };
        self.messages.push_back(msg.clone());
        while self.messages.len() > LOCAL_CHAT_MAX_MESSAGES {
            self.messages.pop_front();
        }
        msg
    }

    fn list(&self, instance_id: Uuid, after: Option<&str>, limit: usize) -> Vec<LocalChatMessage> {
        self.messages
            .iter()
            .filter(|m| m.instance_id == instance_id)
            .filter(|m| after.map(|a| m.created_at.as_str() > a).unwrap_or(true))
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

#[derive(Clone)]
struct LocalState {
    game_id: Uuid,
    instance_id: Uuid,
    game_handle: GameManagerHandle,
    initial_instance_snapshot: Arc<GameInstanceSnapshot>,
    world_script_source: Arc<String>,
    max_players: u32,
    observation_radius: f32,
    player_object_physics: bool,
    physics_config: PhysicsConfig,
    allow_reset: bool,
    state_observation: bool,
    action_feedback: bool,
    afk_timeout_secs: u64,
    error_mode: ErrorMode,
    started_at: Instant,
    base_path: String,
    spectator_token: String,
    skill_md: Option<String>,
    renderer_manifest: LocalRendererManifest,
    sessions: Arc<DashMap<String, (Uuid, String)>>, // token -> (agent_id, name)
    log_file: PathBuf,
    speech_config: SpeechConfig,
    audio_tx: broadcast::Sender<AudioChunk>,
    speech_tx: broadcast::Sender<SpeechEvent>,
    speech_progress_tx: broadcast::Sender<SpeechProgressEvent>,
    playback_done_tx: broadcast::Sender<PlaybackDoneEvent>,
    chat_tx: broadcast::Sender<LocalChatMessage>,
    playback_done_seq: Arc<Mutex<u64>>,
    speech_seq: Arc<Mutex<u64>>,
    speech_progress_seq: Arc<Mutex<u64>>,
    authoritative_speech_event_cursor: Arc<Mutex<u64>>,
    /// Maps stream_id -> stream state for routing and completion semantics.
    audio_streams: Arc<Mutex<HashMap<String, AudioStreamState>>>,
    /// Maps stream_id -> speech text for broadcasting on AudioEnded
    audio_speech_text: Arc<Mutex<HashMap<String, String>>>,
    chat_state: Arc<Mutex<LocalChatState>>,
    /// Session recording writer (None if not recording). Kept alive via Arc.
    #[allow(dead_code)]
    recording_writer: Option<Arc<tokio::sync::Mutex<recording::RecordingWriter>>>,
    recording_timeline: Arc<Mutex<RecordingTimeline>>,
    /// Headless-browser sidecar for agent vision (GET /observe/image).
    /// None when the world does not enable vision ([agent_api] vision = true).
    capture: Option<Arc<CaptureManager>>,
}

#[derive(Clone)]
struct ExternalLocalState {
    base_path: String,
    spectator_token: String,
    renderer_manifest: LocalRendererManifest,
    skill_md: Option<String>,
    started_at: Instant,
}

#[derive(Debug, Default)]
struct RecordingTimeline {
    epoch_offset: u64,
    last_source_tick: Option<u64>,
    latest_recording_tick: u64,
    pending_epoch_break: bool,
}

impl RecordingTimeline {
    fn note_source_tick(&mut self, source_tick: u64) -> u64 {
        let should_advance_epoch = if self.pending_epoch_break {
            self.last_source_tick
                .map(|last_source_tick| source_tick < last_source_tick)
                .unwrap_or(false)
        } else {
            self.last_source_tick
                .map(|last_source_tick| source_tick < last_source_tick)
                .unwrap_or(false)
        };

        if should_advance_epoch {
            self.epoch_offset = self.latest_recording_tick.saturating_add(1);
            self.pending_epoch_break = false;
            self.last_source_tick = None;
        }

        let recording_tick = self.epoch_offset.saturating_add(source_tick);
        self.last_source_tick = Some(source_tick);
        self.latest_recording_tick = self.latest_recording_tick.max(recording_tick);
        recording_tick
    }

    fn note_reset_boundary(&mut self) {
        self.pending_epoch_break = true;
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum LiveSpectatorEntityDelta {
    Add {
        id: u32,
        add: SpectatorEntity,
    },
    Remove {
        id: u32,
        rm: bool,
    },
    Update {
        id: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        p: Option<[f32; 3]>,
        #[serde(skip_serializing_if = "Option::is_none")]
        r: Option<[[f32; 3]; 3]>,
    },
}

#[derive(Debug, Serialize)]
struct LiveSpectatorDelta {
    #[serde(rename = "type")]
    message_type: &'static str,
    tick: u64,
    server_time_ms: u64,
    game_status: String,
    entities: Vec<LiveSpectatorEntityDelta>,
    players: Vec<SpectatorPlayerInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sounds: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recent_events: Vec<serde_json::Value>,
}

fn spectator_render_matches(a: &SpectatorRender, b: &SpectatorRender) -> bool {
    a.kind == b.kind
        && a.role == b.role
        && a.preset_id == b.preset_id
        && a.primitive == b.primitive
        && a.material == b.material
        && a.color == b.color
        && a.is_static == b.is_static
        && a.casts_shadow == b.casts_shadow
        && a.receives_shadow == b.receives_shadow
        && a.visible == b.visible
        && a.double_sided == b.double_sided
        && a.transparency == b.transparency
}

fn spectator_entity_schema_matches(a: &SpectatorEntity, b: &SpectatorEntity) -> bool {
    let billboard_matches =
        serde_json::to_value(&a.billboard_gui).ok() == serde_json::to_value(&b.billboard_gui).ok();

    a.entity_type == b.entity_type
        && a.size == b.size
        && a.shape == b.shape
        && spectator_render_matches(&a.render, &b.render)
        && a.health == b.health
        && a.pickup_type == b.pickup_type
        && a.model_url == b.model_url
        && a.model_yaw_offset_deg == b.model_yaw_offset_deg
        && a.attributes == b.attributes
        && billboard_matches
}

fn compute_live_replication_delta(
    base: &SpectatorObservation,
    current: &SpectatorObservation,
    removed_entity_ids: &[u32],
) -> LiveSpectatorDelta {
    let base_entities: HashMap<u32, &SpectatorEntity> = base
        .entities
        .iter()
        .map(|entity| (entity.id, entity))
        .collect();
    let mut entities = Vec::new();

    for entity in &current.entities {
        match base_entities.get(&entity.id) {
            None => entities.push(LiveSpectatorEntityDelta::Add {
                id: entity.id,
                add: entity.clone(),
            }),
            Some(base_entity) => {
                if !spectator_entity_schema_matches(base_entity, entity) {
                    entities.push(LiveSpectatorEntityDelta::Add {
                        id: entity.id,
                        add: entity.clone(),
                    });
                    continue;
                }

                let position_changed = base_entity.position != entity.position;
                let rotation_changed = base_entity.rotation != entity.rotation;
                if position_changed || rotation_changed {
                    entities.push(LiveSpectatorEntityDelta::Update {
                        id: entity.id,
                        p: position_changed.then_some(entity.position),
                        r: if rotation_changed {
                            entity.rotation
                        } else {
                            None
                        },
                    });
                }
            }
        }
    }

    for id in removed_entity_ids {
        entities.push(LiveSpectatorEntityDelta::Remove { id: *id, rm: true });
    }

    LiveSpectatorDelta {
        message_type: "spectator_delta",
        tick: current.tick,
        server_time_ms: current.server_time_ms,
        game_status: current.game_status.clone(),
        entities,
        players: current.players.clone(),
        sounds: current
            .sounds
            .iter()
            .filter_map(|sound| serde_json::to_value(sound).ok())
            .collect(),
        recent_events: current
            .recent_events
            .iter()
            .filter_map(|event| serde_json::to_value(event).ok())
            .collect(),
    }
}

fn apply_replication_update_to_observation(
    base: &mut SpectatorObservation,
    current: SpectatorObservation,
    removed_entity_ids: &[u32],
) {
    base.tick = current.tick;
    base.server_time_ms = current.server_time_ms;
    base.game_status = current.game_status;
    base.players = current.players;
    base.sounds = current.sounds;
    base.recent_events = current.recent_events;

    if !removed_entity_ids.is_empty() {
        let removed: HashSet<u32> = removed_entity_ids.iter().copied().collect();
        base.entities.retain(|entity| !removed.contains(&entity.id));
    }

    if current.entities.is_empty() {
        return;
    }

    let mut index_by_id: HashMap<u32, usize> = base
        .entities
        .iter()
        .enumerate()
        .map(|(index, entity)| (entity.id, index))
        .collect();
    for entity in current.entities {
        if let Some(index) = index_by_id.get(&entity.id).copied() {
            base.entities[index] = entity;
        } else {
            index_by_id.insert(entity.id, base.entities.len());
            base.entities.push(entity);
        }
    }
}

fn base64_decoded_len(data: &str) -> Option<usize> {
    let len = data.len();
    if len == 0 {
        return Some(0);
    }
    if len % 4 != 0 {
        return None;
    }

    let pad = data
        .as_bytes()
        .iter()
        .rev()
        .take_while(|b| **b == b'=')
        .count();
    if pad > 2 {
        return None;
    }
    Some((len / 4) * 3 - pad)
}

fn compute_stream_duration_secs(stream: &AudioStreamState) -> f64 {
    let bytes_per_second = (stream.sample_rate_hz as f64)
        * (stream.channels as f64)
        * (stream.bytes_per_sample as f64);
    if bytes_per_second <= 0.0 {
        return 0.0;
    }
    let speed = (stream.playback_speed as f64).max(MIN_PLAYBACK_SPEED as f64);
    (stream.total_audio_bytes as f64) / bytes_per_second / speed
}

fn append_server_log(log_file: &Path, message: &str) {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_file) {
        let _ = writeln!(file, "[{}] {}", ts_ms, message);
    }
}

fn complete_audio_stream(state: &LocalState, stream_id: &str, reason: &str) {
    let completed_at = Instant::now();
    let stream_state = {
        let mut streams = state.audio_streams.lock().unwrap();
        streams.remove(stream_id)
    };

    let Some(stream_state) = stream_state else {
        return;
    };

    let expected_lag_ms = stream_state
        .expected_end_at
        .map(|eta| {
            if completed_at >= eta {
                completed_at.duration_since(eta).as_millis() as i128
            } else {
                -(eta.duration_since(completed_at).as_millis() as i128)
            }
        })
        .unwrap_or(0);
    append_server_log(
        &state.log_file,
        &format!(
            "[audio] complete stream={} speaker={} reason={} lag_vs_estimate_ms={} total_bytes={}",
            stream_id,
            stream_state.speaker,
            reason,
            expected_lag_ms,
            stream_state.total_audio_bytes
        ),
    );

    push_playback_done_event(state, stream_state.speaker.clone(), stream_id.to_string());

    let speech_text = state.audio_speech_text.lock().unwrap().remove(stream_id);

    let speech_finished_data = serde_json::json!({
        "stream_id": stream_id,
        "reason": reason,
        "speaker": stream_state.speaker,
        "speech_text": speech_text,
    });

    if let Err(e) = game::queue_input(
        &state.game_handle,
        state.game_id,
        stream_state.agent_id,
        "SpeechFinished".to_string(),
        speech_finished_data,
    ) {
        eprintln!(
            "[audio] Failed to queue SpeechFinished for stream {}: {}",
            stream_id, e
        );
    }

    if let Err(e) = game::queue_input(
        &state.game_handle,
        state.game_id,
        stream_state.agent_id,
        "AudioEnded".to_string(),
        serde_json::json!({
            "stream_id": stream_id,
            "reason": reason
        }),
    ) {
        eprintln!(
            "[audio] Failed to queue AudioEnded for stream {}: {}",
            stream_id, e
        );
    }
}

fn schedule_audio_stream_completion(
    state: LocalState,
    stream_id: String,
    expected_end_at: Instant,
) {
    tokio::spawn(async move {
        tokio::time::sleep_until(tokio::time::Instant::from_std(expected_end_at)).await;
        complete_audio_stream(&state, &stream_id, SERVER_PREDICTED_END_REASON);
    });
}

fn push_speech_event(state: &LocalState, speaker: String, text: String) {
    let seq = {
        let mut seq = state.speech_seq.lock().unwrap();
        let next = *seq;
        *seq += 1;
        next
    };
    let event = SpeechEvent { speaker, text, seq };
    let _ = state.speech_tx.send(event);
}

fn maybe_publish_authoritative_speech_events(
    state: &LocalState,
    recent_events: &[clawblox::game::instance::SpectatorReplicatedEvent],
) {
    if recent_events.is_empty() {
        return;
    }

    let mut cursor = state.authoritative_speech_event_cursor.lock().unwrap();
    for event in recent_events {
        if event.id <= *cursor {
            continue;
        }
        if event.name == "SpeechBus" {
            let payload = &event.payload;
            let op = payload.get("op").and_then(|v| v.as_str()).unwrap_or("");
            if op == "SpeechText" {
                let speaker = payload
                    .get("speaker")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                let text = payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !speaker.is_empty() && !text.is_empty() {
                    push_speech_event(state, speaker, text);
                }
            }
        }
        *cursor = event.id;
    }
}

fn push_speech_progress_event(
    state: &LocalState,
    speaker: String,
    stream_id: String,
    text: String,
) {
    let seq = {
        let mut seq = state.speech_progress_seq.lock().unwrap();
        let next = *seq;
        *seq += 1;
        next
    };
    let event = SpeechProgressEvent {
        speaker,
        stream_id,
        text,
        seq,
    };
    let _ = state.speech_progress_tx.send(event);
}

fn push_playback_done_event(state: &LocalState, speaker: String, stream_id: String) {
    let seq = {
        let mut seq = state.playback_done_seq.lock().unwrap();
        let next = *seq;
        *seq += 1;
        next
    };
    let _ = state.playback_done_tx.send(PlaybackDoneEvent {
        speaker,
        stream_id,
        seq,
    });
}

fn prune_stale_audio_streams(state: &LocalState, now: Instant) {
    let mut stale_stream_ids = Vec::new();
    {
        let mut streams = state.audio_streams.lock().unwrap();
        streams.retain(|stream_id, stream| {
            let keep = now.duration_since(stream.last_seen) < Duration::from_secs(STREAM_TTL_SECS);
            if !keep {
                stale_stream_ids.push(stream_id.clone());
            }
            keep
        });
    }

    if !stale_stream_ids.is_empty() {
        let mut speech_texts = state.audio_speech_text.lock().unwrap();
        for stream_id in stale_stream_ids {
            speech_texts.remove(&stream_id);
        }
    }
}

/// Check if the world instance is halted due to a Lua error.
/// Returns the error message with log path appended, or None if running normally.
fn check_halted(state: &LocalState) -> Option<String> {
    let handle = state.game_handle.instances.get(&state.instance_id)?;
    let instance = handle.read();
    instance.halted_error.as_ref().map(|err| {
        format!(
            "World halted: {}. See logs for full stack trace: {}",
            err,
            state.log_file.display()
        )
    })
}

fn speech_mode_name(mode: SpeechMode) -> &'static str {
    match mode {
        SpeechMode::Global => "global",
        SpeechMode::Proximity => "proximity",
    }
}

fn round_chat_distance(value: f32) -> f32 {
    (value * 100.0).round() / 100.0
}

fn listener_can_receive_message(listener_agent_id: Uuid, message: &LocalChatMessage) -> bool {
    listener_agent_id == message.agent_id || message.heard_by.contains(&listener_agent_id)
}

fn build_chat_audibility(
    state: &LocalState,
    speaker_agent_id: Uuid,
    speaker_name: &str,
) -> Result<(String, Option<f32>, Vec<Uuid>, Vec<ChatAudibilityEntry>), String> {
    let observation =
        game::get_spectator_observation_for_instance(&state.game_handle, state.instance_id)?;
    let speaker_position = observation
        .players
        .iter()
        .find(|player| player.id == speaker_agent_id)
        .map(|player| player.position);

    let mut heard_by = Vec::new();
    let mut audibility = Vec::new();

    for player in &observation.players {
        let distance = speaker_position.map(|speaker_pos| {
            let dx = player.position[0] - speaker_pos[0];
            let dy = player.position[1] - speaker_pos[1];
            let dz = player.position[2] - speaker_pos[2];
            round_chat_distance((dx * dx + dy * dy + dz * dz).sqrt())
        });

        let heard = match state.speech_config.mode {
            SpeechMode::Global => true,
            SpeechMode::Proximity => {
                player.id == speaker_agent_id
                    || distance
                        .map(|value| value <= state.speech_config.hearing_radius)
                        .unwrap_or(false)
            }
        };

        if heard {
            heard_by.push(player.id);
        }

        audibility.push(ChatAudibilityEntry {
            agent_id: player.id,
            agent_name: player.name.clone(),
            distance,
            heard,
        });
    }

    if !audibility
        .iter()
        .any(|entry| entry.agent_id == speaker_agent_id)
    {
        heard_by.push(speaker_agent_id);
        audibility.push(ChatAudibilityEntry {
            agent_id: speaker_agent_id,
            agent_name: speaker_name.to_string(),
            distance: Some(0.0),
            heard: true,
        });
    }

    Ok((
        speech_mode_name(state.speech_config.mode).to_string(),
        match state.speech_config.mode {
            SpeechMode::Global => None,
            SpeechMode::Proximity => Some(state.speech_config.hearing_radius),
        },
        heard_by,
        audibility,
    ))
}

fn content_type_for_path(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".json") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else {
        "application/octet-stream"
    }
}

fn embedded_ui_file_response(path: &str) -> axum::response::Response {
    if let Some(file) = LOCAL_RUNTIME_UI_DIR.get_file(path) {
        let content_type = content_type_for_path(path);
        return (
            axum::http::StatusCode::OK,
            [("content-type", content_type)],
            file.contents().to_vec(),
        )
            .into_response();
    }
    (
        axum::http::StatusCode::NOT_FOUND,
        [("content-type", "text/plain; charset=utf-8")],
        b"Not found".to_vec(),
    )
        .into_response()
}

fn embedded_index_response(index_path: &str, base_path: &str) -> axum::response::Response {
    let Some(file) = LOCAL_RUNTIME_UI_DIR.get_file(index_path) else {
        return embedded_ui_file_response(index_path);
    };
    let mut html = String::from_utf8_lossy(file.contents()).into_owned();
    let base_href = format!("{}/", normalize_base_path(base_path).trim_end_matches('/'));
    let base_href = if base_href == "/" {
        "/".to_string()
    } else {
        base_href
    };
    html = html.replacen(
        "<head>",
        &format!("<head>\n    <base href=\"{}\" />", base_href),
        1,
    );
    (
        axum::http::StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

fn embedded_ui_index_response(base_path: &str) -> axum::response::Response {
    embedded_index_response("index.html", base_path)
}

fn replay_view_response(base_path: &str) -> axum::response::Response {
    let base_href = format!("{}/", normalize_base_path(base_path).trim_end_matches('/'));
    let base_href = if base_href == "/" {
        "/".to_string()
    } else {
        base_href
    };
    let html = format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <base href="{base_href}" />
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <title>Clawblox Replay View</title>
    <link rel="stylesheet" href="ui/styles.css" />
    <script type="importmap">
      {{
        "imports": {{
          "three": "https://esm.sh/three@0.160.0",
          "three/": "https://esm.sh/three@0.160.0/",
          "lucide": "https://esm.sh/lucide@0.468.0"
        }}
      }}
    </script>
    <link rel="modulepreload" crossorigin href="https://esm.sh/three@0.160.0/examples/jsm/loaders/GLTFLoader.js" />
    <link rel="modulepreload" crossorigin href="https://esm.sh/three@0.160.0/examples/jsm/utils/SkeletonUtils.js" />
    <style>
      html, body, #app, .stage-wrap {{
        width: 100%;
        height: 100%;
        margin: 0;
        overflow: hidden;
        background: #05070f;
      }}
      .hud-layer {{
        pointer-events: none;
      }}
      .runtime-meta-sr {{
        position: absolute;
        width: 1px;
        height: 1px;
        overflow: hidden;
        clip: rect(0, 0, 0, 0);
      }}
    </style>
  </head>
  <body>
    <div id="app">
      <main class="stage-wrap" aria-label="Replay viewport">
        <canvas id="stage" aria-label="Replay renderer canvas"></canvas>
        <div class="hud-layer" aria-hidden="true">
          <div class="runtime-meta-sr">
            <p id="status">Connecting...</p>
            <div id="meta"></div>
          </div>
        </div>
      </main>
    </div>
    <script type="module" src="ui/main.js"></script>
  </body>
</html>"#
    );
    (
        axum::http::StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        html,
    )
        .into_response()
}

async fn local_ui_index(State(state): State<LocalState>) -> impl IntoResponse {
    embedded_ui_index_response(&state.base_path)
}

async fn replay_ui_index(State(state): State<ReplayState>) -> impl IntoResponse {
    embedded_ui_index_response(&state.base_path)
}

async fn replay_view_index(State(state): State<ReplayState>) -> impl IntoResponse {
    replay_view_response(&state.base_path)
}

async fn external_ui_index(State(state): State<ExternalLocalState>) -> impl IntoResponse {
    embedded_ui_index_response(&state.base_path)
}

fn external_snapshot(state: &ExternalLocalState) -> serde_json::Value {
    let tick = state.started_at.elapsed().as_millis() as u64;
    serde_json::json!({
        "instance_id": "external",
        "tick": tick,
        "server_time_ms": tick,
        "game_status": "playing",
        "players": [],
        "entities": [],
        "recent_events": []
    })
}

fn clear_local_instance_caches(state: &LocalState) {
    state.game_handle.spectator_cache.remove(&state.instance_id);

    let obs_keys: Vec<_> = state
        .game_handle
        .observation_cache
        .iter()
        .filter(|entry| entry.key().0 == state.instance_id)
        .map(|entry| *entry.key())
        .collect();
    for key in obs_keys {
        state.game_handle.observation_cache.remove(&key);
    }

    let player_keys: Vec<_> = state
        .game_handle
        .player_instances
        .iter()
        .filter(|entry| *entry.value() == state.instance_id)
        .map(|entry| *entry.key())
        .collect();
    for key in player_keys {
        state.game_handle.player_instances.remove(&key);
    }
}

fn apply_afk_timeout(instance: &mut GameInstance, afk_timeout_secs: u64) {
    if afk_timeout_secs == 0 {
        instance.set_afk_timeout(Duration::from_secs(u64::MAX));
    } else {
        instance.set_afk_timeout(Duration::from_secs(afk_timeout_secs));
    }
}

fn reset_local_instance(state: &LocalState) -> Result<(), String> {
    let sessions: Vec<(Uuid, String)> = state
        .sessions
        .iter()
        .map(|entry| {
            let (agent_id, name) = entry.value().clone();
            (agent_id, name)
        })
        .collect();

    let mut instance = GameInstance::new_with_config(
        state.game_id,
        state.max_players,
        state.observation_radius,
        state.game_handle.async_bridge.clone(),
        state.error_mode,
    );
    instance.set_player_object_physics(state.player_object_physics);
    instance.set_physics_config(&state.physics_config);
    instance.instance_id = state.instance_id;
    instance.init_runtime();
    clawblox::snapshot::restore_game_instance(&mut instance, &state.initial_instance_snapshot)?;
    // Reset reuses the initial snapshot captured at startup, but older snapshots
    // may not contain persistent script instances. Re-running the idempotent
    // bundle ensures runtime handlers are rebound without duplicating world state.
    instance.load_script(state.world_script_source.as_str());
    instance.halted_error = None;
    apply_afk_timeout(&mut instance, state.afk_timeout_secs);

    let spectator_obs = instance.get_spectator_observation();

    clear_local_instance_caches(state);
    state.game_handle.instances.insert(
        state.instance_id,
        Arc::new(parking_lot::RwLock::new(instance)),
    );
    state
        .game_handle
        .spectator_cache
        .insert(state.instance_id, spectator_obs);

    for (agent_id, name) in sessions {
        game::join_instance(
            &state.game_handle,
            state.instance_id,
            state.game_id,
            agent_id,
            &name,
        )?;
    }

    Ok(())
}

fn record_local_input(
    state: &LocalState,
    agent_id: Uuid,
    input_type: String,
    input_data: serde_json::Value,
    source_tick: u64,
) {
    if let Some(writer) = &state.recording_writer {
        let time_ms = state.started_at.elapsed().as_millis() as u64;
        let recording_tick = remap_recording_tick(state, source_tick);
        let writer = writer.clone();
        tokio::spawn(async move {
            let w = writer.lock().await;
            w.record_input(recording_tick, time_ms, &agent_id, &input_type, &input_data)
                .await;
        });
    }
}

fn remap_recording_tick(state: &LocalState, source_tick: u64) -> u64 {
    let mut timeline = state.recording_timeline.lock().unwrap();
    timeline.note_source_tick(source_tick)
}

fn build_local_player_observation(
    state: &LocalState,
    agent_id: Uuid,
) -> Result<PlayerObservation, (axum::http::StatusCode, String)> {
    let mut observation = game::get_observation(&state.game_handle, state.game_id, agent_id)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e))?;
    resolve_local_player_assets(&mut observation, &state.base_path);
    maybe_publish_authoritative_speech_events(&state, &observation.recent_events);
    Ok(observation)
}

fn local_instance_tick(state: &LocalState) -> Result<u64, (axum::http::StatusCode, String)> {
    let handle = state
        .game_handle
        .instances
        .get(&state.instance_id)
        .ok_or_else(|| {
            (
                axum::http::StatusCode::NOT_FOUND,
                format!("missing instance {}", state.instance_id),
            )
        })?;
    let instance = handle.read();
    if let Some(err) = &instance.halted_error {
        return Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            format!("World halted: {}", err),
        ));
    }
    Ok(instance.tick)
}

async fn build_local_player_observation_after_tick(
    state: &LocalState,
    agent_id: Uuid,
    queued_before_tick: u64,
) -> Result<PlayerObservation, (axum::http::StatusCode, String)> {
    let timeout = Duration::from_millis(2_000);
    let poll_interval = Duration::from_millis(2);
    let deadline = Instant::now() + timeout;
    let target_tick = queued_before_tick.saturating_add(2);

    loop {
        let observation = build_local_player_observation(state, agent_id)?;
        if observation.tick >= target_tick || Instant::now() >= deadline {
            return Ok(observation);
        }
        tokio::time::sleep(poll_interval).await;
    }
}

fn perform_engine_reset_for_session(
    state: &LocalState,
    agent_id: Uuid,
) -> Result<PlayerObservation, (axum::http::StatusCode, String)> {
    let source_tick =
        game::get_spectator_observation_for_instance(&state.game_handle, state.instance_id)
            .map(|obs| obs.tick)
            .unwrap_or(0);
    record_local_input(
        state,
        agent_id,
        "Reset".to_string(),
        serde_json::json!({}),
        source_tick,
    );
    state
        .recording_timeline
        .lock()
        .unwrap()
        .note_reset_boundary();
    reset_local_instance(state).map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    build_local_player_observation(state, agent_id)
}

fn capture_local_runtime_snapshot(state: &LocalState) -> Result<LocalRuntimeSnapshot, String> {
    let handle = state
        .game_handle
        .instances
        .get(&state.instance_id)
        .ok_or_else(|| format!("missing instance {}", state.instance_id))?;
    let instance_snapshot = {
        let instance = handle.read();
        clawblox::snapshot::capture_game_instance(&instance)?
    };

    let sessions = state
        .sessions
        .iter()
        .map(|entry| {
            let (agent_id, name) = entry.value().clone();
            SessionSnapshot {
                token: entry.key().clone(),
                agent_id,
                name,
            }
        })
        .collect();

    let chat_state = state.chat_state.lock().unwrap();
    let chat = ChatStateSnapshot {
        next_seq: chat_state.next_seq,
        messages: chat_state.messages.iter().cloned().collect(),
    };

    Ok(LocalRuntimeSnapshot {
        schema_version: 1,
        game_id: state.game_id,
        instance: instance_snapshot,
        sessions,
        chat,
    })
}

async fn local_snapshot(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<SpectatorAccessQuery>,
) -> Result<Json<LocalRuntimeSnapshot>, (axum::http::StatusCode, String)> {
    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }
    if !operator_token_matches(&headers) {
        require_spectator_access(
            &state.spectator_token,
            &headers,
            query.spectator_token.as_deref(),
        )?;
    }

    let snapshot = capture_local_runtime_snapshot(&state)
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(snapshot))
}

async fn external_renderer_manifest(
    State(state): State<ExternalLocalState>,
) -> Json<LocalRendererManifest> {
    Json(state.renderer_manifest)
}

async fn external_spectate(
    State(state): State<ExternalLocalState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<SpectatorAccessQuery>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    require_spectator_access(
        &state.spectator_token,
        &headers,
        query.spectator_token.as_deref(),
    )?;
    Ok(Json(external_snapshot(&state)))
}

async fn external_skill(State(state): State<ExternalLocalState>) -> impl IntoResponse {
    match state.skill_md {
        Some(content) => (
            axum::http::StatusCode::OK,
            [("content-type", "text/markdown; charset=utf-8")],
            content,
        ),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            [("content-type", "text/plain")],
            "No API.md or SKILL.md found".to_string(),
        ),
    }
}

async fn external_spectate_ws(
    State(state): State<ExternalLocalState>,
    Query(query): Query<SpectatorAccessQuery>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, (axum::http::StatusCode, String)> {
    require_spectator_access(
        &state.spectator_token,
        &headers,
        query.spectator_token.as_deref(),
    )?;
    Ok(ws.on_upgrade(move |socket| handle_external_spectate_ws(socket, state)))
}

async fn handle_external_spectate_ws(socket: WebSocket, state: ExternalLocalState) {
    let (mut sender, mut receiver) = socket.split();
    let mut tick_interval = interval(Duration::from_millis(250));

    loop {
        tokio::select! {
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        if sender.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            _ = tick_interval.tick() => {
                let payload = external_snapshot(&state);
                if sender.send(Message::Text(payload.to_string().into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

async fn local_ui_file(AxumPath(path): AxumPath<String>) -> impl IntoResponse {
    let req_path = path.trim_start_matches('/');
    if req_path.is_empty() {
        return embedded_ui_file_response("index.html");
    }
    embedded_ui_file_response(req_path)
}

#[derive(Deserialize)]
struct SessionUiFilePath {
    #[allow(dead_code)]
    id: String,
    path: String,
}

async fn session_ui_file(AxumPath(params): AxumPath<SessionUiFilePath>) -> impl IntoResponse {
    let req_path = params.path.trim_start_matches('/');
    if req_path.is_empty() {
        return embedded_ui_file_response("index.html");
    }
    embedded_ui_file_response(req_path)
}

#[derive(Deserialize)]
struct SessionWorldFilePath {
    #[allow(dead_code)]
    id: String,
    path: String,
}

#[derive(Deserialize)]
struct JoinQuery {
    name: String,
}

#[derive(Serialize)]
struct JoinResponse {
    session: String,
    agent_id: Uuid,
}

async fn local_join(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<JoinQuery>,
) -> Result<Json<JoinResponse>, (axum::http::StatusCode, String)> {
    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }

    // If the caller already has a valid session, return it instead of creating
    // a duplicate player.  This prevents agents from re-joining (even under a
    // different name).
    if let Some(token) = headers.get("X-Session").and_then(|h| h.to_str().ok()) {
        if let Some(existing) = state.sessions.get(token) {
            let (agent_id, name) = existing.clone();
            if game::get_player_instance(&state.game_handle, agent_id, state.game_id)
                != Some(state.instance_id)
            {
                game::join_instance(
                    &state.game_handle,
                    state.instance_id,
                    state.game_id,
                    agent_id,
                    &name,
                )
                .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e))?;
            }
            return Ok(Json(JoinResponse {
                session: token.to_string(),
                agent_id,
            }));
        }
    }

    let agent_id = Uuid::new_v4();
    let session_token = Uuid::new_v4().to_string();

    // Join the instance
    game::join_instance(
        &state.game_handle,
        state.instance_id,
        state.game_id,
        agent_id,
        &query.name,
    )
    .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e))?;

    // Store session
    state
        .sessions
        .insert(session_token.clone(), (agent_id, query.name));

    Ok(Json(JoinResponse {
        session: session_token,
        agent_id,
    }))
}

fn get_session(
    state: &LocalState,
    headers: &axum::http::HeaderMap,
) -> Result<(Uuid, String), (axum::http::StatusCode, String)> {
    let token = headers
        .get("X-Session")
        .and_then(|h| h.to_str().ok())
        .ok_or((
            axum::http::StatusCode::UNAUTHORIZED,
            "Missing X-Session header".to_string(),
        ))?;

    state.sessions.get(token).map(|r| r.clone()).ok_or((
        axum::http::StatusCode::UNAUTHORIZED,
        "Invalid session".to_string(),
    ))
}

#[derive(Deserialize)]
struct SpectatorAccessQuery {
    spectator_token: Option<String>,
}

/// Use the launcher-provided spectator token when present (actuate sets
/// WORLD_SPECTATOR_TOKEN and appends it to viewport URLs); otherwise
/// self-generate one and print it at startup.
fn resolve_spectator_token() -> String {
    std::env::var("WORLD_SPECTATOR_TOKEN")
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

/// Accept the save/load protocol's operator token as an alternative to
/// spectator access: clawblox sets WORLD_OPERATOR_TOKEN when it launches a
/// world and presents it via X-Operator-Token when pulling snapshots.
fn operator_token_matches(headers: &axum::http::HeaderMap) -> bool {
    let Ok(expected) = std::env::var("WORLD_OPERATOR_TOKEN") else {
        return false;
    };
    let expected = expected.trim();
    if expected.is_empty() {
        return false;
    }
    headers
        .get("X-Operator-Token")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .is_some_and(|token| token == expected)
}

fn require_spectator_access(
    expected_token: &str,
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
) -> Result<(), (axum::http::StatusCode, String)> {
    let provided = headers
        .get("X-Spectator-Token")
        .and_then(|h| h.to_str().ok())
        .or(query_token)
        .map(str::trim)
        .filter(|token| !token.is_empty());

    match provided {
        Some(token) if token == expected_token => Ok(()),
        _ => Err((
            axum::http::StatusCode::UNAUTHORIZED,
            "Missing or invalid spectator token".to_string(),
        )),
    }
}

#[derive(Deserialize)]
struct AudioChunkRequest {
    stream_id: String,
    seq: u32,
    data: String,
    done: bool,
    #[serde(default)]
    speech_text: Option<String>,
    #[serde(default)]
    sample_rate_hz: Option<u32>,
    #[serde(default)]
    channels: Option<u16>,
    #[serde(default)]
    bytes_per_sample: Option<u16>,
    #[serde(default)]
    playback_speed: Option<f32>,
}

async fn local_audio(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<AudioChunkRequest>,
) -> Result<axum::http::StatusCode, (axum::http::StatusCode, String)> {
    let (agent_id, name) = get_session(&state, &headers)?;
    let stream_id = body.stream_id.clone();
    let chunk_bytes = base64_decoded_len(&body.data).ok_or((
        axum::http::StatusCode::BAD_REQUEST,
        "Invalid base64 PCM chunk".to_string(),
    ))? as u64;

    let sample_rate_hz = body
        .sample_rate_hz
        .unwrap_or(DEFAULT_SAMPLE_RATE_HZ)
        .max(MIN_POSITIVE_U32);
    let channels = body
        .channels
        .unwrap_or(DEFAULT_CHANNELS)
        .max(MIN_POSITIVE_U16);
    let bytes_per_sample = body
        .bytes_per_sample
        .unwrap_or(DEFAULT_BYTES_PER_SAMPLE)
        .max(MIN_POSITIVE_U16);
    let playback_speed = body
        .playback_speed
        .unwrap_or(DEFAULT_PLAYBACK_SPEED)
        .max(MIN_PLAYBACK_SPEED);
    let mut schedule: Option<Instant> = None;

    // Track stream lifecycle metadata and prune stale entries.
    {
        let now = Instant::now();
        prune_stale_audio_streams(&state, now);

        let mut streams = state.audio_streams.lock().unwrap();
        let stream = streams
            .entry(stream_id.clone())
            .or_insert_with(|| AudioStreamState {
                agent_id,
                speaker: name.clone(),
                first_seen: now,
                last_seen: now,
                total_audio_bytes: 0,
                done_received: false,
                done_at: None,
                expected_end_at: None,
                completion_scheduled: false,
                sample_rate_hz,
                channels,
                bytes_per_sample,
                playback_speed,
            });
        stream.last_seen = now;
        stream.total_audio_bytes = stream.total_audio_bytes.saturating_add(chunk_bytes);
        stream.sample_rate_hz = sample_rate_hz;
        stream.channels = channels;
        stream.bytes_per_sample = bytes_per_sample;
        stream.playback_speed = playback_speed;

        if body.done && !stream.done_received {
            stream.done_received = true;
            stream.done_at = Some(now);
            let duration_secs = compute_stream_duration_secs(stream);
            let duration = Duration::from_secs_f64(duration_secs.max(0.0));
            let expected_end_at = stream.first_seen + duration;
            stream.expected_end_at = Some(expected_end_at);

            if !stream.completion_scheduled {
                stream.completion_scheduled = true;
                schedule = Some(expected_end_at);
            }
        }
    }

    if let Some(expected_end_at) = schedule {
        let now = Instant::now();
        let eta_ms = if expected_end_at > now {
            expected_end_at.duration_since(now).as_millis() as u64
        } else {
            0
        };
        let expected_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
            .saturating_add(eta_ms);
        append_server_log(
            &state.log_file,
            &format!(
                "[audio] estimate stream={} speaker={} done_chunk_seq={} eta_in_ms={} eta_unix_ms={} sample_rate={} channels={} bytes_per_sample={} speed={}",
                stream_id,
                name,
                body.seq,
                eta_ms,
                expected_unix_ms,
                sample_rate_hz,
                channels,
                bytes_per_sample,
                playback_speed
            ),
        );
        schedule_audio_stream_completion(state.clone(), stream_id.clone(), expected_end_at);
    }

    // Store speech text if provided (typically on the done=true chunk)
    if let Some(text) = &body.speech_text {
        state
            .audio_speech_text
            .lock()
            .unwrap()
            .insert(stream_id.clone(), text.clone());
    }

    let _ = state.audio_tx.send(AudioChunk {
        speaker: name,
        stream_id: stream_id.clone(),
        seq: body.seq,
        data: body.data,
        done: body.done,
    });

    Ok(axum::http::StatusCode::OK)
}

async fn local_chat(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<ChatPostRequest>,
) -> Result<Json<ChatPostResponse>, (axum::http::StatusCode, String)> {
    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }

    let (agent_id, agent_name) = get_session(&state, &headers)?;
    let content = body.content.trim();
    if content.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "content must be 1-500 non-whitespace characters".to_string(),
        ));
    }
    if content.chars().count() > 500 {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "content exceeds 500 characters".to_string(),
        ));
    }

    let (delivery_mode, hearing_radius, heard_by, audibility) =
        build_chat_audibility(&state, agent_id, &agent_name)
            .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let msg = {
        let mut chat = state.chat_state.lock().unwrap();
        chat.push(
            state.instance_id,
            agent_id,
            agent_name,
            content.to_string(),
            delivery_mode,
            hearing_radius,
            heard_by,
            audibility,
        )
    };

    let _ = state.chat_tx.send(msg.clone());

    // Record chat message if recording is enabled
    if let Some(ref writer) = state.recording_writer {
        let time_ms = state.started_at.elapsed().as_millis() as u64;
        let source_tick =
            game::get_spectator_observation_for_instance(&state.game_handle, state.instance_id)
                .map(|obs| obs.tick)
                .unwrap_or(0);
        let recording_tick = remap_recording_tick(&state, source_tick);
        let writer = writer.clone();
        let msg_id = msg.id.clone();
        let agent_name_clone = msg.agent_name.clone();
        let content_clone = msg.content.clone();
        let created_at = msg.created_at.clone();
        let delivery_mode = msg.delivery_mode.clone();
        let hearing_radius = msg.hearing_radius;
        let heard_by = msg
            .heard_by
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let audibility = msg
            .audibility
            .iter()
            .map(|entry| recording::RecordedChatAudibilityEntry {
                agent_id: entry.agent_id.to_string(),
                agent_name: entry.agent_name.clone(),
                distance: entry.distance,
                heard: entry.heard,
            })
            .collect::<Vec<_>>();
        tokio::spawn(async move {
            let w = writer.lock().await;
            w.record_chat(
                recording_tick,
                time_ms,
                &msg_id,
                &agent_id,
                &agent_name_clone,
                &content_clone,
                &created_at,
                &delivery_mode,
                hearing_radius,
                &heard_by,
                &audibility,
            )
            .await;
        });
    }

    Ok(Json(ChatPostResponse {
        id: msg.id,
        created_at: msg.created_at,
    }))
}

async fn local_chat_messages(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ChatMessagesQuery>,
) -> Result<Json<ChatMessagesResponse>, (axum::http::StatusCode, String)> {
    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }

    let listener_agent_id = get_session(&state, &headers).map(|(agent_id, _)| agent_id)?;
    let instance_id = query.instance_id.unwrap_or(state.instance_id);
    let limit = query
        .limit
        .unwrap_or(LOCAL_CHAT_DEFAULT_LIMIT)
        .clamp(1, LOCAL_CHAT_MAX_LIMIT);
    let after = query
        .after
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let messages = {
        let chat = state.chat_state.lock().unwrap();
        chat.list(instance_id, after, LOCAL_CHAT_MAX_MESSAGES)
            .into_iter()
            .filter(|message| listener_can_receive_message(listener_agent_id, message))
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    };

    Ok(Json(ChatMessagesResponse { messages }))
}

#[derive(Deserialize)]
struct InputRequest {
    #[serde(rename = "type")]
    input_type: String,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Serialize)]
struct ActionFeedbackResponse {
    tick: u64,
    action: ActionFeedback,
}

#[derive(Serialize)]
struct ActionFeedback {
    #[serde(rename = "type")]
    input_type: String,
    status: String,
}

fn action_status_from_observation(action_type: &str, observation: &PlayerObservation) -> String {
    if action_type == "Reset" {
        return "ResetSuccess".to_string();
    }
    observation
        .player
        .attributes
        .get("LastActionResult")
        .and_then(|value| value.as_str())
        .unwrap_or("Unknown")
        .to_string()
}

fn action_response(
    state: &LocalState,
    action_type: String,
    observation: PlayerObservation,
) -> axum::response::Response {
    if state.state_observation {
        return Json(observation).into_response();
    }
    if state.action_feedback {
        let status = action_status_from_observation(&action_type, &observation);
        return Json(ActionFeedbackResponse {
            tick: observation.tick,
            action: ActionFeedback {
                input_type: action_type,
                status,
            },
        })
        .into_response();
    }
    axum::http::StatusCode::NO_CONTENT.into_response()
}

async fn local_input(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Json(input): Json<InputRequest>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    let (agent_id, agent_name) = get_session(&state, &headers)?;
    if input.input_type == "Reset" {
        if !state.allow_reset {
            return Err((
                axum::http::StatusCode::FORBIDDEN,
                "Reset is disabled for this world".to_string(),
            ));
        }
        return perform_engine_reset_for_session(&state, agent_id)
            .map(|observation| action_response(&state, "Reset".to_string(), observation));
    }

    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }

    let speech_progress = extract_speech_progress_event(&input.input_type, &input.data);
    let input_type = input.input_type.clone();
    let input_data = input.data.clone();
    let queued_before_tick = local_instance_tick(&state)?;

    game::queue_input(
        &state.game_handle,
        state.game_id,
        agent_id,
        input.input_type,
        input.data,
    )
    .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e))?;

    // Return an observation after the next full input-processing tick. The
    // manager may receive the HTTP request after the current frame's
    // agent-input phase, so one tick boundary can still be stale.
    let observation =
        build_local_player_observation_after_tick(&state, agent_id, queued_before_tick).await?;
    record_local_input(
        &state,
        agent_id,
        input_type.clone(),
        input_data,
        observation.tick,
    );

    if let Some(progress) = speech_progress {
        maybe_publish_speech_progress(&state, &observation, &agent_name, progress);
    }

    Ok(action_response(&state, input_type, observation))
}

async fn local_reset(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    let (agent_id, _) = get_session(&state, &headers)?;
    if !state.allow_reset {
        return Err((
            axum::http::StatusCode::FORBIDDEN,
            "Reset is disabled for this world".to_string(),
        ));
    }
    perform_engine_reset_for_session(&state, agent_id)
        .map(|observation| action_response(&state, "Reset".to_string(), observation))
}

#[derive(Clone, Debug)]
struct SpeechProgressInput {
    stream_id: String,
    text: String,
}

fn extract_speech_progress_event(
    input_type: &str,
    data: &serde_json::Value,
) -> Option<SpeechProgressInput> {
    if input_type != "RemoteEvent" {
        return None;
    }
    let name = data.get("name")?.as_str()?;
    if name != "SpeechBus" {
        return None;
    }
    let args = data.get("args")?.as_array()?;
    let payload = args.first()?.as_object()?;
    let op = payload.get("op")?.as_str()?;
    if op != "Progress" {
        return None;
    }

    let stream_id = payload.get("stream_id")?.as_str()?.trim().to_string();
    if stream_id.is_empty() {
        return None;
    }
    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Some(SpeechProgressInput { stream_id, text })
}

fn maybe_publish_speech_progress(
    state: &LocalState,
    observation: &PlayerObservation,
    speaker_name: &str,
    progress: SpeechProgressInput,
) {
    if progress.text.is_empty() {
        return;
    }

    let attrs = &observation.player.attributes;
    let is_speaking = attrs
        .get("IsSpeaking")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !is_speaking {
        return;
    }

    let observed_stream_id = attrs
        .get("SpeechStreamId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if observed_stream_id != progress.stream_id {
        return;
    }

    let has_lease = attrs
        .get("SpeechLeaseId")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_lease {
        return;
    }

    push_speech_progress_event(
        state,
        speaker_name.to_string(),
        progress.stream_id,
        progress.text,
    );
}

async fn local_observe(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<PlayerObservation>, (axum::http::StatusCode, String)> {
    if !state.state_observation {
        return Err((
            axum::http::StatusCode::FORBIDDEN,
            "State observation is disabled for this world".to_string(),
        ));
    }

    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }

    let (agent_id, _) = get_session(&state, &headers)?;

    let mut observation = game::get_observation(&state.game_handle, state.game_id, agent_id)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e))?;
    resolve_local_player_assets(&mut observation, &state.base_path);
    maybe_publish_authoritative_speech_events(&state, &observation.recent_events);

    Ok(Json(observation))
}

#[derive(Deserialize)]
struct ObserveImageQuery {
    width: Option<u32>,
    height: Option<u32>,
}

/// First-person PNG frame of what the requesting agent currently sees,
/// rendered by the world's own renderer in the capture sidecar.
async fn local_observe_image(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ObserveImageQuery>,
) -> Result<axum::response::Response, (axum::http::StatusCode, String)> {
    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }

    let Some(capture) = state.capture.clone() else {
        return Err((
            axum::http::StatusCode::NOT_FOUND,
            "Vision is not enabled for this world (set [agent_api] vision = true in world.toml)"
                .to_string(),
        ));
    };

    let (agent_id, _) = get_session(&state, &headers)?;

    // Validates the agent is alive in the instance and provides the engine FOV.
    let pose = game::get_player_camera_pose(&state.game_handle, state.game_id, agent_id)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e))?;

    // Default 1088x640 (aspect ~1.7): the engine's 105° horizontal FOV maps to
    // the 75° vertical FOV used for observation culling.
    let width = query.width.unwrap_or(1088).clamp(64, 1920);
    let height = query.height.unwrap_or(640).clamp(64, 1080);

    // The engine FOV is horizontal; Three.js cameras take vertical FOV.
    let aspect = width as f32 / height as f32;
    let horizontal_rad = pose.fov_deg.clamp(1.0, 179.0).to_radians();
    let vertical_fov_deg = (2.0 * ((horizontal_rad / 2.0).tan() / aspect).atan()).to_degrees();

    // The capture page attaches its camera to the player's interpolated
    // render state, so the eye always matches the drawn character.
    let attach = CaptureAttach {
        player_id: agent_id.to_string(),
        eye_height: clawblox::game::instance::PLAYER_EYE_HEIGHT,
        fov_deg: vertical_fov_deg,
    };
    let png = capture
        .capture(&attach, width, height)
        .await
        .map_err(|e| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e))?;

    Ok(([(axum::http::header::CONTENT_TYPE, "image/png")], png).into_response())
}

fn resolve_local_asset_url(url: &mut String, base_path: &str) {
    if let Some(path) = url.strip_prefix("asset://") {
        *url = join_base_path(base_path, &format!("/assets/{}", path));
        return;
    }
    if url.starts_with("/assets/") {
        *url = join_base_path(base_path, url);
    }
}

fn resolve_local_model_url_attr(
    attrs: &mut std::collections::HashMap<String, serde_json::Value>,
    base_path: &str,
) {
    if let Some(value) = attrs.get_mut("ModelUrl") {
        if let Some(path) = value.as_str().and_then(|url| url.strip_prefix("asset://")) {
            *value =
                serde_json::Value::String(join_base_path(base_path, &format!("/assets/{}", path)));
        }
    }
}

/// Resolve asset:// URLs to local /assets/ paths for local spectator observations.
fn resolve_local_spectator_assets(obs: &mut SpectatorObservation, base_path: &str) {
    for entity in &mut obs.entities {
        if let Some(ref mut url) = entity.model_url {
            resolve_local_asset_url(url, base_path);
        }
        if let Some(attrs) = entity.attributes.as_mut() {
            resolve_local_model_url_attr(attrs, base_path);
        }
    }
}

/// Resolve asset:// URLs to local /assets/ paths for local player observations.
fn resolve_local_player_assets(obs: &mut PlayerObservation, base_path: &str) {
    for entity in &mut obs.world.entities {
        if let Some(attrs) = entity.attributes.as_mut() {
            resolve_local_model_url_attr(attrs, base_path);
        }
    }
}

async fn local_spectate(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<SpectatorAccessQuery>,
) -> Result<Json<SpectatorObservation>, (axum::http::StatusCode, String)> {
    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }
    require_spectator_access(
        &state.spectator_token,
        &headers,
        query.spectator_token.as_deref(),
    )?;

    let mut observation =
        game::get_spectator_observation_for_instance(&state.game_handle, state.instance_id)
            .map_err(|e| (axum::http::StatusCode::NOT_FOUND, e))?;

    resolve_local_spectator_assets(&mut observation, &state.base_path);

    Ok(Json(observation))
}

async fn local_spectate_chat_messages(
    State(state): State<LocalState>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ChatMessagesQuery>,
) -> Result<Json<ChatMessagesResponse>, (axum::http::StatusCode, String)> {
    if let Some(err) = check_halted(&state) {
        return Err((axum::http::StatusCode::SERVICE_UNAVAILABLE, err));
    }
    require_spectator_access(
        &state.spectator_token,
        &headers,
        query.spectator_token.as_deref(),
    )?;

    let instance_id = query.instance_id.unwrap_or(state.instance_id);
    let limit = query
        .limit
        .unwrap_or(LOCAL_CHAT_DEFAULT_LIMIT)
        .clamp(1, LOCAL_CHAT_MAX_LIMIT);
    let after = query
        .after
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let messages = {
        let chat = state.chat_state.lock().unwrap();
        chat.list(instance_id, after, limit)
    };

    Ok(Json(ChatMessagesResponse { messages }))
}

async fn local_skill(State(state): State<LocalState>) -> impl IntoResponse {
    match state.skill_md {
        Some(content) => (
            axum::http::StatusCode::OK,
            [("content-type", "text/markdown; charset=utf-8")],
            content,
        ),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            [("content-type", "text/plain")],
            "No API.md or SKILL.md found".to_string(),
        ),
    }
}

async fn local_renderer_manifest(State(state): State<LocalState>) -> Json<LocalRendererManifest> {
    Json(state.renderer_manifest)
}

async fn local_spectate_ws(
    State(state): State<LocalState>,
    Query(params): Query<SpectateWsQuery>,
    headers: axum::http::HeaderMap,
    ws: WebSocketUpgrade,
) -> Result<impl IntoResponse, (axum::http::StatusCode, String)> {
    require_spectator_access(
        &state.spectator_token,
        &headers,
        params.spectator_token.as_deref(),
    )?;
    let role = SpectateRole::from_param(params.role.as_deref());
    Ok(ws.on_upgrade(move |socket| handle_spectate_ws(socket, state, role)))
}

#[derive(Debug, Deserialize)]
struct SpectateWsQuery {
    role: Option<String>,
    spectator_token: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpectateRole {
    Actor,
    Spectator,
}

impl SpectateRole {
    fn from_param(role: Option<&str>) -> Self {
        match role.map(|s| s.trim().to_ascii_lowercase()) {
            Some(s) if s == "actor" => Self::Actor,
            _ => Self::Spectator,
        }
    }
}

async fn handle_spectate_ws(socket: WebSocket, state: LocalState, role: SpectateRole) {
    let (mut sender, mut receiver) = socket.split();
    let mut audio_rx = state.audio_tx.subscribe();
    let mut speech_rx = state.speech_tx.subscribe();
    let mut speech_progress_rx = state.speech_progress_tx.subscribe();
    let mut playback_done_rx = state.playback_done_tx.subscribe();
    let mut chat_rx = state.chat_tx.subscribe();

    let mut tick_interval = interval(Duration::from_millis(33));
    let mut last_tick: u64 = 0;
    let mut same_tick_count: u32 = 0;
    let mut last_observation: Option<SpectatorObservation> = None;
    let mut known_entity_ids: HashSet<u32> = HashSet::new();
    let mut replication_revision: u64 = 0;

    loop {
        tokio::select! {
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(data))) => {
                        if sender.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Text(_))) => {}
                    _ => {}
                }
            }

            chunk = audio_rx.recv() => {
                if let Ok(chunk) = chunk {
                    let json = serde_json::to_string(&serde_json::json!({
                        "type": "audio_chunk",
                        "speaker": chunk.speaker,
                        "stream_id": chunk.stream_id,
                        "seq": chunk.seq,
                        "data": chunk.data,
                        "done": chunk.done,
                    })).unwrap_or_default();
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            speech = speech_rx.recv() => {
                if let Ok(speech) = speech {
                    let json = serde_json::to_string(&serde_json::json!({
                        "type": "speech",
                        "speaker": speech.speaker,
                        "text": speech.text,
                        "seq": speech.seq,
                    })).unwrap_or_default();
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            speech_progress = speech_progress_rx.recv() => {
                if let Ok(progress) = speech_progress {
                    if role == SpectateRole::Spectator {
                        let json = serde_json::to_string(&serde_json::json!({
                            "type": "speech_progress",
                            "speaker": progress.speaker,
                            "stream_id": progress.stream_id,
                            "text": progress.text,
                            "seq": progress.seq,
                        })).unwrap_or_default();
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                }
            }

            playback_done = playback_done_rx.recv() => {
                if let Ok(event) = playback_done {
                    let json = serde_json::to_string(&serde_json::json!({
                        "type": "playback_done",
                        "speaker": event.speaker,
                        "stream_id": event.stream_id,
                        "seq": event.seq,
                    })).unwrap_or_default();
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            chat = chat_rx.recv() => {
                if let Ok(chat) = chat {
                    let json = serde_json::to_string(&serde_json::json!({
                        "type": "chat_message",
                        "id": chat.id,
                        "instance_id": chat.instance_id,
                        "agent_id": chat.agent_id,
                        "agent_name": chat.agent_name,
                        "message_type": chat.message_type,
                        "content": chat.content,
                        "created_at": chat.created_at,
                        "delivery_mode": chat.delivery_mode,
                        "hearing_radius": chat.hearing_radius,
                        "heard_by": chat.heard_by,
                        "audibility": chat.audibility,
                    })).unwrap_or_default();
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }

            _ = tick_interval.tick() => {
                if last_observation.is_none() {
                    match game::get_spectator_observation_for_instance(&state.game_handle, state.instance_id) {
                        Ok(mut obs) => {
                            resolve_local_spectator_assets(&mut obs, &state.base_path);
                            maybe_publish_authoritative_speech_events(&state, &obs.recent_events);
                            known_entity_ids = obs.entities.iter().map(|entity| entity.id).collect();
                            replication_revision = game::replication_revision_for_instance(&state.game_handle, state.instance_id)
                                .unwrap_or(replication_revision);
                            last_tick = obs.tick;
                            same_tick_count = 0;
                            if let Ok(json) = serde_json::to_vec(&obs) {
                                let msg = Message::Text(String::from_utf8_lossy(&json).into_owned().into());
                                if sender.send(msg).await.is_err() {
                                    break;
                                }
                            }
                            last_observation = Some(obs);
                        }
                        Err(_) => {
                            let _ = sender
                                .send(Message::Text(r#"{"error":"World ended"}"#.to_string().into()))
                                .await;
                            break;
                        }
                    };
                    continue;
                }

                match game::get_replication_observation_update_for_instance(
                    &state.game_handle,
                    state.instance_id,
                    &known_entity_ids,
                    replication_revision,
                ) {
                    Ok(mut update) => {
                        resolve_local_spectator_assets(&mut update.observation, &state.base_path);
                        maybe_publish_authoritative_speech_events(&state, &update.observation.recent_events);

                        let should_send = if update.observation.tick != last_tick {
                            last_tick = update.observation.tick;
                            same_tick_count = 0;
                            true
                        } else {
                            same_tick_count += 1;
                            if same_tick_count >= 5 {
                                same_tick_count = 0;
                                true
                            } else {
                                false
                            }
                        };

                        if should_send {
                            let Some(base) = last_observation.as_mut() else {
                                continue;
                            };
                            let delta = compute_live_replication_delta(
                                base,
                                &update.observation,
                                &update.removed_entity_ids,
                            );
                            if let Ok(json) = serde_json::to_vec(&delta) {
                                let msg = Message::Text(String::from_utf8_lossy(&json).into_owned().into());
                                if sender.send(msg).await.is_err() {
                                    break;
                                }
                            }

                            for entity in &update.observation.entities {
                                known_entity_ids.insert(entity.id);
                            }
                            for id in &update.removed_entity_ids {
                                known_entity_ids.remove(id);
                            }
                            replication_revision = update.latest_revision;
                            apply_replication_update_to_observation(
                                base,
                                update.observation,
                                &update.removed_entity_ids,
                            );
                        }
                    }
                    Err(_) => {
                        let _ = sender
                            .send(Message::Text(r#"{"error":"World ended"}"#.to_string().into()))
                            .await;
                        break;
                    }
                }
            }
        }
    }
}
