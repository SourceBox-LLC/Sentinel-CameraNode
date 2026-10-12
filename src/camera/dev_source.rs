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
//! Development video sources: cameras that are FFmpeg inputs, not devices.
//!
//! Only compiled with the `dev-video-source` feature, which release builds
//! never enable. It exists so the whole system (HLS, motion, snapshots,
//! Sentinel AI) can be exercised against chosen footage on a machine with
//! no camera: a porch at night, a parking lot, a delivery, an empty room.
//!
//! ```text
//! SOURCEBOX_SENTRY_DEV_SOURCES="porch=/clips/porch.mp4;lot=udp://127.0.0.1:5001"
//! ```
//!
//! When the variable is set, these replace the USB scan entirely. Each
//! `name=input` pair becomes a camera with device path `devsrc-<name>`, so
//! its Command Center camera id stays short and stable while the input
//! changes. A file input loops forever in real time. A URL input (anything
//! with `://`) is read as a live stream, so a separate FFmpeg process can
//! switch what the "camera" sees mid-run without the camera restarting.

use super::{CameraCapabilities, DetectedCamera};

/// The environment variable listing the sources.
pub const ENV: &str = "SOURCEBOX_SENTRY_DEV_SOURCES";

/// Device paths of development sources start with this.
pub const PREFIX: &str = "devsrc-";

/// One `name=input` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevSource {
    pub name: String,
    pub input: String,
}

/// Parse `name=input;name=input`. Names are limited to letters, digits,
/// `-` and `_`, because they end up in camera ids and URLs.
pub fn parse(spec: &str) -> Result<Vec<DevSource>, String> {
    let mut sources: Vec<DevSource> = Vec::new();
    for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (name, input) = part
            .split_once('=')
            .ok_or_else(|| format!("{ENV}: `{part}` is not name=input"))?;
        let (name, input) = (name.trim(), input.trim());
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "{ENV}: camera name `{name}` may only use letters, digits, - and _"
            ));
        }
        if input.is_empty() {
            return Err(format!("{ENV}: `{name}` has no input"));
        }
        if sources.iter().any(|s| s.name == name) {
            return Err(format!("{ENV}: camera name `{name}` is used twice"));
        }
        sources.push(DevSource {
            name: name.to_string(),
            input: input.to_string(),
        });
    }
    Ok(sources)
}

/// The configured sources, or `None` when the variable is unset or empty
/// (the normal USB scan then runs). A malformed value is an error rather
/// than a silent fall back to real cameras.
pub fn configured() -> Result<Option<Vec<DevSource>>, String> {
    match std::env::var(ENV) {
        Ok(spec) if !spec.trim().is_empty() => parse(&spec).map(Some),
        _ => Ok(None),
    }
}

/// The sources as detected cameras.
pub fn as_cameras(sources: &[DevSource], width: u32, height: u32) -> Vec<DetectedCamera> {
    sources
        .iter()
        .map(|s| DetectedCamera {
            device_path: format!("{PREFIX}{}", s.name),
            name: format!("Dev: {}", s.name),
            capabilities: CameraCapabilities {
                streaming: true,
                hardware_encoding: false,
                formats: vec!["dev-source".to_string()],
            },
            supported_resolutions: vec![(width, height)],
            preferred_resolution: (width, height),
        })
        .collect()
}

/// The FFmpeg input arguments for a development device path, or `None`
/// when the path is not one (a real camera).
pub fn input_args(device_path: &str) -> Option<Vec<String>> {
    let name = device_path.strip_prefix(PREFIX)?;
    let sources = configured().ok().flatten()?;
    let source = sources.into_iter().find(|s| s.name == name)?;
    Some(args_for(&source.input))
}

fn args_for(input: &str) -> Vec<String> {
    let args: &[&str] = if input.contains("://") {
        // A live stream: timestamps come from arrival, so a scene switch
        // upstream doesn't send the HLS muxer backwards in time.
        &["-fflags", "+genpts", "-use_wallclock_as_timestamps", "1"]
    } else {
        // A file: play at real speed, forever.
        &["-re", "-stream_loop", "-1"]
    };
    args.iter()
        .map(|a| a.to_string())
        .chain(["-i".to_string(), input.to_string()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_name_input_pairs() {
        let s = parse(" porch=/clips/porch.mp4 ; lot=udp://127.0.0.1:5001;").unwrap();
        assert_eq!(
            s,
            vec![
                DevSource {
                    name: "porch".into(),
                    input: "/clips/porch.mp4".into()
                },
                DevSource {
                    name: "lot".into(),
                    input: "udp://127.0.0.1:5001".into()
                },
            ]
        );
    }

    #[test]
    fn rejects_bad_specs() {
        assert!(parse("porch").is_err());
        assert!(parse("=x.mp4").is_err());
        assert!(parse("a b=x.mp4").is_err());
        assert!(parse("a/b=x.mp4").is_err());
        assert!(parse("a=").is_err());
        assert!(parse("a=x.mp4;a=y.mp4").is_err());
    }

    #[test]
    fn files_loop_in_real_time_and_urls_stream() {
        assert_eq!(
            args_for("/clips/a.mp4"),
            ["-re", "-stream_loop", "-1", "-i", "/clips/a.mp4"]
        );
        let url = args_for("udp://127.0.0.1:5001");
        assert!(!url.contains(&"-stream_loop".to_string()));
        assert_eq!(url[url.len() - 2..], ["-i", "udp://127.0.0.1:5001"]);
    }

    #[test]
    fn cameras_get_short_stable_device_paths() {
        let cams = as_cameras(&parse("porch=/a/very/long/path.mp4").unwrap(), 1280, 720);
        assert_eq!(cams[0].device_path, "devsrc-porch");
        assert_eq!(cams[0].preferred_resolution, (1280, 720));
    }

    #[test]
    fn a_real_device_path_is_not_a_dev_source() {
        assert_eq!(input_args("/dev/video0"), None);
    }
}
