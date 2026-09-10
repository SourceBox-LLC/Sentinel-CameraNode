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
//! Camera handling - detection, capture, and streaming
//!
//! This module provides platform-agnostic camera detection and video capture
//! functionality for Sentinel CameraNode.
//!
//! # Platform Support
//!
//! - **Linux**: Uses v4l2 API to access `/dev/video*` devices
//! - **Windows**: Uses DirectShow via FFmpeg to enumerate devices
//! - **macOS**: Uses AVFoundation via FFmpeg to enumerate devices

mod detector;
mod platform;
mod types;

pub use detector::{
    detect_cameras, is_valid_device_path, validate_device_available, DetectedCamera,
};
pub use types::{CameraCapabilities, CameraStatus};
