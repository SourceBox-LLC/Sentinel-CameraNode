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
//! Terminal UI setup wizard

use std::thread;
use std::time::Duration;

use colored::Colorize;
use inquire::{validator::Validation, Confirm, Select, Text};

use anyhow::Result;

use super::animations::{
    animate_rainbow_text, clear_screen, draw_expanding_border, fade_in, rainbow_text_offset,
    show_confetti, show_mini_celebration, Spinner,
};
use super::platform::PlatformInfo;
use super::ui::{
    flush, panel_blank, panel_bottom, panel_check, panel_divider, panel_error, panel_kv, panel_mid,
    panel_row, panel_spinner_row, panel_sub, panel_top, panel_warn, StepState,
};
use super::{DeploymentMethod, SetupConfig};

// ─── Step definitions ────────────────────────────────────────────────────────

const STEPS: [&str; 5] = ["PREREQS", "CONFIGURE", "INSTALL", "VERIFY", "LAUNCH"];

fn steps_for(active: usize) -> Vec<(&'static str, StepState)> {
    STEPS
        .iter()
        .enumerate()
        .map(|(i, &label)| {
            let state = if i < active {
                StepState::Done
            } else if i == active {
                StepState::Active
            } else {
                StepState::Pending
            };
            (label, state)
        })
        .collect()
}

// ─── Entry point ─────────────────────────────────────────────────────────────

/// Run the interactive TUI setup wizard.
/// Returns Ok(true) if should auto-start, Ok(false) otherwise.
pub fn run_tui_setup() -> Result<bool> {
    show_animated_header()?;
    let platform = check_prerequisites()?;
    let config = configure_node(&platform)?;
    install_dependencies(&config, &platform)?;
    verify_setup(&config)?;
    show_success_screen(&config)?;
    Ok(config.auto_start)
}

// ─── Step 0: Header ──────────────────────────────────────────────────────────

fn show_animated_header() -> Result<()> {
    // Wordmark for the "Sentinel by SourceBox" brand.  Big block-letter
    // "SOURCEBOX" (the company) sits above the subtitle that names the
    // specific product ("Sentinel CloudNode Setup"), so the visual
    // hierarchy reads as "Sentinel CloudNode, by SourceBox".  Keeps
    // the banner narrow enough to fit a standard 80-col terminal
    // (~78 cols incl. indent).
    let header_lines = vec![
        "   ███████╗ ██████╗ ██╗   ██╗██████╗  ██████╗███████╗██████╗  ██████╗ ██╗  ██╗",
        "   ██╔════╝██╔═══██╗██║   ██║██╔══██╗██╔════╝██╔════╝██╔══██╗██╔═══██╗╚██╗██╔╝",
        "   ███████╗██║   ██║██║   ██║██████╔╝██║     █████╗  ██████╔╝██║   ██║ ╚███╔╝ ",
        "   ╚════██║██║   ██║██║   ██║██╔══██╗██║     ██╔══╝  ██╔══██╗██║   ██║ ██╔██╗ ",
        "   ███████║╚██████╔╝╚██████╔╝██║  ██║╚██████╗███████╗██████╔╝╚██████╔╝██╔╝ ██╗",
        "   ╚══════╝ ╚═════╝  ╚═════╝ ╚═╝  ╚═╝ ╚═════╝╚══════╝╚═════╝  ╚═════╝ ╚═╝  ╚═╝",
    ];

    for (i, line) in header_lines.iter().enumerate() {
        let colored = rainbow_text_offset(line, i % 6);
        println!("{}", colored);
        thread::sleep(Duration::from_millis(70));
    }

    println!();
    draw_expanding_border(Duration::from_millis(350))?;
    println!();
    fade_in(
        "    📹  Sentinel CloudNode Setup  —  Your camera, connected to the cloud.",
        Duration::from_millis(400),
    )?;
    thread::sleep(Duration::from_millis(250));
    println!();

    Ok(())
}

// ─── Step 1: Prerequisites ───────────────────────────────────────────────────

