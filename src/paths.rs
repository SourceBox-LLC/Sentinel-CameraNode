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

//! Centralised data-directory resolution.
//!
//! CameraNode persists three things to disk: the encrypted SQLite config
//! database (`node.db`), recordings (`recordings/`), and the machine-id
//! fallback file used as the AES key seed on minimal Linux images. All
//! three need to agree on *where* "the data dir" is.
//!
//! Historically the answer was just `./data/` relative to the process's
//! working directory. That worked when CameraNode was launched manually
//! from a terminal, but the moment we ship a Windows MSI that registers
//! a Windows Service, the cwd becomes `C:\Windows\System32` (Service
//! Control Manager's default) and the relative path resolves to a
//! directory the LocalSystem account has no business writing into.
//!
//! Resolution order (first match wins):
//!
//! 1. `$SOURCEBOX_SENTRY_DATA_DIR` if set — explicit override, used by Docker
//!    and by the MSI-installed service registration.
//! 2. The legacy `./data/` directory if it already exists, so anyone who
//!    upgraded a manual `cargo build` install in-place keeps working.
//! 3. Platform default:
//!    - Windows: `%ProgramData%\SourceBoxSentry` (standard system-wide app
//!      data location; writable by services running as LocalSystem and
//!      by interactive admins).
//!    - Other: `./data` (matches legacy behaviour for Linux/macOS where
//!      the install scripts already drop the binary in a directory the
//!      user owns).
//!
//! The function is pure — it does NOT create the directory. Callers
//! that need it to exist should call `std::fs::create_dir_all` after.

use std::path::PathBuf;

/// Where CameraNode stores its config DB, recordings, and any other
/// persistent state. See module docs for the resolution order.
pub fn data_dir() -> PathBuf {
    // 1. Explicit override.
    if let Ok(dir) = std::env::var("SOURCEBOX_SENTRY_DATA_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }

    // 2. Legacy in-place ./data/ check (Linux/macOS only; v0.1.36+).
    //
    //    Originally a single existence check (`legacy.exists()`).
    //    Tightened in v0.1.34 to `legacy.join("node.db").exists()`.
    //    Removed entirely on Windows in v0.1.36 because the
    //    cwd-relative semantics interact badly with every Windows
    //    launch context — Start-menu-shortcut WorkingDirectory,
    //    Explorer double-click, MSI Finish-dialog cmd, PowerShell —
    //    all set cwd differently, and stale `<cwd>/data/node.db`
    //    files from previous buggy versions (or from running the
    //    binary out of a project repo, or from a `.bat` on Desktop)
    //    would override the platform default and route writes into
    //    locations the user can't write to (e.g. UAC-filtered
    //    Program Files\... while running as a "non-elevated admin").
    //
    //    The footgun history:
    //      v0.1.20-v0.1.33: any `./data/` folder hijacked.
    //      v0.1.34: required `./data/node.db` — better, but still
    //        broken when stale node.db files squat in old install
    //        directories (the MSI uninstaller doesn't remove
    //        runtime-created files).
    //      v0.1.36: Windows always uses the platform default (step 3).
    //        cargo-build dev users on Windows can override with
    //        SOURCEBOX_SENTRY_DATA_DIR.
    //
    //    Linux/macOS keep the legacy check because cwd is much more
    //    predictable in those environments (one shell, no Start menu
    //    shortcuts) and cargo-build dev workflows depend on it.
    #[cfg(not(target_os = "windows"))]
    {
        let legacy = PathBuf::from("./data");
        if legacy.join("node.db").exists() {
            return legacy;
        }
    }

    // 3. Platform default.
    #[cfg(target_os = "windows")]
    {
        // Try the env var first — it's the "right" way and lets a
        // future enterprise install relocate ProgramData (rare).
        if let Ok(programdata) = std::env::var("ProgramData") {
            if !programdata.is_empty() {
                return PathBuf::from(programdata).join("SourceBoxSentry");
            }
        }
        // Hardcoded fallback. We don't reach this for interactive
        // user contexts (Windows always sets ProgramData for them),
        // but **Windows Services running as LocalSystem don't always
        // inherit the ProgramData env var** — observed empirically on
        // Windows 11 26200 where the SCM-managed environment block
        // omits it. Falling through to "./data" was disastrous: cwd
        // for a service is C:\Windows\System32, so data_dir resolved
        // to C:\Windows\System32\data which is unwritable + unfindable
        // by Config::load. Hardcode the canonical Windows location
        // (stable since Vista, documented at
        // KNOWNFOLDERID FOLDERID_ProgramData = "{0x62AB5D82,...}")
        // so the service finds the same files the setup wizard wrote.
        return PathBuf::from(r"C:\ProgramData").join("SourceBoxSentry");
    }

    // Final fallback: keep the legacy relative path so a fresh non-MSI
    // install on Linux/macOS behaves the same as before this refactor.
    #[cfg(not(target_os = "windows"))]
    {
        PathBuf::from("./data")
    }
}

/// Where the encrypted config SQLite lives.
///
/// Convenience wrapper — every caller wants `data_dir().join("node.db")`,
/// and centralising it means a future move to `config.db` or a versioned
/// filename is one edit, not a grep-and-replace across the codebase.
pub fn config_db_path() -> PathBuf {
    data_dir().join("node.db")
}
