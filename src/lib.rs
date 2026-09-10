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
//! Sentinel CameraNode Library
//!
//! Core functionality for camera detection, capture, streaming, and cloud communication.

pub mod api;
pub mod camera;
pub mod config;
pub mod dashboard;
pub mod error;
pub mod logging;
pub mod paths;
pub mod server;
pub mod storage;
pub mod streaming;
pub mod node;
pub mod setup;

// Windows Service entry point. Compiled only on Windows because
// `windows-service` isn't a portable crate; on other platforms the
// `service` subcommand short-circuits with a friendly error.
#[cfg(target_os = "windows")]
pub mod service;

// Re-exports for convenience
pub use config::{Config, CliOverrides};
pub use dashboard::Dashboard;
pub use error::{Error, Result};
pub use node::Node;