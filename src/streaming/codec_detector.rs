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
//! Codec Detector - Detects video/audio codecs from MPEG-TS segments using FFprobe
//!
//! Converts FFmpeg codec metadata (profile/level) into HLS-compatible codec strings (RFC 6381)

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

use crate::error::{Error, Result};

/// Detected codec information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodecInfo {
    /// HLS-compatible video codec string (e.g., "avc1.42001E")
    pub video_codec: String,
    /// HLS-compatible audio codec string (e.g., "mp4a.40.2")
    pub audio_codec: String,
}

/// FFprobe stream information
///
/// `coded_width`/`coded_height` are fallbacks for ffprobe builds that omit
/// `width`/`height` on segments with partially malformed SPS — seen in the
/// wild with the Raspberry Pi's `h264_v4l2m2m` encoder, which writes
/// `level_idc=0` and sometimes confuses older ffprobe builds into reporting
/// only one of the two pairs.
#[derive(Debug, Deserialize)]
struct FFprobeStream {
    codec_name: Option<String>,
    profile: Option<String>,
    level: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
    coded_width: Option<i32>,
    coded_height: Option<i32>,
}

/// FFprobe output structure
#[derive(Debug, Deserialize)]
struct FFprobeOutput {
    streams: Vec<FFprobeStream>,
}

/// Convert FFmpeg codec names + profile/level to HLS codec strings (RFC 6381)
///
/// Examples:
/// - H.264 Baseline Level 3.0 → avc1.42e01e (lowercase hex)
/// - H.264 Main Level 4.1 → avc1.4da029
/// - AAC-LC → mp4a.40.2
/// Normalize H.264 level to nearest valid value
///
/// Valid H.264 levels: 10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52
/// FFmpeg sometimes reports non-standard values that need normalization
fn normalize_h264_level(level: i32) -> String {
    let valid_levels = [
        10, 11, 12, 13, 20, 21, 22, 30, 31, 32, 40, 41, 42, 50, 51, 52,
    ];

    let normalized = valid_levels
        .iter()
        .min_by_key(|&&valid| (valid - level).abs())
        .copied()
        .unwrap_or(30);

    format!("{:02x}", normalized)
}

/// Derive the minimum H.264 level required to decode a given resolution.
///
/// Browser MSE decoders reject streams whose NAL units exceed the declared
/// level's capabilities — if we publish `avc1.42e00a` (level 1.0, max
/// 176×144) for a 1080p stream, `MANIFEST_PARSED` never fires and the
/// player shows a spinner forever. This happens in practice because the
/// Raspberry Pi's `h264_v4l2m2m` encoder sometimes writes a `level_idc=0`
/// into the SPS, which ffprobe then reports as `level=0`, which our
/// `normalize_h264_level` rounds to the nearest valid value: 10 (level 1.0).
///
/// The safe fallback is to use the resolution itself as a floor on the
/// declared level. We never downgrade a level ffprobe reports — we only
/// upgrade when the reported level is clearly incompatible with the frame
/// size we're actually shipping. Values chosen from H.264 Annex A Table A-1
/// (max macroblocks per frame per level).
fn derive_level_from_resolution(width: i32, height: i32) -> i32 {
    let pixels = width.max(0).saturating_mul(height.max(0));
    if pixels <= 25_344 {
        10 // ≤ 176×144 (QCIF)
    } else if pixels <= 101_376 {
        12 // ≤ 352×288 (CIF)
    } else if pixels <= 307_200 {
        22 // ≤ 640×480 (VGA)
    } else if pixels <= 414_720 {
        30 // ≤ 720×576 (PAL/NTSC)
    } else if pixels <= 921_600 {
        31 // ≤ 1280×720 (720p)
    } else if pixels <= 2_073_600 {
        40 // ≤ 1920×1080 (1080p)
    } else if pixels <= 3_686_400 {
        50 // ≤ 2560×1440 (1440p)
    } else {
        51 // > 1440p (4K+)
    }
}