fn check_prerequisites() -> Result<PlatformInfo> {
    // Detect platform silently before opening the Step 1 panel — we
    // need to know whether to OFFER the auto-install (Windows) vs
    // hint at a package manager (Linux/macOS) before the user sees
    // any UI. Cheap; ~ms.
    let platform = PlatformInfo::detect()?;

    // Pre-flight: handle missing FFmpeg BEFORE opening the panel.
    //
    // The previous flow checked FFmpeg AFTER camera detection. That
    // was wrong: on Windows, camera detection itself runs
    // `ffmpeg -list_devices`, so a missing FFmpeg makes camera
    // detection fail with a misleading "program not found" partway
    // through Step 1's spinner. Catching it here means the panel
    // only ever opens when prereqs are guaranteed satisfied.
    //
    // Doing the prompt + progress bar OUTSIDE the panel also keeps
    // them from competing with a half-rendered TUI — `inquire` and
    // `indicatif` both write to stdout in their own format, and
    // mixing them with the panel's box-drawing characters is ugly.
    if !super::platform::check_ffmpeg()? {
        prompt_and_install_ffmpeg(&platform)?;
    }

    panel_top("Step 1 / 5 — Prerequisites");
    panel_blank();

    // Progress bar
    panel_row(&{
        let bar = progress_bar_str(&steps_for(0));
        format!("  {}", bar)
    });
    panel_blank();
    panel_divider();
    panel_blank();

    let mut spinner = Spinner::new();

    // Platform — already detected above, just announce it.
    panel_check(&format!("Platform: {}", platform.display()));

    // FFmpeg — guaranteed present at this point (we either found it
    // above, just installed it, or returned Err already).
    if let Some(ver) = super::platform::get_ffmpeg_version() {
        panel_check(&format!("FFmpeg v{}", ver));
    } else {
        panel_check("FFmpeg: installed");
    }

    // Camera — Pi USB enumeration can lag up to ~10s after boot, so we
    // probe repeatedly instead of giving up on the first empty result.
    // Total budget is ~15s; on Windows the first probe almost always wins.
    panel_spinner_row(&spinner.advance(), "Detecting cameras...");
    flush();
    thread::sleep(Duration::from_millis(300));
    let cameras = detect_cameras_with_retry(&mut spinner, &platform)?;
    print!("\r");
    flush();
    if cameras.is_empty() {
        panel_error("No cameras detected — connect a USB camera and restart");
        if platform.is_linux {
            panel_sub("On Raspberry Pi, check: ls /dev/video* and v4l2-ctl --list-devices");
            panel_sub("Add your user to the 'video' group if permissions are the issue");
        }
        panel_blank();
        panel_bottom();
        std::process::exit(1);
    } else {
        panel_check(&format!("{} camera(s) detected", cameras.len()));
        for cam in &cameras {
            panel_sub(&format!(
                "{} — {}×{}",
                cam.name, cam.preferred_resolution.0, cam.preferred_resolution.1
            ));
        }
    }

    // Network
    panel_spinner_row(&spinner.advance(), "Checking network...");
    flush();
    thread::sleep(Duration::from_millis(250));
    print!("\r");
    flush();
    panel_check("Network: connected");

    panel_blank();
    panel_bottom();
    println!();

    Ok(platform)
}

/// Pre-flight FFmpeg handling when `check_ffmpeg()` returns false.
///
/// CloudNode no longer ships its own FFmpeg copy (v0.1.35 onward). The
/// canonical answer on every supported platform is "install FFmpeg via
/// the OS package manager". This function detects the user's package
/// manager, offers to run the install command for them, and tells them
/// to re-launch CloudNode after the install finishes (PATH changes
/// don't apply to the running process — a fresh process is required).
///
/// Why we don't just keep the install running and re-check PATH after:
/// Win32 and POSIX both capture the parent's environment block at
/// CreateProcess/exec time. winget / brew / apt all update the
/// SYSTEM-wide PATH (or user PATH on Windows), but our running process
/// is stuck with the snapshot it inherited at launch. Spawning a new
/// shell to detect ffmpeg works but is fragile. The cleanest UX is:
/// install runs to completion, user re-launches CloudNode, fresh
/// process sees the updated PATH, prereq check passes.
fn prompt_and_install_ffmpeg(platform: &PlatformInfo) -> Result<()> {
    println!();
    println!("  ⚠  FFmpeg not found");
    println!();
    println!("  CloudNode uses the system FFmpeg installation for camera capture");
    println!("  and HLS encoding. Install it once via your OS package manager and");
    println!("  CloudNode will pick it up automatically.");
    println!();

    let (label, command, args, install_url) = if platform.is_windows {
        (
            "winget",
            "winget",
            vec!["install", "--id", "Gyan.FFmpeg", "-e", "--accept-source-agreements", "--accept-package-agreements"],
            "https://www.gyan.dev/ffmpeg/builds/",
        )
    } else if platform.is_macos {
        (
            "Homebrew",
            "brew",
            vec!["install", "ffmpeg"],
            "https://brew.sh/",
        )
    } else {
        // Linux: lots of distros, no vendor consensus. Don't try to
        // auto-run; just print the most common command and let the
        // operator choose the right one for their distro.
        return Err(anyhow::anyhow!(
            "FFmpeg is required. Install via your distro's package manager:\n  \
               Debian/Ubuntu:  sudo apt install ffmpeg\n  \
               Fedora/RHEL:    sudo dnf install ffmpeg\n  \
               Arch:           sudo pacman -S ffmpeg\n  \
               Then re-run setup."
        ));
    };

    let install = Confirm::new(&format!(
        "  Install FFmpeg now via {} ?",
        label
    ))
    .with_default(true)
    .with_help_message(&format!(
        "Runs: {} {}",
        command,
        args.join(" ")
    ))
    .prompt()?;

    if !install {
        return Err(anyhow::anyhow!(
            "FFmpeg is required. Install it yourself, then re-run setup.\n  \
             Manual download: {}",
            install_url
        ));
    }

    println!();
    println!("  Running: {} {}", command, args.join(" "));
    println!();

    // Run the package manager. Inherit stdio so the user sees the
    // install progress live (winget/brew both have nice progress bars
    // of their own; we don't need to wrap them in indicatif).
    let status = std::process::Command::new(command)
        .args(&args)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!();
            println!("  ✓ FFmpeg install command completed.");
            println!();
            // Critical UX: the running process can't see the new PATH.
            // Tell the user to re-launch and exit cleanly. Returning Err
            // here propagates up to run_setup which prints the error and
            // exits — the message is the user's next action.
            Err(anyhow::anyhow!(
                "FFmpeg installed. Close this window and re-launch Sentinel CloudNode \
                 from the Start menu. PATH changes don't apply to the current process — a fresh \
                 launch is required for the prereq check to see the new install."
            ))
        }
        Ok(s) => Err(anyhow::anyhow!(
            "{} exited with status {}. Install FFmpeg manually:\n  \
               {} {}\n  \
               Or download from {} and add to PATH.",
            label, s, command, args.join(" "), install_url
        )),
        Err(e) => Err(anyhow::anyhow!(
            "Could not invoke {}: {}\n  \
             Install FFmpeg manually:\n  \
               {} {}\n  \
             Or download from {} and add to PATH.",
            command, e, command, args.join(" "), install_url
        )),
    }
}

