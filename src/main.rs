// Sentinel CloudNode - Camera streaming node for Sentinel Command Center
// Copyright (C) 2026  SourceBox LLC
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

//! Sentinel CloudNode - Camera streaming node for Sentinel Command Center
//!
//! This node runs on a local device (Raspberry Pi, Mini PC, etc.) and:
//! - Detects USB cameras
//! - Captures video frames
//! - Streams to Sentinel Command Center
//! - Stores recordings locally
//! - Serves recordings via HTTP

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use colored::Colorize;
use sourcebox_sentry_cloudnode::{Config, Node, Result};
use sourcebox_sentry_cloudnode::logging::DashboardLayer;
use tracing::{info, Level};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Parser)]
#[command(name = "sourcebox-sentry-cloudnode")]
#[command(version)]
#[command(about = "Sentinel camera node - stream cameras to Sentinel")]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,
    
    /// Path to config file (YAML)
    #[arg(short, long, env = "SOURCEBOX_SENTRY_CONFIG")]
    config: Option<String>,

    /// Node ID (required for registration)
    #[arg(long, env = "SOURCEBOX_SENTRY_NODE_ID")]
    node_id: Option<String>,

    /// Organization API key (overrides config)
    #[arg(long, env = "SOURCEBOX_SENTRY_API_KEY")]
    api_key: Option<String>,

    /// Command Center URL (overrides config)
    #[arg(long, env = "SOURCEBOX_SENTRY_API_URL")]
    api_url: Option<String>,

    /// Run once and exit (for testing)
    #[arg(long)]
    once: bool,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, env = "RUST_LOG", default_value = "info")]
    log_level: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Start CloudNode (default)
    Run {
        /// Node ID (required for registration)
        #[arg(long, env = "SOURCEBOX_SENTRY_NODE_ID")]
        node_id: Option<String>,
        
        /// Organization API key (overrides config)
        #[arg(long, env = "SOURCEBOX_SENTRY_API_KEY")]
        api_key: Option<String>,
        
        /// Command Center URL (overrides config)
        #[arg(long, env = "SOURCEBOX_SENTRY_API_URL")]
        api_url: Option<String>,

        /// Run once and exit (for testing)
        #[arg(long)]
        once: bool,
    },
    
    /// Setup CloudNode (interactive wizard, or one-command with --url/--node-id/--key)
    Setup {
        /// Command Center URL
        #[arg(long, env = "SOURCEBOX_SENTRY_API_URL")]
        url: Option<String>,

        /// Node ID from Command Center
        #[arg(long, env = "SOURCEBOX_SENTRY_NODE_ID")]
        node_id: Option<String>,

        /// API key from Command Center
        #[arg(long, env = "SOURCEBOX_SENTRY_API_KEY")]
        key: Option<String>,

        /// Bind the local HLS/dashboard server to the LAN (0.0.0.0)
        /// so Home Assistant on the same network can stream directly
        /// from this node.  Default (flag absent) keeps the safe
        /// loopback-only bind — the unauthenticated local API stays
        /// unreachable from other machines.  Re-run setup with or
        /// without the flag to flip it.
        #[arg(long, env = "SOURCEBOX_SENTRY_LAN_STREAMING")]
        lan_streaming: bool,
    },
    
    /// Uninstall CloudNode
    Uninstall {
        /// Force uninstall without confirmation
        #[arg(long)]
        force: bool,
    },

    /// Run as a Windows Service. Invoked by the Service Control Manager —
    /// not intended for direct use. The MSI registers
    /// `sourcebox-sentry-cloudnode service` as the service binary path.
    /// See src/service.rs for the SCM handshake details.
    #[command(hide = true)]
    Service,
}

fn main() -> ExitCode {
    let exit_code = match run() {
        Ok(()) => ExitCode::SUCCESS,
        // Already-reported errors came through a formatted TUI path
        // (e.g. show_registration_error); do not print a second debug line.
        Err(sourcebox_sentry_cloudnode::Error::AlreadyReported)
        | Err(sourcebox_sentry_cloudnode::Error::ResetRequested) => ExitCode::from(1),
        Err(e) => {
            eprintln!();
            eprintln!("  {} {}", "Error:".red().bold(), e);
            eprintln!();
            // CRITICAL: on Windows, the binary may be running in a
            // console window owned by us (Start menu shortcut, MSI
            // Finish-dialog cmd /c start, Explorer double-click).
            // When main returns, that console closes immediately —
            // the user never sees the error message printed above.
            //
            // Pause for input on Windows so the operator can actually
            // read what went wrong before the window vanishes. PowerShell
            // / cmd users (who own their own shell) hit Enter once
            // and move on; that minor friction is the price for not
            // throwing diagnostic info into a window-close vacuum.
            //
            // Skip the pause for AlreadyReported / ResetRequested
            // because those are intentional flows (the TUI already
            // showed the user what happened; we're just exiting with
            // code 1 to signal the outer launcher).
            #[cfg(target_os = "windows")]
            pause_on_exit();
            ExitCode::from(1)
        }
    };
    exit_code
}