fn to_hls_codec_string(
    codec: &str,
    profile: Option<&str>,
    level: Option<i32>,
    width: Option<i32>,
    height: Option<i32>,
) -> String {
    match codec {
        "h264" => {
            let (profile_hex, constraint_byte) = match profile {
                Some("Baseline") | Some("CB") | Some("Constrained Baseline") => ("42", "e0"),
                Some("Main") => ("4d", "a0"),
                Some("High") => ("64", "a0"),
                Some(p) => {
                    tracing::warn!("Unknown H.264 profile '{}', using Baseline", p);
                    ("42", "e0")
                }
                None => ("42", "e0"),
            };

            // Floor the reported level at whatever the resolution demands.
            // If ffprobe reported `Some(0)` (malformed SPS) or a tiny value
            // that can't decode this frame size, bump it up. If the report
            // is missing but we have dimensions, derive from dimensions.
            // If everything's missing, fall back to level 3.0 like before.
            let resolution_level = match (width, height) {
                (Some(w), Some(h)) if w > 0 && h > 0 => Some(derive_level_from_resolution(w, h)),
                _ => None,
            };

            // A reported level below 10 is not a valid H.264 level — it's
            // almost certainly the `h264_v4l2m2m` malformed-SPS case where
            // the Pi's hardware encoder writes `level_idc=0`. Discard it
            // and rely on resolution (or a safe fallback) instead. v0.1.4
            // only rescued us when width/height WERE available; if the
            // same malformed SPS also confused ffprobe into reporting no
            // dimensions, the `(Some(0), None) => 0` arm below passed the
            // garbage through untouched and we shipped `avc1.42e00a` for
            // real 1080p streams — spinner-forever in the browser.
            let sane_level = level.filter(|&l| l >= 10);

            let effective_level = match (sane_level, resolution_level) {
                (Some(l), Some(min)) if l >= min => l,
                (Some(l), Some(min)) => {
                    tracing::warn!(
                        "[Codec] FFprobe reported H.264 level={} but resolution {}x{} requires ≥ {} — using {}",
                        l,
                        width.unwrap_or(0),
                        height.unwrap_or(0),
                        min,
                        min
                    );
                    min
                }
                (Some(l), None) => l,
                (None, Some(min)) => {
                    if level == Some(0) || level.map_or(false, |l| l < 10) {
                        tracing::warn!(
                            "[Codec] FFprobe reported invalid H.264 level={:?} — using resolution-derived level {}",
                            level,
                            min
                        );
                    }
                    min
                }
                (None, None) => {
                    // No usable level AND no resolution. Pick 3.0 — it's
                    // the smallest level that can decode up to 720×576,
                    // which is big enough that we won't hard-fail on
                    // typical webcam output, and small enough that most
                    // decoders accept it without complaint. Better to
                    // understate than to overstate.
                    tracing::warn!(
                        "[Codec] FFprobe returned no usable level or dimensions (reported level={:?}) — falling back to 3.0",
                        level
                    );
                    30
                }
            };

            let level_hex = normalize_h264_level(effective_level);

            format!("avc1.{}{}{}", profile_hex, constraint_byte, level_hex)
        }
        "hevc" | "h265" => {
            let profile_num = match profile {
                Some("Main") => "1",
                Some("Main10") => "2",
                Some(p) => {
                    tracing::warn!("Unknown HEVC profile '{}', using Main", p);
                    "1"
                }
                None => "1",
            };

            // ffprobe reports the raw HEVC level (e.g. 120 for L4.0).
            // The RFC 6381 hvc1 string wants that raw value after "L".
            // The previous expression divided by 30 TWICE (a map(/30)
            // followed by another /30), yielding L0 for every real
            // level — an invalid codec string the moment HEVC ships.
            // Latent today (all encoders are H.264).
            let level_num = level.unwrap_or(90);

            format!("hvc1.{}.L{}.B0", profile_num, level_num)
        }
        "aac" => "mp4a.40.2".to_string(),
        "opus" => "opus".to_string(),
        "mp3" | "mpga" => "mp4a.40.34".to_string(),
        codec_name => {
            tracing::warn!("Unknown codec '{}', using as-is", codec_name);
            codec_name.to_lowercase()
        }
    }
}

