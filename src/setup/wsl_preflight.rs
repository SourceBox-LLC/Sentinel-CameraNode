// Sentinel CameraNode - Camera streaming node for Sentinel Command Center
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
//! WSL2 preflight + auto-install (Scope A).
//!
//! When the user picks `DeploymentMethod::WSL2` on a Windows host, this module:
//!
//! 1. Checks WSL itself is installed.
//! 2. Finds a usable Linux distro (skipping `docker-desktop` which can't run
//!    a node because it has no package manager and no v4l2 support).
//! 3. Checks FFmpeg inside the chosen distro — offers to `apt-get install`
//!    it for the user (passwordless sudo in WSL makes this non-elevated).
//! 4. Checks `usbipd-win` is installed on the host — prints a one-line
//!    `winget` install command if not.
//! 5. Lists connected USB cameras and prints the exact `usbipd bind` +
//!    `usbipd attach --wsl` incantation to forward them into the distro.
//!
//! Things that need admin elevation (installing WSL itself, installing
//! usbipd-win, running `usbipd bind`) are *not* executed by this process —
//! we display the exact command and the operator runs it in an admin
//! PowerShell.  That's the Scope A line.  Scope B would handle elevation.

use std::process::Command;

use anyhow::Result;
use colored::Colorize;
use inquire::{Confirm, Select};

use super::animations::Spinner;
use super::ui::{
    flush, panel_blank, panel_bottom, panel_check, panel_mid, panel_spinner_row, panel_sub,
    panel_top, panel_warn,
};
use super::SetupConfig;

// ─── Types ──────────────────────────────────────────────────────────────────

