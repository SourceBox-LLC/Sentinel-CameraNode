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
//! Setup wizard for Sentinel CloudNode
//!
//! Beautiful animated terminal-based setup experience

pub mod animations;
// `ffmpeg_installer` removed in v0.1.35. CloudNode now uses the system
// FFmpeg (winget / brew / apt), not a bundled copy. The setup wizard's
// pre-flight FFmpeg handling lives in `tui::prompt_and_install_ffmpeg`.
pub mod platform;
pub mod recovery;
pub mod tui;
pub mod ui;
pub mod validator;
pub mod wsl_preflight;

use anyhow::Result;
use std::path::PathBuf;

pub use animations::{
    animate_rainbow_text, clear_screen, draw_box, draw_expanding_border, fade_in, fade_in_lines,
    print_centered, pulse_text, rainbow_text, rainbow_text_offset, show_confetti,
    show_mini_celebration, Spinner,
};
pub use platform::PlatformInfo;
pub use recovery::{
    prompt_for_reset, show_plan_limit_hit, show_registration_error, show_success, show_warning,
    RegistrationError, ResetOutcome,
};
pub use tui::run_tui_setup;
pub use validator::ValidationResult;

/// Setup configuration
#[derive(Debug, Clone)]
pub struct SetupConfig {
    /// Local-only vs Connected install — chosen at the very first
    /// wizard prompt (see `tui::configure_node`). In Local mode the
    /// `node_id` / `api_key` / `api_url` fields stay empty strings;
    /// `save_config_to_database` (in `tui.rs`) detects mode=Local and
    /// skips the Cloud-coupled DB writes.
    pub mode: crate::config::NodeMode,
    pub node_id: String,
    pub api_key: String,
    pub api_url: String,
    pub deployment_method: DeploymentMethod,
    pub output_dir: PathBuf,
    pub auto_start: bool,
    /// Operator-chosen cap for recordings + snapshots, in GB.  Asked
    /// during configure_node with a disk-aware default (see
    /// `storage::suggested_max_gb`).  64 GB if the wizard couldn't
    /// read disk info — same as the historical hardcoded default.
    pub max_size_gb: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeploymentMethod {
    WindowsNative,
    WSL2,
    LinuxNative,
    Docker,
}

/// Run interactive setup wizard
/// Returns: Ok(true) if should auto-start, Ok(false) if should wait for user
pub fn run_setup() -> Result<bool> {
    // Always try TUI setup - works when double-clicked on Windows
    // The TUI uses inquire which handles both terminal and pipe scenarios
    match tui::run_tui_setup() {
        Ok(auto_start) => Ok(auto_start),
        Err(e) => {
            eprintln!("\n  Interactive setup failed: {}", e);

            // Specialise the hint based on what actually went wrong. The
            // catch-all "you're not running in a real terminal" line we
            // used to print here was misleading whenever the failure was
            // anything else — and the most common "anything else" by far
            // is FFmpeg being missing, because the camera-detection step
            // shells out to `ffmpeg -list_devices` and surfaces an
            // Io::NotFound that propagates here as Error::Io.
            //
            // We can't be 100% certain a NotFound came from ffmpeg (any
            // missing file would qualify), but at this point in the wizard
            // ffmpeg is the only external program we've tried to invoke,
            // so the heuristic is safe.
            // run_tui_setup returns anyhow::Result, so `e` here is an
            // anyhow::Error wrapping (potentially) a crate::Error wrapping
            // (potentially) the std::io::Error from `Command::new("ffmpeg")
            // .output()`. Walk the chain and look for any std::io::Error
            // with kind = NotFound — that's the most reliable signal,
            // independent of how many wrappers it went through.
            let is_not_found = e.chain()
                .filter_map(|inner| inner.downcast_ref::<std::io::Error>())
                .any(|io_err| io_err.kind() == std::io::ErrorKind::NotFound);

            if is_not_found {
                eprintln!();
                eprintln!("  A required external program was not found. The most likely");
                eprintln!("  cause is that FFmpeg is missing — CloudNode shells out to");
                eprintln!("  ffmpeg for camera detection and HLS encoding.");
                eprintln!();
                eprintln!("  Install FFmpeg, then re-run setup in a fresh terminal so");
                eprintln!("  the new PATH takes effect:");
                eprintln!();
                #[cfg(target_os = "windows")]
                eprintln!("    winget install Gyan.FFmpeg");
                #[cfg(target_os = "macos")]
                eprintln!("    brew install ffmpeg");
                #[cfg(target_os = "linux")]
                eprintln!("    sudo apt install ffmpeg     # Debian/Ubuntu");
                eprintln!();
                eprintln!("  Then:");
                eprintln!("    sourcebox-sentry-cloudnode setup");
            } else {
                // Generic fallback. Avoid the old "without a proper terminal"
                // claim — that was only true for one of N possible causes
                // and confused users hitting the others.
                eprintln!();
                eprintln!("  To retry, open a terminal and run:");
                eprintln!("    sourcebox-sentry-cloudnode setup");
                eprintln!();
                eprintln!("  If the error above is unclear, run with debug logs:");
                eprintln!("    RUST_LOG=debug sourcebox-sentry-cloudnode setup");
            }

            // Pause on Windows so user sees the error before window closes
            #[cfg(target_os = "windows")]
            {
                eprintln!("\n  Press Enter to exit...");
                let _ = std::io::stdin().read_line(&mut String::new());
            }

            std::process::exit(1);
        }
    }
}

/// Run non-interactive quick setup with pre-supplied credentials.
///
/// Validates credentials against Command Center, saves config to DB,
/// auto-detects GPU encoder, and creates data directories — all without
/// any user prompts. Designed to be invoked as:
///
///   sourcebox-sentry-cloudnode setup --url <URL> --node-id <ID> --key <KEY>
pub fn run_quick_setup(api_url: &str, node_id: &str, api_key: &str) -> Result<()> {
    use colored::Colorize;

    println!();
    println!(
        "  {} Sentinel CloudNode — Quick Setup",
        "⚡".cyan()
    );
    println!("  ────────────────────────────────────────");
    println!();

    // ── Validate inputs ──────────────────────────────────────────
    if node_id.len() != 8 || !node_id.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("Invalid node ID: must be 8 hex characters (got '{}')", node_id);
    }
    let parts: Vec<&str> = api_key.split('-').collect();
    if parts.len() != 5 {
        anyhow::bail!("Invalid API key: must be UUID format (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx)");
    }
    if !api_url.starts_with("http://") && !api_url.starts_with("https://") {
        anyhow::bail!("Invalid URL: must start with http:// or https://");
    }