/// Detect video and audio codecs from an MPEG-TS segment file using FFprobe
///
/// # Arguments
/// * `segment_path` - Path to the segment file (e.g., "segment_00001.ts")
///
/// # Returns
/// * `CodecInfo` with HLS-compatible codec strings
///
/// # Errors
/// * If FFprobe is not found
/// * If segment file cannot be read
/// * If segment has no video stream
pub fn detect_codec(segment_path: &Path) -> Result<CodecInfo> {
    tracing::info!("[Codec] Detecting codec from segment: {:?}", segment_path);

    // Find FFprobe executable
    let ffprobe_path = find_ffprobe();

    // Run FFprobe to get video stream info
    let segment_path_str = segment_path.to_str().ok_or_else(|| {
        Error::Storage(format!(
            "Segment path contains invalid UTF-8: {:?}",
            segment_path
        ))
    })?;

    let video_output = Command::new(&ffprobe_path)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0", // First video stream
            "-show_entries",
            // `coded_width`/`coded_height` are fallbacks for ffprobe builds
            // that omit `width`/`height` when the SPS is partially garbage
            // (seen on Pi h264_v4l2m2m output with level_idc=0).
            "stream=codec_name,profile,level,width,height,coded_width,coded_height",
            "-of",
            "json",
            segment_path_str,
        ])
        .output()
        .map_err(|e| {
            tracing::error!("[Codec] Failed to run FFprobe: {}", e);
            Error::Storage(format!("FFprobe execution failed: {}", e))
        })?;

    if !video_output.status.success() {
        let stderr = String::from_utf8_lossy(&video_output.stderr);
        tracing::error!("[Codec] FFprobe failed: {}", stderr);
        return Err(Error::Storage(format!("FFprobe failed: {}", stderr)));
    }

    // Parse video stream info
    let video_info: FFprobeOutput =
        serde_json::from_slice(&video_output.stdout).map_err(Error::Serialize)?;

    let video_stream = video_info
        .streams
        .first()
        .ok_or(Error::Storage("No video stream found in segment".into()))?;

    let video_codec_name = video_stream
        .codec_name
        .as_ref()
        .ok_or(Error::Storage("Video stream has no codec_name".into()))?;

    // Prefer `width`/`height`; fall back to `coded_width`/`coded_height`
    // when ffprobe omits the primary pair. Both are dimensions in pixels;
    // coded_* is the pre-crop size written in the SPS, so it can be a few
    // pixels larger than the display size, but for level-derivation purposes
    // (which buckets by total pixel count) they're interchangeable.
    let effective_width = video_stream.width.or(video_stream.coded_width);
    let effective_height = video_stream.height.or(video_stream.coded_height);

    tracing::info!(
        "[Codec] Video stream: codec={}, profile={:?}, level={:?}, size={:?}x{:?} (coded {:?}x{:?})",
        video_codec_name,
        video_stream.profile,
        video_stream.level,
        video_stream.width,
        video_stream.height,
        video_stream.coded_width,
        video_stream.coded_height
    );

    // Convert to HLS codec string
    let video_codec = to_hls_codec_string(
        video_codec_name,
        video_stream.profile.as_deref(),
        video_stream.level,
        effective_width,
        effective_height,
    );

    // Run FFprobe to get audio stream info
    let audio_output = Command::new(&ffprobe_path)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0", // First audio stream
            "-show_entries",
            "stream=codec_name",
            "-of",
            "json",
            segment_path_str,
        ])
        .output()
        .map_err(|e| Error::Storage(format!("FFprobe audio detection failed: {}", e)))?;

    let audio_codec = if audio_output.status.success() {
        let audio_info: FFprobeOutput = serde_json::from_slice(&audio_output.stdout)
            .unwrap_or(FFprobeOutput { streams: vec![] });

        let audio_codec_name = audio_info
            .streams
            .first()
            .and_then(|s| s.codec_name.as_ref())
            .map(|s| s.as_str())
            .unwrap_or("aac");

        tracing::info!("[Codec] Audio stream: codec={}", audio_codec_name);

        to_hls_codec_string(audio_codec_name, None, None, None, None)
    } else {
        // No audio stream - use default AAC-LC
        tracing::warn!("[Codec] No audio stream found, using default AAC-LC");
        "mp4a.40.2".to_string()
    };

    Ok(CodecInfo {
        video_codec,
        audio_codec,
    })
}