/// Result of probing WSL on the Windows host.
#[derive(Debug, Clone)]
pub struct WslStatus {
    pub installed: bool,
    pub distros: Vec<DistroInfo>,
    pub default_distro: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistroInfo {
    pub name: String,
    pub state: String,
    pub version: String,
}

/// Result of probing a specific distro for tools.
#[derive(Debug, Clone)]
pub struct DistroStatus {
    pub name: String,
    pub has_ffmpeg: bool,
    pub ffmpeg_version: Option<String>,
}

/// Result of probing `usbipd-win` on the Windows host.
#[derive(Debug, Clone)]
pub struct HostStatus {
    pub usbipd_installed: bool,
    pub usbipd_version: Option<String>,
    pub devices: Vec<UsbDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub busid: String,
    pub vid_pid: String,
    pub name: String,
    pub state: String,
}

// ─── Pure parsers (unit-testable, compile everywhere) ──────────────────────

/// Decode `wsl.exe` output.
///
/// wsl.exe writes its own output (not the distro's output) in UTF-16LE on
/// Windows.  The practical trick: strip null bytes and interpret the rest
/// as UTF-8.  Works for both encodings because our expected output is pure
/// ASCII and UTF-8 ASCII never contains lone null bytes.
pub fn decode_wsl_output(bytes: &[u8]) -> String {
    let stripped: Vec<u8> = bytes.iter().copied().filter(|&b| b != 0).collect();
    String::from_utf8_lossy(&stripped).into_owned()
}

/// Parse `wsl --list --verbose` output.  Returns the list of distros plus
/// the default distro name (marked with `*` in the NAME column).
pub fn parse_distro_list(text: &str) -> (Vec<DistroInfo>, Option<String>) {
    let mut distros = Vec::new();
    let mut default = None;

    for line in text.lines() {
        let trimmed = line.trim_start_matches([' ', '\t', '\r']);
        // Skip blanks and the header row.
        if trimmed.is_empty() || trimmed.starts_with("NAME") {
            continue;
        }

        // Detect default marker: the `*` sits in the leftmost column before
        // the distro name.
        let (is_default, rest) = if let Some(stripped) = trimmed.strip_prefix('*') {
            (true, stripped.trim_start())
        } else {
            (false, trimmed)
        };

        // Columns are whitespace-separated: NAME STATE VERSION.
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() >= 3 {
            let info = DistroInfo {
                name: parts[0].to_string(),
                state: parts[1].to_string(),
                version: parts[2].to_string(),
            };
            if is_default {
                default = Some(info.name.clone());
            }
            distros.push(info);
        }
    }

    (distros, default)
}

/// Parse `usbipd list` output — only the "Connected:" section (the
/// "Persisted:" section is historical and uninteresting for Scope A).
pub fn parse_usbipd_list(text: &str) -> Vec<UsbDevice> {
    let mut devices = Vec::new();
    let mut in_connected = false;

    for raw in text.lines() {
        let line = raw.trim_end();
        let trimmed = line.trim_start();

        // Section transitions.
        if trimmed.starts_with("Connected:") {
            in_connected = true;
            continue;
        }
        if trimmed.starts_with("Persisted:") || trimmed.is_empty() {
            in_connected = false;
            continue;
        }
        // Column header.
        if trimmed.starts_with("BUSID") {
            continue;
        }
        if !in_connected {
            continue;
        }

        // Row format:
        //   BUSID  VID:PID    DEVICE name possibly with spaces    STATE
        //
        // STATE can be multi-word ("Not shared", "Shared (forced)") so we
        // match the known states at the end of the line rather than
        // splitting on whitespace and picking the last token.
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let busid = parts[0].to_string();
        let vid_pid = parts[1].to_string();
        let (name, state) = split_name_and_state(&parts[2..]);

        devices.push(UsbDevice {
            busid,
            vid_pid,
            name,
            state,
        });
    }

    devices
}

/// Split the "rest of the row" into (device-name, state) where state is
/// one of the known `usbipd` states.  Longest-match wins so that "Shared
/// (forced)" isn't misread as just "Shared".
fn split_name_and_state(parts: &[&str]) -> (String, String) {
    // Ordered longest-first.
    const KNOWN_STATES: &[&str] = &["Shared (forced)", "Not shared", "Attached", "Shared"];

    let joined = parts.join(" ");
    for state in KNOWN_STATES {
        if joined.ends_with(state) {
            let name_end = joined.len() - state.len();
            let name = joined[..name_end].trim_end().to_string();
            return (name, (*state).to_string());
        }
    }

    // Fallback: last whitespace-separated token is the state.
    if parts.len() >= 2 {
        let state = parts[parts.len() - 1].to_string();
        let name = parts[..parts.len() - 1].join(" ");
        (name, state)
    } else {
        (joined, String::new())
    }
}

/// Does this look like a video-capture device based on its name?
/// Heuristic-only — `usbipd list` doesn't expose USB class codes.
pub fn is_likely_camera(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("camera") || lower.contains("webcam") || lower.contains("uvc")
}

/// Distros that ship with WSL but aren't useful for running a node:
/// Docker Desktop's internal distros have no package manager available
/// to a user and no udev/v4l2 stack, so we filter them out of distro
/// selection.
pub fn is_internal_distro(name: &str) -> bool {
    matches!(name, "docker-desktop" | "docker-desktop-data")
}

// ─── Shell-out probes ───────────────────────────────────────────────────────

/// Probe WSL on the host.  Returns `installed=false` if `wsl.exe` isn't
/// on PATH (i.e. WSL isn't installed at all) or if the command errors —
/// both cases are user-visible as "WSL is not installed".
pub fn probe_wsl() -> WslStatus {
    let output = Command::new("wsl").args(["--list", "--verbose"]).output();

    match output {
        Ok(out) if out.status.success() => {
            let text = decode_wsl_output(&out.stdout);
            let (distros, default) = parse_distro_list(&text);
            WslStatus {
                installed: true,
                distros,
                default_distro: default,
            }
        }
        // Either wsl.exe isn't there, or it errored — treat as not-installed.
        _ => WslStatus {
            installed: false,
            distros: vec![],
            default_distro: None,
        },
    }
}

/// Probe a specific distro for FFmpeg.
pub fn probe_distro(distro: &str) -> DistroStatus {
    let has = Command::new("wsl")
        .args(["-d", distro, "--", "which", "ffmpeg"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);

    let version = if has {
        Command::new("wsl")
            .args(["-d", distro, "--", "ffmpeg", "-version"])
            .output()
            .ok()
            .and_then(|o| {
                let text = String::from_utf8_lossy(&o.stdout).into_owned();
                text.lines().next().map(|s| s.trim().to_string())
            })
    } else {
        None
    };

    DistroStatus {
        name: distro.to_string(),
        has_ffmpeg: has,
        ffmpeg_version: version,
    }
}

/// Probe `usbipd-win` on the Windows host.
pub fn probe_usbipd() -> HostStatus {
    // `--version` exits 0 only if usbipd is actually installed.
    let installed = Command::new("usbipd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if !installed {
        return HostStatus {
            usbipd_installed: false,
            usbipd_version: None,
            devices: vec![],
        };
    }

    let version = Command::new("usbipd")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.lines().next().map(|s| s.trim().to_string())
        });

