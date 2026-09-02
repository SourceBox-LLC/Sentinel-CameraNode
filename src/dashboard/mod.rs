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

//! Live node dashboard.
//!
//! Renders a persistent full-screen TUI that updates in place. Replaces
//! raw tracing log output while the node is running.
//!
//! # Layout
//!
//! ```text
//! ╔══ ▸ SENTINEL CLOUDNODE ═════════════════════════════════════════════════╗
//! ║  Node: abc12345  │  API: sentinel-command.com         │  ↑ 142 segments ║
//! ╠══ CAMERAS ══════════════════════════════════════════════════════════════╣
//! ║  ● MEE USB Camera    1920×1080   avc1.42e01e / mp4a.40.2   streaming   ║
//! ╠══ LOG ══════════════════════════════════════════════════════════════════╣
//! ║  06:31:12  ✓  Segment 00142 uploaded (188 KB)                          ║
//! ║  06:31:08  ✓  Codec reported: avc1.42e01e, mp4a.40.2                   ║
//! ║  06:31:05  ✓  Registered with cloud                                    ║
//! ╚════════════════════════════════════════════════════════════════════════╝
//! ```
//!
//! # Module layout
//!
//! Pre-split this was one 1,761-line file mixing data types, state mutations,
//! TUI rendering, slash-command handling, and the input event loop. Now:
//!
//! - [`types`]    — small data structs and enums shared everywhere else
//!                  (`LogLevel`, `LogEntry`, `CameraState`, `CameraStatus`,
//!                  `View`, `SettingsInfo`).
//! - [`state`]    — `DashboardState` plus its own state-mutation methods.
//!                  Lives behind a `Mutex` inside [`handle::Dashboard`].
//! - [`handle`]   — the public `Dashboard` wrapper most of the codebase
//!                  imports. Cheap-to-clone `Arc<Mutex<DashboardState>>`
//!                  with the lifecycle / setup methods (`new`, `log_*`,
//!                  `set_db`, `set_disabled_cameras`, ...).
//! - [`render`]   — the giant `render()` method plus the format helpers
//!                  (panel borders, settings divider, status pill, etc.).
//!                  Box-drawing constants live here too, private.
//! - [`commands`] — `run_render_loop` (the input-event loop), the
//!                  `execute_command` slash-command dispatcher, and the
//!                  destructive-command confirm flow. Tests for the
//!                  confirm flow live next to the impl in this file.
//!
//! Internal types (`DashboardState`, the helpers) stay reachable across
//! sibling modules via the re-exports below; external callers only see
//! the public surface re-exported from here.

mod commands;
mod handle;
mod render;
mod state;
mod types;

pub use handle::Dashboard;
pub use state::{DashboardState, CONFIRM_TIMEOUT};
pub use types::{
    CameraState,
    CameraStatus,
    LogEntry,
    LogLevel,
    SettingsInfo,
    View,
};