/// Probe for cameras repeatedly while the spinner animates.
///
/// Why this exists: on a Raspberry Pi booted directly into the setup
/// wizard, USB camera enumeration can lag several seconds behind
/// userspace — the kernel is still binding the uvcvideo driver while
/// we're already calling `v4l2-ctl --list-devices`. A single-shot probe
/// wrongly reports "no cameras" and hard-exits. On Windows the first
/// probe essentially always succeeds, so a short retry there is a
/// no-op in practice.
///
/// Budget:
/// - Linux: ~15s total (50 attempts × ~300ms) — covers boot-time USB
///   enumeration even on slow SD cards.
/// - Windows/macOS: ~2s total (6 attempts × ~300ms) — belt-and-braces
///   against a transiently busy DirectShow / AVFoundation enumerator.
fn detect_cameras_with_retry(
    spinner: &mut Spinner,
    platform: &PlatformInfo,
) -> Result<Vec<crate::camera::DetectedCamera>> {
    let max_attempts = if platform.is_linux { 50 } else { 6 };

    for attempt in 0..max_attempts {
        // Any probe error (e.g. v4l2-ctl missing) is surfaced immediately
        // — retrying won't conjure a tool that isn't installed.
        let cameras = crate::camera::detect_cameras()?;
        if !cameras.is_empty() {
            return Ok(cameras);
        }

        // Don't sleep after the final attempt.
        if attempt + 1 == max_attempts {
            break;
        }

        // Keep the spinner animating with a hint so the operator knows
        // we're waiting on them, not hung. Hint only fires on Linux
        // where the wait is long enough to notice.
        let hint = if platform.is_linux && attempt >= 3 {
            "Detecting cameras... (waiting for USB enumeration)"
        } else {
            "Detecting cameras..."
        };
        panel_spinner_row(&spinner.advance(), hint);
        flush();
        thread::sleep(Duration::from_millis(300));
        // Rewind so the next row replaces the spinner in place.
        print!("\r");
        flush();
    }

    // Exhausted budget — caller handles the empty-result UX.
    Ok(Vec::new())
}

// ─── Step 2: Configure ───────────────────────────────────────────────────────