    let devices = Command::new("usbipd")
        .arg("list")
        .output()
        .ok()
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout).into_owned();
            parse_usbipd_list(&text)
        })
        .unwrap_or_default();

    HostStatus {
        usbipd_installed: true,
        usbipd_version: version,
        devices,
    }
}

/// Install FFmpeg inside the named WSL distro via passwordless sudo.
///
/// This is non-elevated from the Windows side: `wsl -d <distro> -- sudo
/// apt-get install -y ffmpeg` works because WSL's default user has
/// passwordless sudo out of the box.  Takes ~30–60s on a fresh distro.
pub fn install_ffmpeg_in_distro(distro: &str) -> Result<()> {
    let update = Command::new("wsl")
        .args(["-d", distro, "--", "sudo", "apt-get", "update", "-qq"])
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to invoke wsl.exe: {}", e))?;
    if !update.status.success() {
        let stderr = String::from_utf8_lossy(&update.stderr);
        let first = stderr.lines().next().unwrap_or("(no stderr)").trim();
        anyhow::bail!("apt-get update failed in {}: {}", distro, first);
    }

    let install = Command::new("wsl")
        .args([
            "-d",
            distro,
            "--",
            "sudo",
            "apt-get",
            "install",
            "-y",
            "-qq",
            "ffmpeg",
        ])
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to invoke wsl.exe: {}", e))?;
    if !install.status.success() {
        let stderr = String::from_utf8_lossy(&install.stderr);
        let first = stderr.lines().next().unwrap_or("(no stderr)").trim();
        anyhow::bail!("apt-get install ffmpeg failed in {}: {}", distro, first);
    }

    Ok(())
}

// ─── Interactive flow ───────────────────────────────────────────────────────

