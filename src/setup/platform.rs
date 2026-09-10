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
//! Platform detection and system information

use anyhow::Result;
use std::env;
use sysinfo::System;

#[derive(Debug, Clone)]
pub struct PlatformInfo {
    pub os: String,
    pub os_version: String,
    pub arch: String,
    pub is_windows: bool,
    pub is_linux: bool,
    pub is_macos: bool,
    pub hostname: String,
}

impl PlatformInfo {
    pub fn detect() -> Result<Self> {
        let mut sys = System::new_all();
        sys.refresh_all();

        let os = env::consts::OS.to_string();
        let arch = env::consts::ARCH.to_string();

        let os_version = match System::name() {
            Some(name) => {
                let version = System::os_version().unwrap_or_else(|| "unknown".to_string());
                format!("{} {}", name, version)
            }
            None => "unknown".to_string(),
        };

        let hostname = System::host_name().unwrap_or_else(|| "unknown".to_string());

        Ok(Self {
            os: os.clone(),
            os_version,
            arch,
            is_windows: cfg!(target_os = "windows"),
            is_linux: cfg!(target_os = "linux"),
            is_macos: cfg!(target_os = "macos"),
            hostname,
        })
    }

    pub fn display(&self) -> String {
        format!("{} ({})", self.os_version, self.arch)
    }
}

/// Find the FFmpeg executable path — delegates to the shared
/// `streaming::find_ffmpeg` so the setup-time check matches runtime exactly.
/// Keeping these in sync prevents "setup said ffmpeg was installed but
/// the node can't find it" mysteries.
fn find_ffmpeg_path() -> String {
    crate::streaming::find_ffmpeg()
}

/// Check if FFmpeg is available
pub fn check_ffmpeg() -> Result<bool> {
    let ffmpeg = find_ffmpeg_path();
    let output = std::process::Command::new(&ffmpeg)
        .arg("-version")
        .output();

    Ok(output.is_ok())
}

/// Get FFmpeg version string
pub fn get_ffmpeg_version() -> Option<String> {
    let ffmpeg = find_ffmpeg_path();
    let output = std::process::Command::new(&ffmpeg)
        .arg("-version")
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next()?;

    // Parse "ffmpeg version 8.1-essentials_build-www.gyan.dev"
    let version = first_line.split_whitespace().nth(2)?.to_string();

    Some(version)
}