fn configure_node(platform: &PlatformInfo) -> Result<SetupConfig> {
    use crate::config::NodeMode;

    panel_top("Step 2 / 5 — Node Configuration");
    panel_blank();

    panel_row(&{
        let bar = progress_bar_str(&steps_for(1));
        format!("  {}", bar)
    });
    panel_blank();
    panel_divider();
    panel_blank();

    // ── Mode selection (Local vs Connected) ─────────────────────────
    // First decision in setup. Connected = the existing SaaS-paired
    // flow (registers with Command Center, opens WS, ships HLS).
    // Local = standalone install on the LAN; the runtime gates every
    // CC-coupled path on `mode == Local`.
    panel_row(&format!(
        "  {}",
        "How do you want to run this node?".bright_white()
    ));
    panel_blank();
    panel_row(&format!(
        "  {} {}",
        "•".cyan(),
        "Connected — pair with Sentinel Command Center for".bright_white()
    ));
    panel_row("    multi-site dashboards, AI agent, mobile remote access,");
    panel_row("    email alerts, and team workflows. Free account required.");
    panel_blank();
    panel_row(&format!(
        "  {} {}",
        "•".cyan(),
        "Local-only — runs on your LAN, no Command Center pairing.".bright_white()
    ));
    panel_row("    Browser dashboard, snapshots, recording, playback. Free.");
    panel_blank();
    panel_bottom();
    println!();

    let connect_to_cc = Confirm::new("  Connect this node to a Command Center?")
        .with_help_message(
            "Yes = Connected mode (full SaaS).  No = Local-only mode (LAN-only, no account).",
        )
        .with_default(true)
        .prompt()?;

    let mode = if connect_to_cc {
        NodeMode::Connected
    } else {
        NodeMode::Local
    };

    // ── Connected-mode: CC creds + validation ──────────────────────
    // Local-mode skips this entire block; the empty-string defaults
    // below feed save_config_to_database, which detects mode=Local
    // and writes only the rows that make sense for a standalone node.
    let (node_id, api_key, api_url) = if mode.is_connected() {
        println!();
        panel_top("Command Center Pairing");
        panel_blank();
        panel_row(&format!(
            "  Open {} in your browser:",
            "Sentinel Command Center".cyan().bold()
        ));
        panel_row(&format!(
            "  {} {}",
            "→".cyan(),
            "https://opensentry-command.fly.dev".bright_white()
        ));
        panel_blank();
        panel_row(&format!(
            "  Navigate to: {} → {} → {}",
            "Settings".cyan(),
            "Nodes".cyan(),
            "Add Node".cyan()
        ));
        panel_blank();
        panel_bottom();
        println!();

        // Inputs (outside panel — inquire draws its own UI)
        let node_id = Text::new("  Node ID:")
            .with_placeholder("cf394d69")
            .with_validator(|input: &str| {
                if input.len() == 8 && input.chars().all(|c| c.is_ascii_hexdigit()) {
                    Ok(Validation::Valid)
                } else {
                    Ok(Validation::Invalid(
                        "Must be 8 hexadecimal characters (e.g. cf394d69)".into(),
                    ))
                }
            })
            .prompt()?;

        let api_key = Text::new("  API Key:")
            .with_placeholder("f3eda4fd-7810-4577-94a8-290fbb6d9523")
            .with_validator(|input: &str| {
                let parts: Vec<&str> = input.trim().split('-').collect();
                if parts.len() == 5
                    && parts[0].len() == 8
                    && parts[1].len() == 4
                    && parts[2].len() == 4
                    && parts[3].len() == 4
                    && parts[4].len() == 12
                    && parts
                        .iter()
                        .all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
                {
                    Ok(Validation::Valid)
                } else {
                    Ok(Validation::Invalid(
                        "Must be a UUID: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx".into(),
                    ))
                }
            })
            .prompt()?;

        let default_url = "https://opensentry-command.fly.dev";
        let api_url = Text::new("  Command Center URL:")
            .with_placeholder(default_url)
            .with_default(default_url)
            .with_validator(|input: &str| {
                // HTTPS is required for production Command Centers — the API
                // key travels in headers and must not go over the wire in
                // plaintext.  We carve out an exception for `http://localhost`
                // and `http://127.0.0.1` so dev workflows running a local
                // Command Center on plain HTTP keep working (the loopback
                // interface never leaves the host).
                //
                // Rejecting plain http:// for any other host closes a real
                // footgun: an operator who fat-fingers `http://` in front of
                // a public Command Center URL would silently ship their API
                // key over an unencrypted connection.  Better to fail loudly
                // at setup than to leak credentials in transit.
                if input.starts_with("https://") {
                    Ok(Validation::Valid)
                } else if input.starts_with("http://localhost")
                    || input.starts_with("http://127.0.0.1")
                {
                    Ok(Validation::Valid)
                } else if input.starts_with("http://") {
                    Ok(Validation::Invalid(
                        "Plain http:// is only allowed for localhost. Use https:// for production Command Centers — the API key travels in request headers and must not go over the wire unencrypted.".into(),
                    ))
                } else {
                    Ok(Validation::Invalid(
                        "Must start with https:// (or http://localhost for local dev)".into(),
                    ))
                }
            })
            .prompt()?;

        // Validation panel
        println!();
        panel_top("Validating Connection");
        panel_blank();

        let mut spinner = Spinner::new();
        panel_spinner_row(&spinner.advance(), &format!("Connecting to {}...", api_url));
        flush();

        let validation = tokio::runtime::Runtime::new()
            .expect("tokio runtime")
            .block_on(super::validator::validate_api_connection(
                &api_url, &node_id, &api_key,
            ))?;

        print!("\r");
        flush();

        if !validation.is_valid {
            panel_error("Connection failed");
            if let Some(msg) = &validation.error_message {
                for line in msg.lines() {
                    panel_row(&format!("  {}", line.red()));
                }
            }
            panel_blank();
            panel_bottom();
            println!();

            let retry = Confirm::new("  Try again with different credentials?")
                .with_default(true)
                .prompt()?;
            if retry {
                println!();
                return configure_node(platform);
            } else {
                println!();
                println!(
                    "  {}",
                    "Run 'sourcebox-sentry-cloudnode setup' to try again.".yellow()
                );
                std::process::exit(1);
            }
        }

        panel_check("Connected successfully");
        if let Some(name) = &validation.node_name {
            panel_sub(&format!("Node name: {}", name));
        }
        panel_blank();
        panel_bottom();
        println!();

        (node_id, api_key, api_url)
    } else {
        // Local mode: no creds, no remote validation.  The TUI status
        // bar will print the LAN URL once the node boots; setup just
        // needs to confirm what the user is about to install.
        println!();
        panel_top("Local-only Mode");
        panel_blank();
        panel_row("  This node will run standalone on your local network.");
        panel_row("  No Command Center pairing, no account, no cloud.");
        panel_blank();
        panel_row(&format!(
            "  After setup, open the dashboard in any browser on the LAN:"
        ));
        panel_row(&format!(
            "    {}",
            "http://<node-IP>:8080".bright_white()
        ));
        panel_row("    (the IP appears in the TUI status bar after boot)");
        panel_blank();
        panel_bottom();
        println!();

        (String::new(), String::new(), String::new())
    };

    // Deployment + auto-start
    let deployment_method = select_deployment_method(platform)?;

    // Storage cap.  Default = the disk-aware suggestion (80% of free,
    // clamped to [5, 64] GB) so a Pi-on-32GB-SD-card user gets a
    // sensible default instead of the historical 64 GB which would
    // blow up the host.  Operator can override; we validate the
    // input as a positive integer and warn (not block) if they pick
    // > 90% of free, since the retention loop runs on a 5 min tick
    // and a near-full cap means the writer can sneak past into ENOSPC.
    println!();
    let data_dir = crate::paths::data_dir();
    let (free_bytes, _) = crate::storage::disk_info(&data_dir);
    let suggested = crate::storage::suggested_max_gb(&data_dir).unwrap_or(64);
    let free_gb = if free_bytes > 0 {
        free_bytes / (1024 * 1024 * 1024)
    } else {
        0
    };
    if free_gb > 0 {
        panel_sub(&format!(
            "Available disk: {} GB free at {}",
            free_gb,
            data_dir.display(),
        ));
    } else {
        panel_sub(&format!(
            "Couldn't read disk info for {} — using {} GB default",
            data_dir.display(),
            suggested,
        ));
    }
    let max_size_gb_str = inquire::Text::new(&format!(
        "  Recording cap (GB) [default {}]:",
        suggested,
    ))
    .with_default(&suggested.to_string())
    .with_validator(|input: &str| {
        match input.trim().parse::<u64>() {
            Ok(v) if v >= 1 => Ok(inquire::validator::Validation::Valid),
            Ok(_) => Ok(inquire::validator::Validation::Invalid(
                "Cap must be at least 1 GB".into(),
            )),
            Err(_) => Ok(inquire::validator::Validation::Invalid(
                "Enter a whole number of GB (e.g. 32)".into(),
            )),
        }
    })
    .prompt()?;
    let max_size_gb: u64 = max_size_gb_str.trim().parse().unwrap_or(suggested);
    if free_gb > 0 && max_size_gb > (free_gb * 9) / 10 {
        // Warning, not a blocker — operator might know something we
        // don't (e.g. they're about to free up disk).  Print and move
        // on; the safety floor in storage::stats will pause recording
        // before the disk is fully filled regardless.
        panel_warn(&format!(
            "Cap of {} GB is more than 90% of free disk ({} GB).",
            max_size_gb, free_gb,
        ));
        panel_sub(
            "Recording will pause automatically when host disk drops below 1 GB free.",
        );
    }

    println!();
    let auto_start = Confirm::new("  Auto-start CloudNode after setup?")
        .with_default(true)
        .prompt()?;

    // Summary panel — mode-aware. Local mode hides the CC creds rows
    // (they're empty strings) and surfaces the LAN dashboard URL
    // instead, since that's the daily-driver entry point.
    println!();
    panel_top("Configuration Summary");
    panel_blank();
    panel_kv(
        "  Mode        :",
        if mode.is_local() { "Local-only" } else { "Connected" },
    );
    if mode.is_connected() {
        panel_kv("  Node ID     :", &node_id);
        panel_kv("  API Key     :", &format!("{}…", &api_key[..10]));
        panel_kv("  API URL     :", &api_url);
    } else {
        panel_kv("  Dashboard   :", "http://<node-IP>:8080");
    }
    panel_kv("  Deploy      :", &format!("{:?}", deployment_method));
    panel_kv("  Storage cap :", &format!("{} GB", max_size_gb));
    panel_kv("  Auto-start  :", if auto_start { "Yes" } else { "No" });
    panel_blank();
    panel_bottom();
    println!();

    let confirm = Confirm::new("  Continue with these settings?")
        .with_default(true)
        .prompt()?;

    if !confirm {
        println!("  {}", "Setup cancelled.".red());
        std::process::exit(0);
    }

    Ok(SetupConfig {
        mode,
        node_id,
        api_key,
        api_url,
        deployment_method,
        output_dir: std::env::current_dir()?,
        auto_start,
        max_size_gb,
    })
}