/// Run the full WSL2 preflight inside the existing "Step 3 — Install" panel.
///
/// Assumes the caller has already opened the panel (`panel_top(...)`).
/// Emits rows inside the panel and only breaks out of it for interactive
/// prompts, re-opening the panel afterwards so the caller's closing
/// `panel_bottom()` still matches up.
///
/// Does not return errors for missing-prereqs (WSL, distro, usbipd) —
/// those are displayed as warnings with actionable guidance and the user
/// is expected to complete them out-of-band.  Only unexpected failures
/// (e.g. wsl.exe launched but panicked) bubble up.
pub fn run_wsl_preflight_interactive(config: &SetupConfig) -> Result<()> {
    panel_blank();
    panel_mid("WSL2 Environment Check");
    panel_blank();

    let mut spinner = Spinner::new();

    // ── 1. WSL installed? ───────────────────────────────────────────────
    panel_spinner_row(&spinner.advance(), "Checking WSL on host...");
    flush();
    let wsl = probe_wsl();
    print!("\r");
    flush();

    if !wsl.installed {
        panel_warn("WSL is not installed on this Windows host");
        panel_sub("Open PowerShell as Administrator and run:");
        panel_sub(&format!("    {}", "wsl --install".cyan()));
        panel_sub("A reboot is required after first-time install.");
        panel_sub("Re-run CameraNode setup once WSL and a distro are ready.");
        return Ok(());
    }
    panel_check("WSL is installed");

    // ── 2. A usable distro? ─────────────────────────────────────────────
    let usable: Vec<&DistroInfo> = wsl
        .distros
        .iter()
        .filter(|d| !is_internal_distro(&d.name))
        .collect();

    if usable.is_empty() {
        panel_warn("No usable WSL distro found");
        if !wsl.distros.is_empty() {
            panel_sub(
                "Only Docker Desktop's internal distros are present — they cannot stream cameras.",
            );
        }
        panel_sub("Install Ubuntu (admin PowerShell):");
        panel_sub(&format!("    {}", "wsl --install -d Ubuntu".cyan()));
        return Ok(());
    }

    let distro_name = pick_distro(&wsl, &usable)?;
    panel_check(&format!("Distro selected: {}", distro_name.cyan()));

    // Persist the choice for the runtime launcher (Scope B will consume it).
    let _ = save_wsl_distro(config, &distro_name);

    // ── 3. FFmpeg inside the distro? ────────────────────────────────────
    panel_spinner_row(
        &spinner.advance(),
        &format!("Checking FFmpeg in {}...", distro_name),
    );
    flush();
    let distro_status = probe_distro(&distro_name);
    print!("\r");
    flush();

    if distro_status.has_ffmpeg {
        let version = distro_status
            .ffmpeg_version
            .as_deref()
            .unwrap_or("(version unknown)");
        panel_check(&format!("FFmpeg present — {}", version));
    } else {
        panel_warn(&format!("FFmpeg is not installed in {}", distro_name));

        // Break out of the panel for the Confirm prompt.
        panel_blank();
        panel_bottom();
        println!();

        let do_install = Confirm::new("  Install FFmpeg now via apt-get? (~60s)")
            .with_default(true)
            .prompt()
            .unwrap_or(false);

        println!();
        panel_top("Step 3 / 5 — Installing Dependencies");
        panel_blank();

        if do_install {
            panel_spinner_row(
                &spinner.advance(),
                &format!("Running apt-get install ffmpeg in {}...", distro_name),
            );
            flush();

            match install_ffmpeg_in_distro(&distro_name) {
                Ok(()) => {
                    print!("\r");
                    flush();
                    panel_check("FFmpeg installed");
                }
                Err(e) => {
                    print!("\r");
                    flush();
                    panel_warn(&format!("Automatic install failed: {}", e));
                    panel_sub("Try it manually:");
                    panel_sub(&format!(
                        "    {}",
                        format!("wsl -d {} -- sudo apt-get install -y ffmpeg", distro_name).cyan()
                    ));
                }
            }
        } else {
            panel_sub("Install it later with:");
            panel_sub(&format!(
                "    {}",
                format!("wsl -d {} -- sudo apt-get install -y ffmpeg", distro_name).cyan()
            ));
        }
    }

    // ── 4. usbipd-win on host? ──────────────────────────────────────────
    panel_spinner_row(&spinner.advance(), "Checking usbipd-win on host...");
    flush();
    let host = probe_usbipd();
    print!("\r");
    flush();

    if !host.usbipd_installed {
        panel_warn("usbipd-win is not installed on the Windows host");
        panel_sub("usbipd-win forwards USB cameras from Windows into WSL.");
        panel_sub("Install via admin PowerShell:");
        panel_sub(&format!(
            "    {}",
            "winget install --id dorssel.usbipd-win".cyan()
        ));
        panel_sub("Or: https://github.com/dorssel/usbipd-win/releases");
    } else {
        let version_suffix = host
            .usbipd_version
            .as_deref()
            .map(|v| format!(" ({})", v))
            .unwrap_or_default();
        panel_check(&format!("usbipd-win installed{}", version_suffix));

        let cameras: Vec<&UsbDevice> = host
            .devices
            .iter()
            .filter(|d| is_likely_camera(&d.name))
            .collect();

        if cameras.is_empty() {
            panel_warn("No USB cameras detected by usbipd on the host");
            panel_sub("Plug in a camera, then (admin PowerShell):");
            panel_sub(&format!("    {}", "usbipd list".cyan()));
            panel_sub("Find the BUSID of the camera, then:");
            panel_sub(&format!("    {}", "usbipd bind --busid X-Y".cyan()));
            panel_sub(&format!(
                "    {}",
                format!("usbipd attach --wsl --busid X-Y --distribution {}", distro_name).cyan()
            ));
        } else {
            panel_check(&format!("Found {} USB camera(s) on host:", cameras.len()));
            for c in &cameras {
                panel_sub(&format!(
                    "{}  {}  {}",
                    c.busid.cyan(),
                    c.name,
                    format!("[{}]", c.state).dimmed()
                ));
            }

            let needs_attach: Vec<&&UsbDevice> =
                cameras.iter().filter(|c| c.state != "Attached").collect();
            if !needs_attach.is_empty() {
                panel_sub("To forward a camera into WSL (admin PowerShell):");
                for c in &needs_attach {
                    panel_sub(&format!(
                        "    {}",
                        format!("usbipd bind --busid {}", c.busid).cyan()
                    ));
                    panel_sub(&format!(
                        "    {}",
                        format!(
                            "usbipd attach --wsl --busid {} --distribution {}",
                            c.busid, distro_name
                        )
                        .cyan()
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Pick which distro to use.  Auto-selects if only one is usable;
/// prompts the user otherwise.  Prefers the system default if it's
/// usable.
fn pick_distro(wsl: &WslStatus, usable: &[&DistroInfo]) -> Result<String> {
    if usable.len() == 1 {
        return Ok(usable[0].name.clone());
    }

    // If the default is usable, use it.
    if let Some(def) = &wsl.default_distro {
        if usable.iter().any(|d| d.name == *def) {
            return Ok(def.clone());
        }
    }

    // Multiple candidates, no good default — prompt.  Break out of the
    // panel for the prompt, reopen after.
    panel_blank();
    panel_bottom();
    println!();
    let names: Vec<String> = usable.iter().map(|d| d.name.clone()).collect();
    let selection = Select::new("  Which WSL distro should CameraNode run in?", names)
        .prompt()
        .map_err(|e| anyhow::anyhow!("Distro selection cancelled: {}", e))?;
    println!();
    panel_top("Step 3 / 5 — Installing Dependencies");
    panel_blank();
    Ok(selection)
}

/// Persist the selected WSL distro to the node database so the runtime
/// launcher (Scope B) knows which distro to invoke.  Errors here are
/// non-fatal — we log them but don't interrupt setup.
fn save_wsl_distro(_config: &SetupConfig, distro: &str) -> Result<()> {
    let db_path = crate::paths::config_db_path();
    let db = crate::storage::NodeDatabase::new(&db_path)
        .map_err(|e| anyhow::anyhow!("DB error: {}", e))?;
    db.set_config("wsl.distro", distro)
        .map_err(|e| anyhow::anyhow!("Config save error: {}", e))?;
    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── decode_wsl_output ───────────────────────────────────────────────

    #[test]
    fn decode_utf16le_strips_null_pads() {
        // "Ubuntu" in UTF-16LE: each char followed by 0x00.
        let bytes = b"U\0b\0u\0n\0t\0u\0";
        assert_eq!(decode_wsl_output(bytes), "Ubuntu");
    }

    #[test]
    fn decode_utf8_passes_through() {
        assert_eq!(decode_wsl_output(b"Ubuntu"), "Ubuntu");
    }

    #[test]
    fn decode_empty_is_empty() {
        assert_eq!(decode_wsl_output(b""), "");
    }

    #[test]
    fn decode_real_wsl_list_header() {
        // The real "  NAME    STATE    VERSION" line in UTF-16LE.
        let bytes = b"\x20\0\x20\0N\0A\0M\0E\0";
        let out = decode_wsl_output(bytes);
        assert_eq!(out, "  NAME");
    }

    // ── parse_distro_list ───────────────────────────────────────────────

    #[test]
    fn distro_list_single_default() {
        let text = "  NAME      STATE     VERSION\n\
                    * Ubuntu    Running   2\n";
        let (distros, default) = parse_distro_list(text);
        assert_eq!(distros.len(), 1);
        assert_eq!(distros[0].name, "Ubuntu");
        assert_eq!(distros[0].state, "Running");
        assert_eq!(distros[0].version, "2");
        assert_eq!(default.as_deref(), Some("Ubuntu"));
    }

    #[test]
    fn distro_list_multiple_mixed_states() {
        let text = "  NAME              STATE       VERSION\n\
                    * Ubuntu            Running     2\n  \
                    docker-desktop      Running     2\n  \
                    Debian              Stopped     2\n";
        let (distros, default) = parse_distro_list(text);
        assert_eq!(distros.len(), 3);
        assert_eq!(distros[0].name, "Ubuntu");
        assert_eq!(distros[1].name, "docker-desktop");
        assert_eq!(distros[2].name, "Debian");
        assert_eq!(default.as_deref(), Some("Ubuntu"));
    }

    #[test]
    fn distro_list_empty_has_no_default() {
        let text = "  NAME  STATE  VERSION\n";
        let (distros, default) = parse_distro_list(text);
        assert!(distros.is_empty());
        assert!(default.is_none());
    }

    #[test]
    fn distro_list_tolerates_trailing_whitespace_and_carriage_returns() {
        let text = "  NAME    STATE    VERSION   \r\n\
                    * Ubuntu  Running  2   \r\n";
        let (distros, default) = parse_distro_list(text);
        assert_eq!(distros.len(), 1);
        assert_eq!(distros[0].name, "Ubuntu");
        assert_eq!(default.as_deref(), Some("Ubuntu"));
    }

    // ── parse_usbipd_list ───────────────────────────────────────────────

    #[test]
    fn usbipd_list_parses_cameras_with_multiword_names() {
        let text = "Connected:\n\
                    BUSID  VID:PID    DEVICE                                              STATE\n\
                    2-1    0c45:6366  MEE USB Camera, Realtek USB2.0 Audio                Not shared\n\
                    2-4    8087:0029  Intel(R) Wireless Bluetooth(R)                      Not shared\n\
                    \n\
                    Persisted:\n\
                    GUID                                  DEVICE\n\
                    some-guid                             Historical\n";
        let devices = parse_usbipd_list(text);
        assert_eq!(devices.len(), 2);

        assert_eq!(devices[0].busid, "2-1");
        assert_eq!(devices[0].vid_pid, "0c45:6366");
        assert!(
            devices[0].name.contains("MEE USB Camera"),
            "got name: {:?}",
            devices[0].name
        );
        assert_eq!(devices[0].state, "Not shared");

        assert_eq!(devices[1].busid, "2-4");
        assert!(devices[1].name.contains("Bluetooth"));
        assert_eq!(devices[1].state, "Not shared");
    }

    #[test]
    fn usbipd_list_empty_connected_section() {
        let text = "Connected:\n\
                    BUSID  VID:PID  DEVICE  STATE\n\
                    \n\
                    Persisted:\n";
        let devices = parse_usbipd_list(text);
        assert!(devices.is_empty());
    }

    #[test]
    fn usbipd_list_distinguishes_multiword_states() {
        let text = "Connected:\n\
                    BUSID  VID:PID    DEVICE                STATE\n\
                    2-1    0c45:6366  USB Camera            Attached\n\
                    2-2    abcd:1234  Widget Device         Shared\n\
                    3-1    0000:0001  Forced Device         Shared (forced)\n\
                    \n";
        let devices = parse_usbipd_list(text);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].state, "Attached");
        assert_eq!(devices[1].state, "Shared");
        assert_eq!(devices[2].state, "Shared (forced)");
        assert_eq!(devices[0].name, "USB Camera");
        assert_eq!(devices[1].name, "Widget Device");
        assert_eq!(devices[2].name, "Forced Device");
    }

    #[test]
    fn usbipd_list_ignores_persisted_section() {
        let text = "Persisted:\n\
                    GUID                                  DEVICE\n\
                    abc-123                               Old Camera\n";
        let devices = parse_usbipd_list(text);
        // No "Connected:" section → no devices emitted.
        assert!(devices.is_empty());
    }

    // ── is_likely_camera ────────────────────────────────────────────────

    #[test]
    fn likely_camera_positives() {
        assert!(is_likely_camera("MEE USB Camera"));
        assert!(is_likely_camera("Integrated Camera"));
        assert!(is_likely_camera("USB2.0 HD UVC WebCam"));
        assert!(is_likely_camera("Logitech webcam"));
        assert!(is_likely_camera("C920 UVC Gadget"));
    }

    #[test]
    fn likely_camera_negatives() {
        assert!(!is_likely_camera("Intel(R) Wireless Bluetooth"));
        assert!(!is_likely_camera("USB Mass Storage Device"));
        assert!(!is_likely_camera("Yubikey"));
    }

    // ── is_internal_distro ──────────────────────────────────────────────

    #[test]
    fn internal_distros_are_filtered() {
        assert!(is_internal_distro("docker-desktop"));
        assert!(is_internal_distro("docker-desktop-data"));
        assert!(!is_internal_distro("Ubuntu"));
        assert!(!is_internal_distro("Debian"));
    }

    // ── split_name_and_state (internal but worth covering) ──────────────

    #[test]
    fn split_handles_state_substring_in_name() {
        // A device named "Attached Storage" in "Not shared" state: the
        // state match must be "Not shared" (longest trailing match), not
        // get confused by "Attached" appearing in the middle.
        let parts = ["Attached", "Storage", "Not", "shared"];
        let (name, state) = split_name_and_state(&parts);
        assert_eq!(name, "Attached Storage");
        assert_eq!(state, "Not shared");
    }

    #[test]
    fn split_falls_back_on_unknown_state() {
        // Unknown trailing word — still return something non-empty.
        let parts = ["Some", "Device", "WeirdState"];
        let (name, state) = split_name_and_state(&parts);
        assert_eq!(name, "Some Device");
        assert_eq!(state, "WeirdState");
    }
}
