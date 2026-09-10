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
//! Windows camera detection using DirectShow via FFmpeg
//!
//! Uses FFmpeg to enumerate DirectShow video devices on Windows.

use std::process::Command;

use super::CameraDetector;
use crate::camera::types::CameraCapabilities;
use crate::camera::DetectedCamera;
use crate::error::Result;

pub struct WindowsDetector;

impl WindowsDetector {
    pub fn new() -> Self {
        Self
    }

    /// Parse FFmpeg DirectShow device list output (supports both old and new FFmpeg formats)
    fn parse_dshow_devices(output: &str) -> Vec<String> {
        let mut devices = Vec::new();
        let mut in_video_section = true; // Track which section we're in (old format)

        for line in output.lines() {
            // Format 1 (Old FFmpeg): [dshow @ 0x...] DirectShow video devices
            // Format 2 (New FFmpeg): [in#0 @ 0x...] "Camera Name" (video)

            // Old format: Track section changes
            if line.contains("DirectShow video devices") {
                in_video_section = true;
            } else if line.contains("DirectShow audio devices") {
                in_video_section = false;
            }

            // Skip audio devices (new format)
            if line.contains("(audio)") {
                continue;
            }

            // Look for video devices with "(video)" marker (new format)
            if line.contains("(video)") {
                // Extract camera name between quotes
                // Line format: [in#0 @ 0x...] "Camera Name" (video)
                if let Some(start) = line.find('"') {
                    if let Some(end) = line[start + 1..].find('"') {
                        let name = &line[start + 1..start + 1 + end];
                        if !name.is_empty() && !name.starts_with("@device") {
                            devices.push(name.to_string());
                        }
                    }
                }
                continue;
            }

            // Old format: Extract from lines with quotes after "DirectShow video devices"
            // Format: [dshow @ 0x...]  "Camera Name"
            // Only process if we're in video section and line has a quoted name
            if in_video_section
                && line.contains('"')
                && !line.contains("DirectShow")
                && !line.contains("Alternative")
            {
                if let Some(start) = line.find('"') {
                    if let Some(end) = line[start + 1..].find('"') {
                        let name = &line[start + 1..start + 1 + end];
                        if !name.is_empty() && !name.starts_with("@device") {
                            devices.push(name.to_string());
                        }
                    }
                }
            }
        }

        devices
    }
}

impl CameraDetector for WindowsDetector {
    fn detect_cameras(&self) -> Result<Vec<DetectedCamera>> {
        tracing::info!("Scanning for cameras on Windows (DirectShow)...");

        // Find FFmpeg - check local path first, then system PATH
        let ffmpeg_path = self.find_ffmpeg();

        // Use FFmpeg to list DirectShow devices
        let output = Command::new(&ffmpeg_path)
            .args(["-list_devices", "true", "-f", "dshow", "-i", "dummy"])
            .output()?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        let camera_names = Self::parse_dshow_devices(&stderr);

        if camera_names.is_empty() {
            tracing::warn!("No cameras found via DirectShow. FFmpeg output:");
            tracing::warn!("{}", stderr);
        }

        let mut cameras = Vec::new();

        for name in camera_names {
            tracing::info!("Detected camera: {}", name);

            cameras.push(DetectedCamera {
                device_path: name.clone(), // Use name as path on Windows
                name,
                capabilities: CameraCapabilities {
                    streaming: true,
                    hardware_encoding: false,
                    formats: vec!["dshow".to_string(), "mjpeg".to_string()],
                },
                supported_resolutions: vec![
                    (1920, 1080), // 1080p
                    (1280, 720),  // 720p
                    (640, 480),   // VGA
                ],
                preferred_resolution: (1280, 720),
            });
        }

        tracing::info!("Found {} camera(s)", cameras.len());
        Ok(cameras)
    }

    fn platform_name(&self) -> &'static str {
        "Windows (DirectShow)"
    }
}

impl WindowsDetector {
    /// Find FFmpeg executable — delegates to the shared
    /// `streaming::find_ffmpeg` so detection and runtime resolve the
    /// same binary.  Since v0.1.35 that's PATH only — the bundled
    /// `./ffmpeg/bin/ffmpeg.exe` path was removed.
    fn find_ffmpeg(&self) -> String {
        crate::streaming::find_ffmpeg()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dshow_devices_old_format() {
        // Old FFmpeg format (versions < 7.0)
        let output = r#"
[dshow @ 0x1234] DirectShow video devices
[dshow @ 0x1234]  "Integrated Camera"
[dshow @ 0x1234]  "USB2.0 HD UVC WebCam"
[dshow @ 0x1234] DirectShow audio devices
[dshow @ 0x1234]  "Microphone"
"#;

        let devices = WindowsDetector::parse_dshow_devices(output);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0], "Integrated Camera");
        assert_eq!(devices[1], "USB2.0 HD UVC WebCam");
    }

    #[test]
    fn test_parse_dshow_devices_new_format() {
        // New FFmpeg format (versions >= 7.0)
        let output = r#"
ffmpeg version 8.1-essentials_build-www.gyan.dev Copyright (c) 2000-2026 the FFmpeg developers
[in#0 @ 000001ecd3346b80] "MEE USB Camera" (video)
[in#0 @ 000001ecd3346b80]   Alternative name "@device_pnp_\\?\usb#vid_1bcf&pid_2283&mi_00#8&23137d8f&0&0000#{65e8773d-8f56-11d0-a3b9-00a0c9223196}\global"
[in#0 @ 000001ecd3346b80] "Microphone (MEE USB Camera Audio)" (audio)
[in#0 @ 000001ecd3346b80]   Alternative name "@device_cm_{33D9A762-90C8-11D0-BD43-00A0C911CE86}\wave_{FA29A4EB-A1C8-47F8-A4A4-1AC3BD1B3736}"
"#;

        let devices = WindowsDetector::parse_dshow_devices(output);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0], "MEE USB Camera");
    }

    #[test]
    fn test_parse_dshow_devices_empty() {
        let output = r#"
ffmpeg version 8.1
[in#0 @ 0x1234] Alternative name "..."
"#;

        let devices = WindowsDetector::parse_dshow_devices(output);
        assert_eq!(devices.len(), 0);
    }
}