fn select_deployment_method(platform: &PlatformInfo) -> Result<DeploymentMethod> {
    let options = vec!["Build from Source (Recommended)", "Docker"];
    let choice = if platform.is_windows || platform.is_linux || platform.is_macos {
        Select::new("  How would you like to run CloudNode?", options)
            .with_starting_cursor(0)
            .prompt()?
    } else {
        "Build from Source (Recommended)"
    };

    if choice == "Build from Source (Recommended)" {
        if platform.is_windows {
            println!();
            let sub = vec![
                "Windows Native (DirectShow)",
                "WSL2 (v4l2, requires USB passthrough)",
            ];
            let sel = Select::new("  Run where?", sub)
                .with_starting_cursor(0)
                .prompt()?;
            Ok(if sel.starts_with("Windows") {
                DeploymentMethod::WindowsNative
            } else {
                DeploymentMethod::WSL2
            })
        } else {
            Ok(DeploymentMethod::LinuxNative)
        }
    } else {
        Ok(DeploymentMethod::Docker)
    }
}

// ─── Step 3: Install ─────────────────────────────────────────────────────────

fn install_dependencies(config: &SetupConfig, platform: &PlatformInfo) -> Result<()> {
    panel_top("Step 3 / 5 — Installing Dependencies");
    panel_blank();

    panel_row(&{
        let bar = progress_bar_str(&steps_for(2));
        format!("  {}", bar)
    });
    panel_blank();
    panel_divider();
    panel_blank();

    let mut spinner = Spinner::new();

    // Save config to database
    panel_spinner_row(&spinner.advance(), "Saving configuration to database...");
    flush();
    save_config_to_database(config)?;
    print!("\r");
    flush();
    panel_check("Configuration saved to database (API key encrypted)");

    // FFmpeg + GPU encoder detection (Windows path).
    //
    // FFmpeg installation moved to Step 1 (`prompt_and_install_ffmpeg`) —
    // by the time we reach Step 3, ffmpeg is guaranteed present (or the
    // wizard would have bailed earlier with a clear error). All we still
    // do here is the per-camera GPU encoder probe, which is purely a
    // performance optimisation that depends on ffmpeg being callable.
    if matches!(config.deployment_method, DeploymentMethod::WindowsNative) {
        // GPU encoder detection
        panel_blank();
        panel_mid("Video Encoder Detection");
        panel_blank();
        panel_spinner_row(&spinner.advance(), "Probing for GPU hardware encoder...");
        flush();

        let ffmpeg_path = find_ffmpeg_for_setup();
        let hw_encoder = crate::streaming::hls_generator::HlsGenerator::detect_hw_encoder(&ffmpeg_path);
        print!("\r");
        flush();

        match &hw_encoder {
            Some(enc) => {
                let gpu_name = match enc.as_str() {
                    "h264_nvenc" => "NVIDIA NVENC (GPU)",
                    "h264_qsv" => "Intel Quick Sync (GPU)",
                    "h264_amf" => "AMD AMF (GPU)",
                    "h264_v4l2m2m" => "V4L2 M2M (Hardware)",
                    other => other,
                };
                panel_check(&format!("GPU encoder available: {}", gpu_name.cyan()));
                panel_blank();
                panel_bottom();
                println!();

                let options = vec![
                    format!("{} (Recommended — frees CPU, faster encoding)", gpu_name),
                    "Software (libx264 — CPU-based, works everywhere)".to_string(),
                ];
                let choice = Select::new("  Video encoder:", options)
                    .with_starting_cursor(0)
                    .prompt()?;

                let use_gpu = choice.contains("GPU") || choice.contains("NVENC")
                    || choice.contains("Quick Sync") || choice.contains("AMF")
                    || choice.contains("Hardware");

                if use_gpu {
                    save_config_to_db(config, "encoder", &enc)?;
                    println!();
                    panel_top("Step 3 / 5 — Installing Dependencies");
                    panel_blank();
                    panel_check(&format!("Encoder: {} (GPU)", gpu_name.cyan()));
                } else {
                    save_config_to_db(config, "encoder", "libx264")?;
                    println!();
                    panel_top("Step 3 / 5 — Installing Dependencies");
                    panel_blank();
                    panel_check(&format!("Encoder: {} (CPU)", "libx264".cyan()));
                }
            }
            None => {
                panel_check(&format!("Encoder: {} (CPU — no GPU detected)", "libx264".cyan()));
                save_config_to_db(config, "encoder", "libx264")?;
            }
        }

        // Codec detection
        panel_blank();
        panel_mid("Camera Codec Detection");
        panel_blank();

        let cameras = crate::camera::detect_cameras()?;
        if let Some(first) = cameras.first() {
            panel_spinner_row(&spinner.advance(), &format!("Probing {}...", first.name));
            flush();

            match crate::streaming::codec_detector::detect_codec_from_camera(&first.device_path) {
                Ok(info) => {
                    print!("\r");
                    flush();
                    panel_check(&format!(
                        "Video: {}   Audio: {}",
                        info.video_codec.cyan(),
                        info.audio_codec.cyan()
                    ));
                    panel_sub("This codec will be used for HLS streaming");
                }
                Err(e) => {
                    print!("\r");
                    flush();
                    panel_warn(&format!("Codec detection skipped: {}", e));
                    panel_sub(
                        "Default: avc1.42e01e (H.264 Baseline) — compatible with most cameras",
                    );
                }
            }
        }
    } else if matches!(config.deployment_method, DeploymentMethod::WSL2) {
        // WSL2: the host is Windows but the node will run inside a Linux
        // distro.  Delegate to the preflight module, which probes WSL,
        // the distro's ffmpeg, usbipd-win, and connected USB cameras —
        // auto-installing what it can and printing actionable guidance
        // for the rest.
        super::wsl_preflight::run_wsl_preflight_interactive(config)?;
    } else {
        if !super::platform::check_ffmpeg()? {
            if platform.is_linux {
                panel_warn("FFmpeg not found — install with: sudo apt install ffmpeg");
            } else if platform.is_macos {
                panel_warn("FFmpeg not found — install with: brew install ffmpeg");
            } else {
                panel_warn("FFmpeg not found — please install it before running");
            }
        } else {
            panel_check("FFmpeg available");

            // GPU detection for non-Windows platforms too
            let ffmpeg_path = "ffmpeg".to_string();
            let hw_encoder = crate::streaming::hls_generator::HlsGenerator::detect_hw_encoder(&ffmpeg_path);
            if let Some(enc) = &hw_encoder {
                let gpu_name = match enc.as_str() {
                    "h264_nvenc" => "NVIDIA NVENC (GPU)",
                    "h264_qsv" => "Intel Quick Sync (GPU)",
                    "h264_amf" => "AMD AMF (GPU)",
                    "h264_v4l2m2m" => "V4L2 M2M (Hardware)",
                    other => other,
                };
                panel_check(&format!("GPU encoder available: {}", gpu_name.cyan()));
                panel_blank();
                panel_bottom();
                println!();

                let options = vec![
                    format!("{} (Recommended — frees CPU, faster encoding)", gpu_name),
                    "Software (libx264 — CPU-based, works everywhere)".to_string(),
                ];
                let choice = Select::new("  Video encoder:", options)
                    .with_starting_cursor(0)
                    .prompt()?;

                let use_gpu = !choice.contains("Software");
                if use_gpu {
                    save_config_to_db(config, "encoder", &enc)?;
                } else {
                    save_config_to_db(config, "encoder", "libx264")?;
                }

                println!();
                panel_top("Step 3 / 5 — Installing Dependencies");
                panel_blank();
            }
        }
    }

    // Directories
    panel_spinner_row(&spinner.advance(), "Creating data directories...");
    flush();
    create_directories(config)?;
    print!("\r");
    flush();
    panel_check("Data directories created");

    panel_blank();
    panel_bottom();
    println!();

    Ok(())
}