    // ── Validate connection ──────────────────────────────────────
    print!("  Validating credentials... ");
    let rt = tokio::runtime::Runtime::new()?;
    let validation = rt.block_on(validator::validate_api_connection(api_url, node_id, api_key))?;

    if !validation.is_valid {
        println!("{}", "FAILED".red().bold());
        if let Some(msg) = &validation.error_message {
            for line in msg.lines() {
                eprintln!("  {}", line);
            }
        }
        std::process::exit(1);
    }

    let node_name = validation
        .node_name
        .as_deref()
        .unwrap_or(node_id);
    println!("{} ({})", "OK".green().bold(), node_name);

    // ── Save config to database ──────────────────────────────────
    print!("  Saving configuration...   ");
    // `output_dir` is the install root (where the binary + bundled
    // ffmpeg live). The DB path comes from `paths::config_db_path()`
    // so a Windows-Service install lands the DB under %ProgramData%
    // \SourceBoxSentry\node.db rather than next to the exe in Program Files
    // (which a service running as LocalSystem would still be able to
    // write to, but conventionally Program Files is read-only).
    let output_dir = std::env::current_dir()?;
    let db_path = crate::paths::config_db_path();
    std::fs::create_dir_all(db_path.parent().unwrap())?;

    let db = crate::storage::NodeDatabase::new(&db_path)
        .map_err(|e| anyhow::anyhow!("DB error: {}", e))?;