/// Find FFprobe — delegates to the shared `streaming::find_ffprobe` so
/// bundled paths and platform-aware fallbacks stay in one place.
fn find_ffprobe() -> String {
    super::find_ffprobe()
}

/// Find FFmpeg — delegates to the shared `streaming::find_ffmpeg`.
fn find_ffmpeg() -> String {
    super::find_ffmpeg()
}

/// Detect codec by capturing a single frame from camera during setup
pub fn detect_codec_from_camera(camera_device: &str) -> Result<CodecInfo> {
    use std::process::Command;

    tracing::info!("[Codec] Detecting codec from camera: {}", camera_device);

    let temp_dir = std::env::temp_dir();
    let temp_file = temp_dir.join("sourcebox_sentry_codec_test.ts");

    let ffmpeg_path = find_ffmpeg();

    // FFmpeg 7+ on Windows requires "video=" prefix for DirectShow devices
    // FFmpeg 6 and earlier accept device name directly
    #[cfg(target_os = "windows")]
    {
        let input_device = format!("video={}", camera_device);
        let mut args = vec!["-f", "dshow", "-i", &input_device];
        args.extend(vec![
            "-frames:v",
            "30",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-f",
            "mpegts",
            temp_file.to_str().ok_or_else(|| {
                Error::Storage("Temp file path contains invalid UTF-8".to_string())
            })?,
        ]);

        let status = Command::new(&ffmpeg_path)
            .args(&args)
            .output()
            .map_err(|e| Error::Storage(format!("FFmpeg execution failed: {}", e)))?;

        if !status.status.success() {
            let stderr = String::from_utf8_lossy(&status.stderr);
            return Err(Error::Storage(format!("FFmpeg failed: {}", stderr)));
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut args = vec!["-f", "v4l2", "-i", camera_device];
        args.extend(vec![
            "-frames:v",
            "30",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-f",
            "mpegts",
            temp_file.to_str().ok_or_else(|| {
                Error::Storage("Temp file path contains invalid UTF-8".to_string())
            })?,
        ]);

        let status = Command::new(&ffmpeg_path)
            .args(&args)
            .output()
            .map_err(|e| Error::Storage(format!("FFmpeg execution failed: {}", e)))?;

        if !status.status.success() {
            let stderr = String::from_utf8_lossy(&status.stderr);
            return Err(Error::Storage(format!("FFmpeg failed: {}", stderr)));
        }
    }

    let codec_info = detect_codec(&temp_file)?;

    if temp_file.exists() {
        let _ = std::fs::remove_file(&temp_file);
    }

    Ok(codec_info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_h264_level_valid() {
        assert_eq!(normalize_h264_level(30), "1e"); // Level 3.0
        assert_eq!(normalize_h264_level(31), "1f"); // Level 3.1
        assert_eq!(normalize_h264_level(41), "29"); // Level 4.1
    }

    #[test]
    fn test_normalize_h264_level_invalid() {
        // Invalid values should round to nearest valid level, then format as hex
        assert_eq!(normalize_h264_level(15), "0d"); // 15 -> level 13 -> hex 0d
        assert_eq!(normalize_h264_level(33), "20"); // 33 -> level 32 -> hex 20
    }

    #[test]
    fn test_h264_baseline_codec_string() {
        assert_eq!(
            to_hls_codec_string("h264", Some("Baseline"), Some(30), None, None),
            "avc1.42e01e"
        );
    }

    #[test]
    fn test_h264_main_codec_string() {
        assert_eq!(
            to_hls_codec_string("h264", Some("Main"), Some(41), None, None),
            "avc1.4da029"
        );
    }

    #[test]
    fn test_h264_high_codec_string() {
        assert_eq!(
            to_hls_codec_string("h264", Some("High"), Some(51), None, None),
            "avc1.64a033"
        );
    }

    #[test]
    fn test_aac_codec_string() {
        assert_eq!(
            to_hls_codec_string("aac", None, None, None, None),
            "mp4a.40.2"
        );
    }

    #[test]
    fn test_unknown_codec_string() {
        assert_eq!(to_hls_codec_string("vp9", None, None, None, None), "vp9");
    }

    #[test]
    fn test_derive_level_from_resolution_1080p() {
        assert_eq!(derive_level_from_resolution(1920, 1080), 40);
    }

    #[test]
    fn test_derive_level_from_resolution_720p() {
        assert_eq!(derive_level_from_resolution(1280, 720), 31);
    }

    #[test]
    fn test_derive_level_from_resolution_4k() {
        assert_eq!(derive_level_from_resolution(3840, 2160), 51);
    }

    #[test]
    fn test_derive_level_from_resolution_vga() {
        assert_eq!(derive_level_from_resolution(640, 480), 22);
    }

    #[test]
    fn test_derive_level_from_resolution_zero() {
        // Malformed input shouldn't panic — should return smallest level
        assert_eq!(derive_level_from_resolution(0, 0), 10);
        assert_eq!(derive_level_from_resolution(-1, -1), 10);
    }

    /// Regression: v0.1.3 shipped with avc1.42e00a (level 1.0) for 1080p
    /// streams because `h264_v4l2m2m` on the Raspberry Pi reports
    /// `level=0` in the SPS, and we rounded 0 → 10 (level 1.0, max
    /// 176×144). Browser MSE rejects the stream; spinner never clears.
    #[test]
    fn test_h264_level_zero_with_1080p_resolution_upgrades_to_level_4() {
        assert_eq!(
            to_hls_codec_string("h264", Some("Baseline"), Some(0), Some(1920), Some(1080)),
            "avc1.42e028" // Baseline level 4.0
        );
    }

    #[test]
    fn test_h264_level_zero_with_720p_resolution_upgrades_to_level_31() {
        assert_eq!(
            to_hls_codec_string("h264", Some("Main"), Some(0), Some(1280), Some(720)),
            "avc1.4da01f" // Main level 3.1
        );
    }

    #[test]
    fn test_h264_valid_level_above_resolution_floor_is_preserved() {
        // If ffprobe reports a valid level >= resolution floor, keep it.
        // 720p floor is 31; ffprobe says 41 → keep 41.
        assert_eq!(
            to_hls_codec_string("h264", Some("Main"), Some(41), Some(1280), Some(720)),
            "avc1.4da029"
        );
    }

    #[test]
    fn test_h264_no_level_uses_resolution_floor() {
        assert_eq!(
            to_hls_codec_string("h264", Some("Baseline"), None, Some(1920), Some(1080)),
            "avc1.42e028"
        );
    }

    #[test]
    fn test_h264_no_level_no_resolution_falls_back_to_level_3() {
        assert_eq!(
            to_hls_codec_string("h264", Some("Baseline"), None, None, None),
            "avc1.42e01e" // Level 3.0 — old behavior preserved
        );
    }

    /// Regression #2: v0.1.4 shipped a partial fix that only upgraded the
    /// level when ffprobe reported BOTH `level=0` AND valid dimensions.
    /// On some Pi segments the same malformed SPS confused ffprobe into
    /// reporting `level=0` AND no dimensions — the `(Some(0), None) => 0`
    /// arm passed the garbage through and we still shipped `avc1.42e00a`
    /// for real 1080p streams. This test locks in that a `level=0` report
    /// with no dimensions falls back to safe level 3.0 instead of
    /// rounding 0 → 10.
    #[test]
    fn test_h264_level_zero_no_resolution_falls_back_to_safe_level() {
        // Level 0 is invalid — must NOT be normalized to level 10 (1.0).
        // With no resolution to floor against, fall back to 3.0.
        assert_eq!(
            to_hls_codec_string("h264", Some("Baseline"), Some(0), None, None),
            "avc1.42e01e" // Level 3.0, NOT avc1.42e00a
        );
    }

    #[test]
    fn test_h264_sub_valid_level_no_resolution_falls_back_to_safe_level() {
        // Any level < 10 is garbage (valid H.264 levels start at 10).
        // Must not be rounded by normalize_h264_level.
        assert_eq!(
            to_hls_codec_string("h264", Some("Main"), Some(5), None, None),
            "avc1.4da01e" // Level 3.0
        );
    }
}
