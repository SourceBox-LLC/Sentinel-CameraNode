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
//! Platform-specific camera detection
//!
//! This module provides platform-agnostic camera detection through platform-specific
//! implementations for Linux (v4l2), Windows (DirectShow), and macOS (AVFoundation).

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

use crate::camera::DetectedCamera;
use crate::error::Result;

/// Platform-agnostic camera detector trait
pub trait CameraDetector: Send + Sync {
    /// Detect all available cameras
    fn detect_cameras(&self) -> Result<Vec<DetectedCamera>>;

    /// Get platform name for display
    fn platform_name(&self) -> &'static str;
}

/// Create platform-specific detector
pub fn create_detector() -> Box<dyn CameraDetector> {
    #[cfg(target_os = "linux")]
    {
        Box::new(linux::LinuxDetector::new())
    }

    #[cfg(target_os = "windows")]
    {
        Box::new(windows::WindowsDetector::new())
    }

    #[cfg(target_os = "macos")]
    {
        Box::new(macos::MacOSDetector::new())
    }
}

/// Check if device path is valid for current platform
pub fn is_valid_device_path(_path: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        _path.starts_with("/dev/video")
    }

    #[cfg(target_os = "windows")]
    {
        // DirectShow uses device names, not paths
        true
    }

    #[cfg(target_os = "macos")]
    {
        // AVFoundation uses numeric indices as strings
        _path.parse::<u32>().is_ok()
    }
}

/// Verify the device is actually present and usable *before* we hand it
/// to FFmpeg.  Returns `Ok(())` if the device can be opened, or an
/// error with a concrete, actionable message otherwise.
///
/// This is the difference between a clean "No such device: /dev/video0
/// — on WSL2 USB passthrough is required" failure at the front door and
/// a cryptic asynchronous FFmpeg death 500ms later that the operator
/// has to dig through logs to interpret.
///
/// Only Linux does filesystem validation — on Windows the "path" is a
/// DirectShow camera name and on macOS it's an AVFoundation index, so
/// those branches pass through.  `is_valid_device_path` handles the
/// format check; this one handles actual existence + character-device
/// kind + readability.
pub fn validate_device_available(_path: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use crate::error::Error;
        use std::os::unix::fs::FileTypeExt;

        let meta = std::fs::metadata(_path).map_err(|e| {
            // Distinguish "not there at all" from "there but we can't
            // see it" — the first is almost always a WSL passthrough
            // or udev issue, the second is almost always permissions.
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::Camera(format!(
                    "Camera device '{}' not found. On bare Linux, confirm a USB camera is \
                     plugged in and visible via `ls /dev/video*`. On WSL2, native USB cameras \
                     are not accessible without manual `usbipd` passthrough.",
                    _path
                ))
            } else {
                Error::Camera(format!(
                    "Cannot access camera device '{}': {}. Check that the current user is in \
                     the 'video' group (`usermod -a -G video $USER`, then log out/in).",
                    _path, e
                ))
            }
        })?;

        // v4l2 devices are character devices; a regular file at
        // /dev/video0 would mean something is very wrong (and FFmpeg
        // would consume it as a video file, which is not what anyone
        // wants).
        if !meta.file_type().is_char_device() {
            return Err(Error::Camera(format!(
                "Path '{}' exists but is not a character device — expected a v4l2 node.",
                _path
            )));
        }

        // Best-effort readability probe: opening the device for read
        // catches the common "exists but owned by root" case without
        // requiring /sys walks.  Dropping the handle here is fine —
        // FFmpeg will reopen it momentarily.
        std::fs::File::open(_path).map_err(|e| {
            Error::Camera(format!(
                "Camera device '{}' exists but is not readable: {}. This usually means the \
                 current user is not in the 'video' group.",
                _path, e
            ))
        })?;

        Ok(())
    }

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        // Windows DShow names and macOS AVFoundation indices have no
        // filesystem presence to check — FFmpeg enumerates those
        // internally.  Format validation already happened in
        // `is_valid_device_path`.
        let _ = _path;
        Ok(())
    }
}