    // Read-modify-write: quick setup re-runs (key rotation, scripted
    // re-enrol) must not reset operator tuning.  Building from
    // `..Default::default()` here silently re-armed the 64 GB storage
    // cap on small-disk Pis and reverted fps/motion/`/set` adjustments.
    // Only the fields quick setup actually collects are overwritten.
    let mut app_config = if db.has_config() {
        crate::config::Config::load_from_db(&db)
            .unwrap_or_else(|_| crate::config::Config::default())
    } else {
        crate::config::Config::default()
    };
    // Quick setup IS the Connected-mode enrolment path (it just
    // validated CC credentials) — make the mode explicit rather than
    // inheriting whatever was persisted before.
    app_config.mode = crate::config::NodeMode::Connected;
    // Bind is MODE POLICY (mirrors the interactive wizard): Connected
    // keeps the safe loopback-only bind.  Without this, re-enrolling a
    // former Local-mode install preserved its 0.0.0.0 bind — leaving
    // the UNAUTHENTICATED local HTTP API (snapshots, recordings, live
    // HLS) exposed to the LAN on a node whose remote surface is now
    // the Command Center.
    app_config.server.bind = crate::config::ServerConfig::default().bind;
    app_config.node.node_id = Some(node_id.to_string());
    app_config.cloud.api_url = api_url.to_string();
    app_config.cloud.api_key = api_key.to_string();
    app_config
        .save_to_db(&db)
        .map_err(|e| anyhow::anyhow!("Config save error: {}", e))?;
    println!("{}", "OK".green().bold());

    // ── Auto-detect GPU encoder ──────────────────────────────────
    print!("  Detecting video encoder... ");

    // Delegate to the shared lookup so this non-interactive setup
    // path matches what `run_tui_setup` and the running node both use
    // — including the new data-dir bundled-copy candidate that the
    // auto-installer drops ffmpeg into.
    //
    // Note: the non-interactive path doesn't auto-install ffmpeg (no
    // way to prompt). If ffmpeg is missing, encoder detection returns
    // None and we fall through to libx264 logged as the encoder; the
    // node will fail later at camera detection. Scripted callers are
    // expected to install ffmpeg as a separate step before invoking
    // `setup --url ... --node-id ... --key ...`.
    let _ = &output_dir;
    let ffmpeg_path = crate::streaming::find_ffmpeg();

    let hw_encoder =
        crate::streaming::hls_generator::HlsGenerator::detect_hw_encoder(&ffmpeg_path);

    match &hw_encoder {
        Some(enc) => {
            let label = match enc.as_str() {
                "h264_nvenc" => "NVIDIA NVENC (GPU)",
                "h264_qsv" => "Intel Quick Sync (GPU)",
                "h264_amf" => "AMD AMF (GPU)",
                "h264_v4l2m2m" => "V4L2 M2M (Hardware)",
                other => other,
            };
            db.set_config("encoder", enc)
                .map_err(|e| anyhow::anyhow!("Config save error: {}", e))?;
            println!("{}", label.green().bold());
        }
        None => {
            db.set_config("encoder", "libx264")
                .map_err(|e| anyhow::anyhow!("Config save error: {}", e))?;
            println!("{}", "libx264 (CPU)".yellow());
        }
    }

    // ── Create data directories ──────────────────────────────────
    print!("  Creating directories...   ");
    std::fs::create_dir_all(crate::paths::data_dir().join("hls"))?;
    println!("{}", "OK".green().bold());

    // ── Check FFmpeg availability ────────────────────────────────
    let has_ffmpeg = platform::check_ffmpeg().unwrap_or(false);
    if !has_ffmpeg {
        println!();
        println!(
            "  {} FFmpeg not found — install it before starting.",
            "⚠".yellow()
        );
        #[cfg(target_os = "linux")]
        println!("    sudo apt install ffmpeg");
        #[cfg(target_os = "macos")]
        println!("    brew install ffmpeg");
    }

    // ── Done ─────────────────────────────────────────────────────
    println!();
    println!("  {} Setup complete — starting CloudNode...", "✓".green().bold());
    println!();

    Ok(())
}