// ─── Step 4: Verify ──────────────────────────────────────────────────────────

fn verify_setup(_config: &SetupConfig) -> Result<()> {
    panel_top("Step 4 / 5 — Verification");
    panel_blank();

    panel_row(&{
        let bar = progress_bar_str(&steps_for(3));
        format!("  {}", bar)
    });
    panel_blank();
    panel_divider();
    panel_blank();

    let mut spinner = Spinner::new();

    panel_spinner_row(&spinner.advance(), "Verifying database configuration...");
    flush();
    let db_path = crate::paths::config_db_path();
    if !db_path.exists() {
        print!("\r");
        flush();
        panel_error("Database file missing");
        panel_blank();
        panel_bottom();
        std::process::exit(1);
    }
    thread::sleep(Duration::from_millis(200));
    print!("\r");
    flush();
    panel_check("Database configuration valid");

    panel_spinner_row(&spinner.advance(), "Verifying directories...");
    flush();
    thread::sleep(Duration::from_millis(200));
    print!("\r");
    flush();
    panel_check("Data directories present");

    panel_spinner_row(&spinner.advance(), "Finalizing...");
    flush();
    thread::sleep(Duration::from_millis(200));
    print!("\r");
    flush();
    panel_check("Setup verified");

    panel_blank();
    panel_bottom();
    println!();

    Ok(())
}

