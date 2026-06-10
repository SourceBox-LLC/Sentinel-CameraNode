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
//! macOS camera detection using AVFoundation via FFmpeg
//!
//! Uses FFmpeg to enumerate AVFoundation video devices on macOS.

use std::process::Command;

use super::CameraDetector;
use crate::camera::types::CameraCapabilities;
use crate::camera::DetectedCamera;
use crate::error::Result;

pub struct MacOSDetector;

impl MacOSDetector {
    pub fn new() -> Self {
        Self
    }

    /// Parse FFmpeg AVFoundation device list output
    fn parse_avfoundation_devices(output: &str) -> Vec<(u32, String)> {
        let mut devices = Vec::new();
        let mut in_video_section = false;

        for line in output.lines() {
            // AVFoundation output format:
            // [AVFoundation indev @ 0x...] AVFoundation video devices:
            // [AVFoundation indev @ 0x...]  [0] FaceTime HD Camera
            // [AVFoundation indev @ 0x...]  [1] USB Camera
            // [AVFoundation indev @ 0x...] AVFoundation audio devices:

            if line.contains("AVFoundation video devices") {
                in_video_section = true;
                continue;
            }

            if line.contains("AVFoundation audio devices") {
                in_video_section = false;
                continue;
            }

            if in_video_section {
                // Line format: [AVFoundation indev @ 0x...]  [0] FaceTime HD Camera
                //
                // The line STARTS with FFmpeg's bracketed log prefix —
                // so a naive `line.find('[')` lands on the prefix,
                // yields "AVFoundation indev @ 0x..." as the "index",
                // u32::parse fails, and every device line gets dropped
                // (zero cameras detected; the wizard exits "No cameras
                // found").  Instead, walk the '['s and take the FIRST
                // bracket whose contents parse as a u32 — that's the
                // device index whether or not the log prefix is present;
                // the name is whatever follows its closing ']'.
                for (open, _) in line.match_indices('[') {
                    let after = &line[open + 1..];
                    if let Some(close) = after.find(']') {
                        if let Ok(index) = after[..close].parse::<u32>() {
                            let name = after[close + 1..].trim();
                            if !name.is_empty() {
                                devices.push((index, name.to_string()));
                            }
                            break;
                        }
                    }
                }
            }
        }

        devices
    }
}

impl CameraDetector for MacOSDetector {
    fn detect_cameras(&self) -> Result<Vec<DetectedCamera>> {
        tracing::info!("Scanning for cameras on macOS (AVFoundation)...");

        // Use FFmpeg to list AVFoundation devices
        let output = Command::new("ffmpeg")
            .args([
                "-f",
                "avfoundation",
                "-list_devices",
                "true",
                "-i",
                "", // Empty input for device listing
            ])
            .output()?;

        let stderr = String::from_utf8_lossy(&output.stderr);
        let devices = Self::parse_avfoundation_devices(&stderr);

        if devices.is_empty() {
            tracing::warn!("No cameras found via AVFoundation. FFmpeg output:");
            tracing::warn!("{}", stderr);
        }

        let mut cameras = Vec::new();

        for (index, name) in devices {
            tracing::info!("Detected camera [{}]: {}", index, name);

            cameras.push(DetectedCamera {
                device_path: index.to_string(), // Use numeric index as path
                name,
                capabilities: CameraCapabilities {
                    streaming: true,
                    hardware_encoding: false,
                    formats: vec!["avfoundation".to_string()],
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
        "macOS (AVFoundation)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_avfoundation_devices() {
        let output = r#"
[AVFoundation indev @ 0x1234] AVFoundation video devices:
[AVFoundation indev @ 0x1234]  [0] FaceTime HD Camera
[AVFoundation indev @ 0x1234]  [1] USB Camera
[AVFoundation indev @ 0x1234] AVFoundation audio devices:
[AVFoundation indev @ 0x1234]  [0] MacBook Pro Microphone
"#;

        let devices = MacOSDetector::parse_avfoundation_devices(output);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0], (0, "FaceTime HD Camera".to_string()));
        assert_eq!(devices[1], (1, "USB Camera".to_string()));
    }

    #[test]
    fn test_parse_avfoundation_devices_empty() {
        let output = r#"
[AVFoundation indev @ 0x1234] AVFoundation video devices:
[AVFoundation indev @ 0x1234] AVFoundation audio devices:
"#;

        let devices = MacOSDetector::parse_avfoundation_devices(output);
        assert_eq!(devices.len(), 0);
    }
}