fn run() -> Result<()> {
    // Enable Virtual Terminal Processing on Windows so the setup TUI's
    // ANSI escape codes (colors, cursor moves, panel borders) are
    // interpreted by the console instead of printed as literal
    // `<ESC>[36m` garbage. Modern Windows Terminal auto-enables this,
    // but classic conhost — which is what `cmd.exe /c start` opens
    // (the console the MSI's LaunchSetup CustomAction spawns the
    // setup wizard in) — leaves it OFF unless the process explicitly
    // calls SetConsoleMode with ENABLE_VIRTUAL_TERMINAL_PROCESSING.
    //
    // Without this, the setup wizard's rainbow header, panel borders,
    // and the live dashboard's box-drawing all show up as a wall of
    // `←[1;36m` codes that looks like a crash. The `colored` crate
    // wraps the underlying SetConsoleMode call. Best-effort: on the
    // rare host where it fails (extremely old Windows, output
    // redirected to a file), we silently fall through and the user
    // gets uncolored text — still readable, just less pretty.
    #[cfg(target_os = "windows")]
    {
        let _ = colored::control::set_virtual_terminal(true);
    }

    // Load .env file if it exists (legacy — config now stored in data/node.db).
    // Parsing args before the terminal-check is intentional: the `service`
    // subcommand must short-circuit BEFORE `launch_in_terminal()` would
    // try to spawn a console (the SCM has no console for us to attach to,
    // and any spawned cmd window gets orphaned anyway).
    dotenvy::dotenv().ok();
    let args = Args::parse();

    // ── Windows Service short-circuit ────────────────────────────────
    // SCM invokes us as `sourcebox-sentry-cloudnode.exe service`. From here we
    // hand off to the windows-service dispatcher which blocks until the
    // service exits. Don't reach the terminal-check or interactive flow.
    //
    // Install a panic hook + dispatcher-error capture BEFORE doing anything
    // else: if the dispatcher itself fails, or anything in the service
    // body panics across the FFI boundary, the only error-reporting paths
    // are stderr (no console) and SCM's "Win32 exit code 1" (useless).
    // Writing to fatal-startup-error.txt before that happens means an
    // operator can actually diagnose what went wrong.
    #[cfg(target_os = "windows")]
    if matches!(args.command, Some(Commands::Service)) {
        // Best-effort write to ProgramData fatal-startup-error.txt with
        // the same dual-fallback strategy service.rs uses, but reachable
        // even when the service-body never ran.
        fn write_service_diag(message: &str) {
            use std::io::Write;
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let line = format!(
                "[{}] sourcebox-sentry-cloudnode service path: {}\n",
                timestamp, message
            );
            let candidates = [
                std::path::PathBuf::from(r"C:\ProgramData\SourceBoxSentry")
                    .join("fatal-startup-error.txt"),
                std::env::temp_dir()
                    .join("sourcebox-sentry-cloudnode-fatal-startup-error.txt"),
            ];
            for path in &candidates {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    if f.write_all(line.as_bytes()).is_ok() {
                        return;
                    }
                }
            }
        }

        write_service_diag(&format!(
            "service subcommand reached main.rs (v{})",
            env!("CARGO_PKG_VERSION")
        ));

        // Panic hook: rust panics across the FFI boundary inside
        // service_dispatcher are UB and can abort the process before
        // service_main's own error handler ever runs. Catch panics
        // here and dump them to disk so the operator has SOMETHING.
        std::panic::set_hook(Box::new(|info| {
            let msg = if let Some(s) = info.payload().downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = info.payload().downcast_ref::<String>() {
                s.clone()
            } else {
                "unknown panic payload".to_string()
            };
            let location = info
                .location()
                .map(|l| format!(" at {}:{}:{}", l.file(), l.line(), l.column()))
                .unwrap_or_default();
            write_service_diag(&format!("PANIC: {}{}", msg, location));
        }));

        match sourcebox_sentry_cloudnode::service::run() {
            Ok(()) => {
                write_service_diag("service::run returned Ok — clean shutdown");
                return Ok(());
            }
            Err(e) => {
                let msg = format!("service::run returned Err: {}", e);
                write_service_diag(&msg);
                return Err(sourcebox_sentry_cloudnode::Error::Unknown(msg));
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    if matches!(args.command, Some(Commands::Service)) {
        return Err(sourcebox_sentry_cloudnode::Error::Config(
            "The `service` subcommand is Windows-only — \
             use systemd / launchd / your platform's service manager \
             to run CloudNode as a daemon on Linux/macOS."
                .to_string(),
        ));
    }

    // On Windows, check if we have a proper terminal attached
    // If not (double-clicked from Explorer), launch in a new terminal window
    #[cfg(target_os = "windows")]
    {
        if !has_terminal() {
            // Try to launch in a new terminal window
            if let Ok(true) = launch_in_terminal() {
                // Successfully launched in new terminal - exit cleanly
                // The new window will run the setup
                return Ok(());
            }
            // If launching failed, we'll continue in this process
            // but show a clear message first
            show_terminal_required_message();
            // Pause so user sees the message
            pause_on_exit();
        }
    }

    // Determine if we're launching setup or running the node.
    // IMPORTANT: Do NOT initialize the tracing subscriber until AFTER setup
    // completes — tracing logs would bleed into the TUI and destroy the layout.
    let needs_setup = match &args.command {
        Some(Commands::Setup { .. }) => true,
        Some(Commands::Run { .. }) | None => {
            !Config::load(args.config.as_deref())
                .map(|c| {
                    // Local-mode nodes don't have CC credentials by
                    // design — having mode=Local set is itself the
                    // "configured" signal.  Connected-mode still
                    // requires both api_key and node_id; without them
                    // the runtime has nothing to register or
                    // heartbeat with.
                    if c.mode.is_local() {
                        true
                    } else {
                        !c.cloud.api_key.is_empty() && c.node.node_id.is_some()
                    }
                })
                .unwrap_or(false)
        }
        _ => false,
    };

    if needs_setup {
        // Check if this is a quick (non-interactive) setup with all args provided
        let quick_args = match &args.command {
            Some(Commands::Setup { url, node_id, key, lan_streaming }) => {
                match (url.as_deref(), node_id.as_deref(), key.as_deref()) {
                    (Some(u), Some(n), Some(k)) => {
                        Some((u.to_string(), n.to_string(), k.to_string(), *lan_streaming))
                    }
                    // Partial args — tell the user what's missing.
                    // `--lan-streaming` alone counts as partial too: the
                    // interactive wizard has no LAN-streaming step, so
                    // falling through would SILENTLY drop the flag the
                    // operator explicitly asked for (flag-absent is the
                    // only state the wizard can't be wrong about).
                    _ if url.is_some() || node_id.is_some() || key.is_some() || *lan_streaming => {
                        eprintln!("Error: Quick setup requires all three flags: --url, --node-id, and --key");
                        eprintln!("  (--lan-streaming only works together with them — the interactive wizard has no LAN-streaming step)");
                        eprintln!("  Example: sourcebox-sentry-cloudnode setup --url https://... --node-id abc12345 --key xxxxxxxx-... [--lan-streaming]");
                        std::process::exit(1);
                    }
                    _ => None,
                }
            }
            _ => None,
        };

        if let Some((url, node_id, key, lan_streaming)) = quick_args {
            // Non-interactive quick setup. Save config, then run the
            // node in the foreground in the same console.
            init_logging(&args.log_level);
            sourcebox_sentry_cloudnode::setup::run_quick_setup(&url, &node_id, &key, lan_streaming)?;
            return run_cloudnode(None, None, None, args.once, args.config);
        }

        // Interactive TUI setup with logging completely suppressed.
        //
        // After the wizard completes, hand control to `run_cloudnode`
        // which takes over the same console with the live TUI dashboard
        // (cameras, logs, streaming counters).  Foreground TUI is the
        // recommended runtime on every platform (cargo-build, install.sh,
        // and now the MSI Start menu shortcut).  The MSI still
        // registers a Windows Service named `SourceBoxSentryCloudNode`
        // for unattended-operation use cases, but it's manual-start by
        // default — see README "Running as a Windows Service".
        let auto_start = sourcebox_sentry_cloudnode::setup::run_setup()?;

        if !auto_start {
            println!("\n  Press Enter to start CloudNode...");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
        }
        // Now safe to init logging before running the node.
        init_logging(&args.log_level);
        return run_cloudnode(args.node_id, args.api_key, args.api_url, args.once, args.config);
    }

    // No setup needed — init logging and handle remaining subcommands.
    init_logging(&args.log_level);

    match args.command {
        Some(Commands::Run { node_id, api_key, api_url, once }) => {
            run_cloudnode(node_id.or(args.node_id), api_key.or(args.api_key), api_url.or(args.api_url), once, args.config)?;
        }
        Some(Commands::Uninstall { force }) => {
            uninstall_cloudnode(force)?;
        }
        Some(Commands::Setup { .. }) => {
            // Already handled above via needs_setup path.
        }
        Some(Commands::Service) => {
            // Already handled at the top of run() via the Windows
            // short-circuit. Reaching this arm means the cfg gate
            // skipped (non-Windows) — but we already returned an
            // error for that case, so this branch is unreachable.
            unreachable!("Service subcommand handled before reaching this match");
        }
        None => {
            // Bare invocation with credentials already on disk: just
            // run the node in the foreground. Same path as cargo-build
            // / Linux / Docker installs already use. The TUI dashboard
            // takes over the console.
            //
            // Print a clear "starting" banner before run_cloudnode so
            // the user has visual feedback that the binary is doing
            // something, even if Node::new takes a couple seconds to
            // build the tokio runtime + open the SQLite DB + register
            // with Command Center. Without this, the console is
            // blank for ~1-3 seconds before the dashboard takes over,
            // which feels like the binary hung.
            use colored::Colorize;
            println!();
            println!("  {}  Loading Sentinel CloudNode...", "📡".cyan());
            println!("  {}", "Configuration found. Starting camera node...".dimmed());
            println!();
            run_cloudnode(args.node_id, args.api_key, args.api_url, args.once, args.config)?;
        }
    }

    Ok(())
}

fn run_cloudnode(
    node_id: Option<String>,
    api_key: Option<String>,
    api_url: Option<String>,
    once: bool,
    config_path: Option<String>,
) -> Result<()> {
    info!("Starting Sentinel CloudNode v{}", env!("CARGO_PKG_VERSION"));

    // Retry loop: if the user confirms a credential reset after a registration
    // failure, the node returns `Error::ResetRequested`. We then re-run the
    // setup wizard and loop to re-attempt with the fresh credentials. Ok /
    // other errors exit immediately.
    loop {
        match run_cloudnode_once(
            node_id.clone(),
            api_key.clone(),
            api_url.clone(),
            once,
            config_path.clone(),
        ) {
            Err(sourcebox_sentry_cloudnode::Error::ResetRequested) => {
                // Unhook the previous node's dashboard from the tracing layer
                // so the setup wizard's events don't flow to an orphaned TUI.
                sourcebox_sentry_cloudnode::logging::clear_dashboard();
                // Relaunch the interactive setup wizard synchronously. On
                // success it writes fresh credentials to data/node.db, which
                // the next loop iteration picks up via Config::load.
                sourcebox_sentry_cloudnode::setup::run_setup()?;
                continue;
            }
            other => return other,
        }
    }
}

fn run_cloudnode_once(
    node_id: Option<String>,
    api_key: Option<String>,
    api_url: Option<String>,
    once: bool,
    config_path: Option<String>,
) -> Result<()> {
    // Load configuration
    let config = Config::load(config_path.as_deref())?;

    // Apply CLI overrides
    let config = config.with_overrides(sourcebox_sentry_cloudnode::config::CliOverrides {
        node_id,
        api_key,
        api_url,
    });

    // Validate configuration.  Local-mode nodes don't have CC
    // credentials and don't need them — `run_internal` skips every
    // CC-coupled path when mode=Local (registration, heartbeat, WS,
    // segment push).  Only enforce the Connected-mode requirements
    // when we're actually going to talk to a Command Center.
    if config.mode.is_connected() {
        if config.cloud.api_key.is_empty() {
            return Err(sourcebox_sentry_cloudnode::Error::Config(
                "API key required. Set SOURCEBOX_SENTRY_API_KEY env var or use --api-key flag".to_string()
            ));
        }

        if config.node.node_id.is_none() {
            return Err(sourcebox_sentry_cloudnode::Error::Config(
                "Node ID required. Set SOURCEBOX_SENTRY_NODE_ID env var or use --node-id flag".to_string()
            ));
        }
    }

    info!("Node name: {}", config.node.name);
    info!("API URL: {}", config.cloud.api_url);

    // Create and run node on its own tokio runtime. If this returns
    // ResetRequested, the runtime drops here and the outer loop re-enters
    // with a fresh one.
    let rt = tokio::runtime::Runtime::new()?;

    rt.block_on(async {
        let node = Node::new(config).await?;

        if once {
            node.run_once().await?;
        } else {
            node.run().await?;
        }

        Ok(())
    })
}

fn init_logging(log_level: &str) {
    let level = match log_level.to_lowercase().as_str() {
        "trace" => Level::TRACE,
        "debug" => Level::DEBUG,
        "warn"  => Level::WARN,
        "error" => Level::ERROR,
        _       => Level::INFO,
    };

    // Route tracing events through DashboardLayer → TUI + SQLite.
    // Before a Dashboard is installed via logging::set_dashboard(), events
    // are silently discarded (startup messages use println! directly).
    let filter = tracing_subscriber::filter::LevelFilter::from_level(level);

    tracing_subscriber::registry()
        .with(filter)
        .with(DashboardLayer)
        .init();
}

/// Detect whether the running binary was installed by the Windows MSI.
///
/// Heuristic: the MSI installs `sourcebox-sentry-cloudnode.exe` under
/// `C:\Program Files\Sentinel CloudNode\` (or the `(x86)` mirror
/// on 32-bit emulation, though we only build x86_64 today). Legacy
/// v0.1.x installs landed under `C:\Program Files\OpenSentry CloudNode\`
/// — both paths are matched below for diagnostic continuity.
///
/// Used by `uninstall_cloudnode` to redirect MSI-installed users to
/// Settings → Apps instead of running the dev-cleanup logic, which
/// would do nothing useful for an MSI install (cwd is unrelated to
/// the install path; the service stays registered; ProgramData isn't
/// touched).
#[cfg(target_os = "windows")]
fn is_msi_install() -> bool {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return false,
    };
    // Windows path comparison is case-insensitive; normalise to lowercase
    // before substring matching.
    //
    // Match the current install-dir name AND every legacy variant we've
    // shipped so the diagnostic stays accurate across rebrands.  Brand
    // history: OpenSentry → Sentinel → Sentinel.  Probing all
    // three is one extra string compare per setup invocation; tiny cost
    // for preserving the "you're on an old test install" signal.
    let path = exe.to_string_lossy().to_lowercase();
    path.contains(r"\program files\sentinel cloudnode\")
        || path.contains(r"\program files (x86)\sentinel cloudnode\")
        || path.contains(r"\program files\sourcebox sentry cloudnode\")
        || path.contains(r"\program files (x86)\sourcebox sentry cloudnode\")
        || path.contains(r"\program files\opensentry cloudnode\")
        || path.contains(r"\program files (x86)\opensentry cloudnode\")
}

#[cfg(not(target_os = "windows"))]
fn is_msi_install() -> bool {
    // No MSI on Linux/macOS — `cloudnode uninstall` always falls
    // through to the dev-cleanup path on those platforms.
    false
}


fn uninstall_cloudnode(force: bool) -> Result<()> {
    use colored::Colorize;

    // Detect the MSI-install case first and bail with a Settings →
    // Apps pointer. The dev-cleanup logic below is meaningful for
    // `cargo build` / `cargo install` users; for an MSI-installed
    // binary it would just remove unrelated files in the user's cwd
    // (or, worst case, fail to find anything and leave the user
    // wondering whether the uninstall worked).
    if is_msi_install() {
        println!();
        println!("  This is an MSI install — uninstall via Windows Settings:");
        println!();
        println!("    Settings → Apps → Installed apps → Sentinel CloudNode → Uninstall");
        println!();
        println!("  Settings → Apps cleanly stops the service, removes the binary,");
        println!("  removes the Windows Service registration, and (on a real");
        println!("  uninstall) wipes data under C:\\ProgramData\\SourceBoxSentry\\.");
        println!();

        // Best-effort: try to open Settings → Apps directly via the
        // ms-settings: URI scheme. Saves the user from clicking through
        // the Start menu. Falls through silently if the spawn fails —
        // the printed instructions above are still useful.
        #[cfg(target_os = "windows")]
        {
            let _ = std::process::Command::new("cmd")
                .args(["/c", "start", "ms-settings:appsfeatures"])
                .spawn();
        }

        return Ok(());
    }

    println!("{}", "╔════════════════════════════════════════════════════╗".red());
    println!("{}", "║          Sentinel CloudNode Uninstall           ║".red());
    println!("{}", "╚════════════════════════════════════════════════════╝".red());
    println!();

    // Find what to remove.  Use `paths::data_dir()` for the data
    // location — same resolver as runtime, so the uninstall finds
    // node.db wherever it actually lives (env var → ProgramData on
    // Windows → ./data on Linux/macOS).  Reading current_dir() here
    // would silently miss the data on every Windows install and
    // every Docker container, which is the same bug class as the
    // v0.1.39 saga in node/runner.rs.
    //
    // .env stays cwd-relative — it was always a legacy alongside-
    // the-binary file in the YAML era; cleanup target hasn't moved.
    //
    // No more `ffmpeg/` cleanup — bundled FFmpeg was retired in
    // v0.1.35.  System FFmpeg (winget / brew / apt) is owned by the
    // OS package manager and isn't ours to remove on uninstall.
    let env_path = std::env::current_dir()?.join(".env");
    let data_dir = sourcebox_sentry_cloudnode::paths::data_dir();

    println!("  The following will be removed:");
    if env_path.exists() {
        println!("    - {} (legacy)", env_path.display());
    }
    if data_dir.exists() {
        println!("    - {}", data_dir.display());
    }
    println!();

    if !force {
        use inquire::Confirm;

        let confirm = Confirm::new("Continue with uninstall?")
            .with_default(false)
            .prompt()
            .map_err(|e| anyhow::anyhow!("Prompt error: {}", e))?;

        if !confirm {
            println!("  Uninstall cancelled.");
            return Ok(());
        }
    }

    println!("  Removing files...");

    if env_path.exists() {
        std::fs::remove_file(&env_path)?;
        println!("  {} Removed legacy .env file", "✓".green());
    }

    if data_dir.exists() {
        std::fs::remove_dir_all(&data_dir)?;
        println!("  {} Removed data directory", "✓".green());
    }
    
    println!();
    println!("  {}", "Uninstall complete.".green());
    println!("  To reinstall:");
    println!("    {} sourcebox-sentry-cloudnode setup", "→".cyan());
    
    Ok(())
}

/// Check if we have a proper terminal attached
#[cfg(target_os = "windows")]
fn has_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Launch CloudNode in a new terminal window on Windows
/// Returns Ok(true) if successfully launched, Ok(false) if failed
#[cfg(target_os = "windows")]
fn launch_in_terminal() -> std::result::Result<bool, anyhow::Error> {
    use std::process::Command;
    
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("Failed to get executable path: {}", e))?;
    
    // Launch in a new cmd window with /K to keep it open after completion
    let result = Command::new("cmd")
        .args(["/C", "start", "cmd", "/K"])
        .arg(&exe)
        .current_dir(std::env::current_dir().unwrap_or_default())
        .spawn();

    match result {
        Ok(_) => Ok(true),
        Err(e) => {
            tracing::warn!("Failed to open terminal: {}", e);
            Ok(false)
        }
    }
}

/// Show a Windows message if terminal cannot be opened
#[cfg(target_os = "windows")]
fn show_terminal_required_message() {
    eprintln!();
    eprintln!("  Sentinel CloudNode");
    eprintln!("  ────────────────────────────────────────");
    eprintln!();
    eprintln!("  This application requires a terminal window.");
    eprintln!();
    eprintln!("  Opening in a new terminal window...");
    eprintln!();
}

/// Pause before exit on Windows to prevent window from closing immediately
#[cfg(target_os = "windows")]
fn pause_on_exit() {
    use std::io::{self, BufRead};
    
    // Give the new window time to start if we spawned one
    std::thread::sleep(std::time::Duration::from_secs(2));
    
    eprintln!();
    eprintln!("  Press Enter to continue...");
    let _ = io::stdin().lock().read_line(&mut String::new());
}