// ─── Step 5: Success ─────────────────────────────────────────────────────────

fn show_success_screen(config: &SetupConfig) -> Result<()> {
    clear_screen()?;
    show_confetti(Duration::from_secs(2))?;

    thread::sleep(Duration::from_millis(200));
    animate_rainbow_text("        ✓  SETUP COMPLETE  ✓", Duration::from_millis(800))?;
    thread::sleep(Duration::from_millis(200));

    println!();
    panel_top("Step 5 / 5 — Ready to Launch");
    panel_blank();

    panel_row(&{
        let bar = progress_bar_str(&steps_for(4));
        format!("  {}", bar)
    });
    panel_blank();
    panel_divider();
    panel_blank();

    // Pulse the tagline inside the panel
    let tagline = "🎉  Your Sentinel CloudNode is ready!";
    for _ in 0..2 {
        panel_row(&format!("  {}", tagline.green().bold()));
        thread::sleep(Duration::from_millis(450));
        panel_row(&format!("  {}", tagline.dimmed()));
        thread::sleep(Duration::from_millis(450));
    }
    panel_row(&format!("  {}", tagline.green().bold()));
    panel_blank();
    panel_divider();
    panel_blank();

    // Summary — mode-aware. Local installs hide the empty Node ID /
    // API URL rows (they're empty strings by design) and surface the
    // LAN dashboard URL placeholder instead, since that's the
    // daily-driver entry point.
    panel_kv(
        "  Mode        :",
        if config.mode.is_local() { "Local-only" } else { "Connected" },
    );
    if config.mode.is_connected() {
        panel_kv("  Node ID     :", &config.node_id);
        panel_kv("  API URL     :", &config.api_url);
    } else {
        panel_kv("  Dashboard   :", "http://<node-IP>:8080");
    }
    panel_kv(
        "  Deploy      :",
        &format!("{:?}", config.deployment_method),
    );
    panel_blank();
    panel_divider();
    panel_blank();

    // Checks
    panel_check("Configuration saved");
    panel_check("Directories created");
    if matches!(config.deployment_method, DeploymentMethod::WindowsNative) {
        panel_check("FFmpeg installed");
    }
    panel_blank();
    panel_divider();
    panel_blank();

    // Next steps
    match config.deployment_method {
        DeploymentMethod::WindowsNative => {
            // No hint needed: FFmpeg lives at
            // %ProgramData%\SourceBoxSentry\ffmpeg\bin\ and the node's
            // streaming::find_tool lookup finds it there without any
            // shell-PATH gymnastics from the user.
        }
        DeploymentMethod::Docker => {
            panel_row(&format!(
                "  {}  {}",
                "Start with:".white().bold(),
                "docker compose up".cyan()
            ));
        }
        DeploymentMethod::LinuxNative | DeploymentMethod::WSL2 => {
            panel_row(&format!(
                "  {}  {}",
                "Start with:".white().bold(),
                "./target/release/sourcebox-sentry-cloudnode".cyan()
            ));
        }
    }

    // Dashboard URL printed in the post-install "Next steps" block.
    // Local installs hand the operator the local LAN URL — that's the
    // daily-driver dashboard now.  Connected installs point at the
    // Command Center URL they paired with, with a localhost dev
    // fallback for `http://localhost`/`127.0.0.1` API URLs.
    let dashboard = if config.mode.is_local() {
        "http://<node-IP>:8080  (URL in TUI status bar after boot)".to_string()
    } else if config.api_url.contains("localhost") || config.api_url.contains("127.0.0.1") {
        "http://localhost:5173".to_string()
    } else {
        config.api_url.clone()
    };
    panel_row(&format!(
        "  {}  {}",
        "Dashboard:  ".white().bold(),
        dashboard.cyan()
    ));
    panel_row(&format!(
        "  {}  {}",
        "Docs:       ".white().bold(),
        "https://github.com/SourceBox-LLC/Sentinel-CameraNode".cyan()
    ));

    panel_blank();
    panel_divider();
    panel_blank();

    if config.auto_start {
        panel_row(&format!("  {}", "🚀  Starting CloudNode...".green().bold()));
    } else {
        panel_row(&format!(
            "  {}",
            "Press Enter to start CloudNode...".yellow().bold()
        ));
    }

    panel_blank();
    panel_bottom();
    println!();

    show_mini_celebration()?;

    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Render the pill progress bar as a string (for embedding inside a panel row).
fn progress_bar_str(steps: &[(&'static str, StepState)]) -> String {
    let connector = "──".dimmed().to_string();
    steps
        .iter()
        .map(|(label, state)| match state {
            StepState::Done => format!("{}", format!("[ ✓ {} ]", label).bright_green().bold()),
            StepState::Active => format!("{}", format!("[ ● {} ]", label).cyan().bold()),
            StepState::Pending => format!("{}", format!("[  {}  ]", label).dimmed()),
        })
        .collect::<Vec<_>>()
        .join(&connector)
}

fn save_config_to_database(config: &SetupConfig) -> Result<()> {
    // Store config in the SQLite database with the API key encrypted.
    let db_path = crate::paths::config_db_path();
    std::fs::create_dir_all(db_path.parent().unwrap())?;
    let db = crate::storage::NodeDatabase::new(&db_path)
        .map_err(|e| anyhow::anyhow!("DB error: {}", e))?;

    // In Local mode, `node_id` and `api_key` are empty strings from the
    // wizard — leave node_id as None and let save_to_db skip the api_key
    // row (it already gates on `is_empty`).  We rely on the `mode='local'`
    // row to signal "configured for local-only" to needs_setup at boot.
    let node_id_opt = if config.mode.is_connected() {
        Some(config.node_id.clone())
    } else {
        None
    };

    // Re-running setup is the documented remedy for key rotation /
    // credential failure ("/reauth" → wizard), so it must NOT reset the
    // operator's tuning.  Start from the EXISTING persisted config when
    // one exists and overwrite only the fields the wizard actually
    // collects — a `Config { .., ..Default::default() }` here silently
    // reverted every `/set` adjustment (fps, bitrate, motion
    // sensitivity/cooldown, node name, log level, ports…) on each
    // re-run.
    let mut app_config = if db.has_config() {
        crate::config::Config::load_from_db(&db)
            .unwrap_or_else(|_| crate::config::Config::default())
    } else {
        crate::config::Config::default()
    };

    app_config.mode = config.mode;
    app_config.node.node_id = node_id_opt;
    app_config.cloud.api_url = config.api_url.clone();
    app_config.cloud.api_key = config.api_key.clone();
    // Operator-chosen storage cap from the wizard prompt.  Defaults
    // to 64 GB if the wizard couldn't read disk info; otherwise
    // ~80% of free disk, clamped to [5, 64] GB.  Persisted via
    // save_to_db's `max_size_gb` row so the running node + the
    // dashboard's storage bar both see the chosen value on next boot.
    app_config.storage.max_size_gb = config.max_size_gb;
    // Bind address is MODE POLICY, not tuning: Local binds 0.0.0.0 so a
    // phone on the LAN can open the dashboard; Connected keeps the safe
    // loopback-only bind (the public Command Center is the remote
    // surface).  The port, by contrast, is operator-tunable — preserve
    // whatever the existing config says (default 8080 on fresh installs).
    app_config.server.bind = if config.mode.is_local() {
        "0.0.0.0".to_string()
    } else {
        crate::config::ServerConfig::default().bind
    };

    app_config
        .save_to_db(&db)
        .map_err(|e| anyhow::anyhow!("Config save error: {}", e))?;

    // Switching Connected → Local opts OUT of cloud pairing: clear the
    // now-retired credential rows.  save_to_db skips None/empty values,
    // so without this the stale node_id + encrypted api_key would linger
    // in the DB indefinitely after the operator chose local-only.
    if config.mode.is_local() {
        let _ = db.delete_config("node_id");
        let _ = db.delete_config("api_key");
    }

    Ok(())
}

fn create_directories(_config: &SetupConfig) -> Result<()> {
    // Data dir resolution moved to crate::paths to support Windows
    // Service installs (cwd = System32) under %ProgramData%\SourceBoxSentry\.
    let data = crate::paths::data_dir();
    std::fs::create_dir_all(data.join("hls"))?;
    Ok(())
}

/// Find FFmpeg path for setup probing — delegates to the shared
/// `streaming::find_ffmpeg` so the setup detection matches what the
/// running node will actually use (including Linux/macOS fallback paths).
fn find_ffmpeg_for_setup() -> String {
    crate::streaming::find_ffmpeg()
}

/// Save a config key-value pair to the SQLite database.
fn save_config_to_db(_config: &SetupConfig, key: &str, value: &str) -> Result<()> {
    let db_path = crate::paths::config_db_path();
    let db = crate::storage::NodeDatabase::new(&db_path)
        .map_err(|e| anyhow::anyhow!("DB error: {}", e))?;
    db.set_config(key, value)
        .map_err(|e| anyhow::anyhow!("Config save error: {}", e))?;
    Ok(())
}

