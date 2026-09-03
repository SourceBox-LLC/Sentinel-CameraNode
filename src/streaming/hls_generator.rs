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
//! HLS Generator - Generates HLS playlist and segments from video frames
//!
//! Uses FFmpeg to transcode frames into HLS format.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::io::{BufRead, BufReader};

use crate::error::Result;

/// Value passed to FFmpeg's `-hls_flags` option.
///
/// `append_list`      — don't clobber the `.m3u8` on each write (the uploader
///                      polls this file to discover new segments).
///
/// We deliberately *omit* `delete_segments`. On Windows, FFmpeg's own
/// rotation delete calls `DeleteFile` on the .ts as soon as it falls off the
/// `hls_list_size` window, and any other handle on that file — Windows
/// Defender's on-access scan, NTFS's lazy-close of the just-finished write,
/// or a slow transient reader — causes `DeleteFile` to fail with
/// `ERROR_SHARING_VIOLATION`. FFmpeg then logs `failed to delete old
/// segment ...` on *every* rotation, which is both noisy and actually leaks
/// disk if it ever persists.
///
/// Ownership of cleanup lives in `sweep_orphan_segments` (hls_uploader.rs),
/// which runs on a blocking executor every 60s, keeps the `keep_count`
/// segments with the highest sequence numbers, and removes the rest. By the
/// time the sweeper runs the transient handles are long gone, so
/// `std::fs::remove_file` succeeds cleanly on every platform.
///
/// Upper bound on disk use: `sweep_keep_count` segments per camera
/// (default 30+) between sweeps ≈ ~20 MB/camera on a 1s cadence — much
/// smaller than the headroom we need anyway. See
/// `docs/runbooks/video-not-showing.md` for the original motivation.
pub(crate) const HLS_FLAGS_VALUE: &str = "append_list";

/// HLS segment information
#[derive(Debug, Clone)]
pub struct HlsSegment {
    /// Segment filename (e.g., "segment_00001.ts")
    pub filename: String,
    /// Full path to segment file
    pub path: PathBuf,
    /// Segment duration in seconds
    pub duration: f64,
    /// Segment sequence number
    pub sequence: u64,
}

/// HLS generator configuration
#[derive(Debug, Clone)]
pub struct HlsGeneratorConfig {
    /// Output directory for segments
    pub output_dir: PathBuf,
    /// Segment duration in seconds
    pub segment_duration: u32,
    /// Playlist name
    pub playlist_name: String,
    /// Number of segments to keep in playlist
    pub playlist_size: u32,
    /// Video width
    pub width: u32,
    /// Video height
    pub height: u32,
    /// Target bitrate (e.g., "2000k")
    pub bitrate: String,
    /// FPS
    pub fps: u32,
    /// Video encoder override (empty = auto-detect)
    pub encoder: String,
}

impl Default for HlsGeneratorConfig {
    fn default() -> Self {
        Self {
            output_dir: PathBuf::from("./hls_output"),
            segment_duration: 1,
            playlist_name: "stream.m3u8".to_string(),
            playlist_size: 15,
            width: 1280,
            height: 720,
            bitrate: "2500k".to_string(),
            fps: 30,
            encoder: String::new(),
        }
    }
}

impl From<crate::config::HlsConfig> for HlsGeneratorConfig {
    fn from(config: crate::config::HlsConfig) -> Self {
        Self {
            output_dir: PathBuf::from("./data/hls"),
            segment_duration: config.segment_duration,
            playlist_name: "stream.m3u8".to_string(),
            playlist_size: config.playlist_size,
            width: 1280,
            height: 720,
            bitrate: config.bitrate,
            fps: 30,
            encoder: String::new(),
        }
    }
}

/// HLS Generator state
pub struct HlsGenerator {
    config: HlsGeneratorConfig,
    running: Arc<AtomicBool>,
    ffmpeg_process: Option<std::process::Child>,
    /// Handle for the stderr drain thread (kept alive so it isn't dropped)
    _stderr_thread: Option<std::thread::JoinHandle<()>>,
}

impl HlsGenerator {
    /// Create a new HLS generator
    pub fn new(config: HlsGeneratorConfig) -> Result<Self> {
        // Create output directory
        std::fs::create_dir_all(&config.output_dir)?;

        Ok(Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            ffmpeg_process: None,
            _stderr_thread: None,
        })
    }

    /// Find FFmpeg executable — delegates to the shared `streaming::find_ffmpeg`.
    pub fn find_ffmpeg() -> String {
        super::find_ffmpeg()
    }

    /// Detect available hardware encoder by probing FFmpeg.
    /// Returns the encoder name (e.g. "h264_nvenc", "h264_qsv", "h264_amf")
    /// or None if only software encoding is available.
    ///
    /// Each candidate is still round-tripped through `verify_encoder` (encode
    /// + ffprobe-strict parse) so a broken NVENC/QSV/AMF build on an older
    /// driver falls through to libx264 instead of silently producing garbage.
    ///
    /// **`h264_v4l2m2m` is intentionally excluded.**  The Raspberry Pi V4L2
    /// M2M H.264 encoder writes a non-conforming SPS across every Pi
    /// hardware revision we've tested.  The resulting bitstream looks valid
    /// enough to trick a shallow `ffmpeg -f null -` round-trip, *and* can
    /// even slip past a strict 720p ffprobe check on some kernels — but at
    /// real 1080p capture resolution it produces segments that browsers'
    /// MSE parser refuses to decode (and FFmpeg itself errors out on with
    /// "Conversion failed!").  See the long investigation in the v0.1.10 /
    /// v0.1.13 / v0.1.14 commit history; the short version is "testing
    /// this encoder is harder than just not using it".  libx264 at
    /// `-preset ultrafast -tune zerolatency` sustains 1080p30 on a Pi 4
    /// at ~1.5 cores per stream, which is the correct trade.
    pub fn detect_hw_encoder(ffmpeg_path: &str) -> Option<String> {
        // Probe order: NVIDIA NVENC > Intel QSV > AMD AMF.
        // `h264_v4l2m2m` is deliberately NOT here — see function docstring.
        let candidates = [
            "h264_nvenc",   // NVIDIA (GeForce GTX 600+, all RTX)
            "h264_qsv",    // Intel Quick Sync (most Intel CPUs with iGPU)
            "h264_amf",    // AMD AMF (Radeon RX 400+)
        ];

        // Run -encoders once, check all candidates against the output
        let encoder_list = Command::new(ffmpeg_path)
            .args(["-hide_banner", "-encoders"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .ok();

        let available_encoders = match &encoder_list {
            Some(output) => String::from_utf8_lossy(&output.stdout).to_string(),
            None => {
                tracing::warn!("Failed to query FFmpeg encoders");
                return None;
            }
        };

        for encoder in &candidates {
            if !available_encoders.contains(encoder) {
                continue;
            }

            if Self::verify_encoder(ffmpeg_path, encoder) {
                tracing::info!("Hardware encoder verified (encode+decode): {}", encoder);
                return Some(encoder.to_string());
            }
            tracing::warn!(
                "[Encoder] {} listed by FFmpeg but failed round-trip verification — skipping",
                encoder
            );
        }

        tracing::info!("No hardware encoder verified, using software (libx264)");
        None
    }

    /// Round-trip verify that a given encoder produces a stream a browser
    /// (MSE / hls.js) will actually play.
    ///
    /// Pipeline:
    ///   1. Encode ~0.5s of `testsrc` through the candidate at 1280×720.
    ///   2. Require non-trivial output size (> 188 B = 1 TS packet).
    ///   3. Run `ffprobe` on the output and require all four of
    ///      `profile`, `level`, `width`, `height` to parse cleanly.
    ///
    /// **Why not just `ffmpeg -f null -`?**
    ///
    /// FFmpeg's decoder is *extremely* forgiving — it will happily skip
    /// broken SPS/PPS NAL units and still exit 0 after "decoding" garbage.
    /// `h264_v4l2m2m` on Raspberry Pi is the poster child: it produces a
    /// bitstream that `ffmpeg -i x.ts -f null -` accepts with no error,
    /// but browsers' strict MSE parser rejects because the SPS is
    /// malformed (profile/level fields unreadable).  `ffprobe`'s stream
    /// parser uses the same strict path as MSE — if ffprobe can't read
    /// `profile`/`level`/`width`/`height`, the browser can't either.
    ///
    /// **Why 1280×720 instead of 320×240?**
    ///
    /// Some hardware encoders pass at tiny synthetic resolutions and fail
    /// at capture-realistic ones.  720p is the smallest resolution a
    /// modern webcam actually produces in the wild, so we test there.
    ///
    /// The temp file is always cleaned up, on every exit path.
    fn verify_encoder(ffmpeg_path: &str, encoder: &str) -> bool {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Unique filename so concurrent node starts can't stomp on each other.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let test_file = std::env::temp_dir()
            .join(format!("sourcebox_sentry_enc_test_{}_{}.ts", encoder, nanos));
        let test_path = match test_file.to_str() {
            Some(s) => s.to_string(),
            None => return false,
        };

        // Encode pass — 15 frames of testsrc at 30fps → 0.5s of video.
        // 1280×720 matches the smallest capture resolution we'll see in
        // production; some HW encoders pass at 320×240 but fail higher.
        let encode = Command::new(ffmpeg_path)
            .args([
                "-hide_banner",
                "-y",
                "-f", "lavfi",
                "-i", "testsrc=s=1280x720:d=0.5:r=30",
                "-pix_fmt", "yuv420p",
                "-c:v", encoder,
                "-f", "mpegts",
                &test_path,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        let encode_ok = matches!(encode, Ok(s) if s.success());
        if !encode_ok {
            let _ = std::fs::remove_file(&test_file);
            return false;
        }

        // File must be non-trivial — any encoder that produces < 188 bytes
        // (one MPEG-TS packet) is broken regardless of what the exit code said.
        let size_ok = std::fs::metadata(&test_file)
            .map(|m| m.len() >= 188)
            .unwrap_or(false);
        if !size_ok {
            let _ = std::fs::remove_file(&test_file);
            return false;
        }

        // Strict bitstream parse — ffprobe must extract profile + level +
        // width + height from the first video stream.  This is where
        // `h264_v4l2m2m`'s malformed SPS finally gets caught: ffprobe
        // exits non-zero (or prints `N/A` / negative level) when the SPS
        // can't be parsed, which is the same failure mode MSE will hit.
        let ffprobe_path = super::find_ffprobe();
        let probe = Command::new(&ffprobe_path)
            .args([
                "-v", "error",
                "-select_streams", "v:0",
                "-show_entries", "stream=profile,level,width,height",
                "-of", "default=noprint_wrappers=1:nokey=0",
                &test_path,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();

        // Remove the temp file now — we've read what we need.
        let _ = std::fs::remove_file(&test_file);

        let probe_stdout = match probe {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout).into_owned()
            }
            Ok(_) => {
                tracing::warn!(
                    "[Encoder] {} — ffprobe exited non-zero on test output (bitstream unparseable)",
                    encoder
                );
                return false;
            }
            Err(e) => {
                tracing::warn!(
                    "[Encoder] {} — couldn't run ffprobe to verify: {}",
                    encoder, e
                );
                return false;
            }
        };

        Self::probe_output_is_playable(encoder, &probe_stdout)
    }

    /// Parse `ffprobe -show_entries stream=profile,level,width,height`
    /// output and decide whether the stream is playable.  A stream is
    /// playable iff every required field is present AND non-degenerate:
    ///   * `profile`  — non-empty, not `unknown`, not `N/A`
    ///   * `level`    — parses to a positive integer
    ///   * `width`    — parses to a positive integer
    ///   * `height`   — parses to a positive integer
    ///
    /// Any missing or degenerate field logs a warning naming the encoder
    /// and the raw probe output, so operators can see exactly which
    /// encoder produced junk on their hardware.
    ///
    /// Factored out of `verify_encoder` so it's unit-testable without
    /// spinning up ffmpeg.
    fn probe_output_is_playable(encoder: &str, probe_stdout: &str) -> bool {
        let mut profile: Option<String> = None;
        let mut level: Option<i64> = None;
        let mut width: Option<i64> = None;
        let mut height: Option<i64> = None;

        for line in probe_stdout.lines() {
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let k = k.trim();
            let v = v.trim();
            match k {
                "profile" => profile = Some(v.to_string()),
                "level" => level = v.parse().ok(),
                "width" => width = v.parse().ok(),
                "height" => height = v.parse().ok(),
                _ => {}
            }
        }

        let profile_ok = profile
            .as_deref()
            .map(|p| !p.is_empty() && !p.eq_ignore_ascii_case("unknown") && p != "N/A")
            .unwrap_or(false);
        let level_ok = level.map(|l| l > 0).unwrap_or(false);
        let width_ok = width.map(|w| w > 0).unwrap_or(false);
        let height_ok = height.map(|h| h > 0).unwrap_or(false);

        if profile_ok && level_ok && width_ok && height_ok {
            return true;
        }

        tracing::warn!(
            "[Encoder] {} — ffprobe found an unplayable stream \
             (profile={:?}, level={:?}, width={:?}, height={:?}); \
             browsers will not decode this encoder's output",
            encoder, profile, level, width, height
        );
        false
    }

    /// Build encoding arguments based on available hardware.
    /// Hardware encoders use the GPU's dedicated media engine, freeing
    /// the CPU for other work (upload, dashboard, multi-camera).
    fn build_encoding_args(
        hw_encoder: &Option<String>,
        bitrate: &str,
        bufsize_k: u32,
        fps: u32,
        segment_duration: u32,
    ) -> Vec<String> {
        let gop_size = (fps * segment_duration).to_string();
        let bufsize = format!("{}k", bufsize_k);

        match hw_encoder.as_deref() {
            Some("h264_nvenc") => {
                tracing::info!("Using NVIDIA NVENC hardware encoding");
                vec![
                    "-pix_fmt".into(), "yuv420p".into(),
                    "-c:v".into(), "h264_nvenc".into(),
                    "-preset".into(), "p4".into(),         // balanced speed/quality (p1=fastest, p7=best)
                    "-tune".into(), "ll".into(),           // low latency
                    "-profile:v".into(), "main".into(),
                    "-level".into(), "auto".into(),        // let NVENC pick the right level for the resolution
                    "-rc".into(), "cbr".into(),            // constant bitrate — smooth upload sizes
                    "-b:v".into(), bitrate.into(),
                    "-maxrate".into(), bitrate.into(),
                    "-bufsize".into(), bufsize,
                    "-g".into(), gop_size.clone(),
                    "-strict_gop".into(), "1".into(),      // enforce strict GOP — no scene-cut keyframes
                    "-c:a".into(), "aac".into(),
                    "-b:a".into(), "128k".into(),
                ]
            }
            Some("h264_qsv") => {
                tracing::info!("Using Intel Quick Sync hardware encoding");
                vec![
                    "-pix_fmt".into(), "yuv420p".into(),
                    "-c:v".into(), "h264_qsv".into(),
                    "-preset".into(), "fast".into(),
                    "-profile:v".into(), "main".into(),
                    "-level".into(), "auto".into(),
                    "-b:v".into(), bitrate.into(),
                    "-maxrate".into(), bitrate.into(),
                    "-bufsize".into(), bufsize,
                    "-g".into(), gop_size.clone(),
                    "-keyint_min".into(), gop_size,
                    "-c:a".into(), "aac".into(),
                    "-b:a".into(), "128k".into(),
                ]
            }
            Some("h264_amf") => {
                tracing::info!("Using AMD AMF hardware encoding");
                vec![
                    "-pix_fmt".into(), "yuv420p".into(),
                    "-c:v".into(), "h264_amf".into(),
                    "-quality".into(), "balanced".into(),
                    "-profile:v".into(), "main".into(),
                    "-level".into(), "auto".into(),
                    "-rc".into(), "cbr".into(),
                    "-b:v".into(), bitrate.into(),
                    "-maxrate".into(), bitrate.into(),
                    "-bufsize".into(), bufsize,
                    "-g".into(), gop_size.clone(),
                    "-keyint_min".into(), gop_size,
                    "-c:a".into(), "aac".into(),
                    "-b:a".into(), "128k".into(),
                ]
            }
            Some("h264_v4l2m2m") => {
                tracing::info!("Using V4L2 M2M hardware encoding (Raspberry Pi / ARM)");
                vec![
                    "-pix_fmt".into(), "yuv420p".into(),
                    "-c:v".into(), "h264_v4l2m2m".into(),
                    "-b:v".into(), bitrate.into(),
                    "-maxrate".into(), bitrate.into(),
                    "-bufsize".into(), bufsize,
                    "-g".into(), gop_size.clone(),
                    "-keyint_min".into(), gop_size,
                    "-c:a".into(), "aac".into(),
                    "-b:a".into(), "128k".into(),
                ]
            }
            Some(other) => {
                // Unknown hardware encoder — use generic args
                tracing::info!("Using hardware encoder: {}", other);
                vec![
                    "-pix_fmt".into(), "yuv420p".into(),
                    "-c:v".into(), other.into(),
                    "-b:v".into(), bitrate.into(),
                    "-maxrate".into(), bitrate.into(),
                    "-bufsize".into(), bufsize,
                    "-g".into(), gop_size.clone(),
                    "-c:a".into(), "aac".into(),
                    "-b:a".into(), "128k".into(),
                ]
            }
            None => {
                tracing::info!("Using software encoding (libx264)");
                // Unlike NVENC/QSV/AMF, libx264 does NOT accept `-level auto`
                // (x264 errors with "Error parsing option 'level' with value 'auto'"
                // and refuses to open the encoder).  Omitting `-level` entirely
                // lets libx264 auto-compute the correct level from resolution,
                // framerate, and bitrate and embed it in the SPS — which is
                // what hls.js / MSE needs to decode the stream.
                //
                // `-preset ultrafast` (not veryfast) is deliberate for the
                // Pi 4 case: at 1080p30 it runs ~1.5 cores per stream, so
                // two simultaneous cameras fit in the Pi 4's 4-core budget
                // with headroom for the upload/dashboard/WS tasks.  veryfast
                // is ~2-3 cores/stream and would starve the second camera.
                vec![
                    "-pix_fmt".into(), "yuv420p".into(),
                    "-c:v".into(), "libx264".into(),
                    "-profile:v".into(), "main".into(),
                    "-preset".into(), "ultrafast".into(),
                    "-tune".into(), "zerolatency".into(),
                    "-b:v".into(), bitrate.into(),
                    "-maxrate".into(), bitrate.into(),
                    "-bufsize".into(), bufsize,
                    "-g".into(), gop_size.clone(),
                    "-keyint_min".into(), fps.to_string(),
                    "-sc_threshold".into(), "0".into(),
                    "-c:a".into(), "aac".into(),
                    "-b:a".into(), "128k".into(),
                ]
            }
        }
    }

    /// Get platform-specific FFmpeg input arguments
    fn get_platform_input_args(&self, device_path: &str) -> Vec<String> {
        #[cfg(target_os = "linux")]
        {
            // `-input_format mjpeg` is required for any UVC camera that
            // advertises BOTH MJPEG and YUYV (uncompressed YUV 4:2:2).
            //
            // Without it, FFmpeg's V4L2 demuxer picks whichever format
            // the driver enumerates first — and `uvcvideo` typically
            // lists YUYV first. YUYV at 1280×720 @ 30fps is ~530 Mbit/s
            // of raw pixel data, which blows past USB 2.0's 480 Mbit/s
            // ceiling. On the Raspberry Pi 4's shared xhci bus (where
            // all four USB ports plus any hub downstream contend for
            // one ~4 Gbit/s bus) the capture stalls outright: FFmpeg's
            // child process stays alive but produces zero segments,
            // surfacing to the operator as a stuck "detecting…" state
            // in the Command Center with no useful error in the logs.
            //
            // MJPG is motion-JPEG — every frame is a self-contained
            // JPEG at ~3–5 Mbit/s for 720p30. Every real-world UVC
            // webcam we've seen supports it for any resolution above
            // 640×480 because the USB bandwidth math only works with
            // it. FFmpeg decodes MJPG→YUV420 on the CPU at negligible
            // cost (~1% of one core) before handing frames to the
            // encoder, so the overall pipeline is identical from the
            // encoder's perspective.
            //
            // Failure mode if a camera is truly YUYV-only: FFmpeg
            // aborts immediately with "Cannot find a proper format
            // for codec_type 'Video' / codec_id 'Rawvideo'" — a loud,
            // actionable failure vs. the current silent stall. If that
            // ever happens in the field we can enumerate formats via
            // VIDIOC_ENUM_FMT in the detector and make this a per-
            // camera choice; hardcoding MJPG buys us the common case.
            vec![
                "-f".to_string(),
                "v4l2".to_string(),
                "-input_format".to_string(),
                "mjpeg".to_string(),
                "-framerate".to_string(),
                self.config.fps.to_string(),
                "-video_size".to_string(),
                format!("{}x{}", self.config.width, self.config.height),
                "-i".to_string(),
                device_path.to_string(),
            ]
        }

        #[cfg(target_os = "windows")]
        {
            vec![
                "-f".to_string(),
                "dshow".to_string(),
                "-framerate".to_string(),
                self.config.fps.to_string(),
                "-video_size".to_string(),
                format!("{}x{}", self.config.width, self.config.height),
                "-i".to_string(),
                format!("video={}", device_path), // Use camera name
            ]
        }

        #[cfg(target_os = "macos")]
        {
            vec![
                "-f".to_string(),
                "avfoundation".to_string(),
                "-framerate".to_string(),
                self.config.fps.to_string(),
                "-video_size".to_string(),
                format!("{}x{}", self.config.width, self.config.height),
                "-i".to_string(),
                device_path.to_string(), // Use numeric index as string
            ]
        }
    }

    /// Start HLS generation from a video device.
    ///
    /// Returns the encoder name that was actually selected (e.g.
    /// `"h264_v4l2m2m"`, `"libx264"`), so the caller can surface it to
    /// the operator.  Selecting the right encoder is the single biggest
    /// determinant of whether a Pi / NUC / old workstation can stream
    /// two cameras without thermal throttling, and we want that choice
    /// visible without having to `docker logs` the container.
    pub fn start_from_device(&mut self, device_path: &str) -> Result<String> {
        if self.running.load(Ordering::SeqCst) {
            return Err(crate::error::Error::Streaming("Already running".into()));
        }

        // Pre-flight: confirm the device actually exists before we hand
        // it to FFmpeg.  FFmpeg's failure mode on a missing /dev/videoN
        // is a cryptic async death 500ms after spawn — catching it here
        // turns that into an actionable error at the call site, and
        // prevents the `running` flag from being set on a doomed start.
        // Windows/macOS branches of this helper are no-ops since their
        // "device paths" aren't filesystem entries.
        crate::camera::validate_device_available(device_path)?;

        self.running.store(true, Ordering::SeqCst);

        let playlist_path = self.config.output_dir.join(&self.config.playlist_name);

        // Clean up old files
        self.cleanup()?;

        // Calculate bufsize
        let bitrate_val: u32 = self
            .config
            .bitrate
            .trim_end_matches('k')
            .parse()
            .unwrap_or(2000);
        let bufsize = bitrate_val * 2;

        // Get platform-specific input args
        let input_args = self.get_platform_input_args(device_path);

        // Find FFmpeg executable
        let ffmpeg_path = Self::find_ffmpeg();

        // Check if user chose an encoder in setup (stored in config DB)
        let hw_encoder = if self.config.encoder == "libx264" {
            tracing::info!("Using software encoder (configured)");
            None
        } else if !self.config.encoder.is_empty() {
            tracing::info!("Using encoder from config: {}", self.config.encoder);
            Some(self.config.encoder.clone())
        } else {
            // No preference set — auto-detect hardware encoder
            Self::detect_hw_encoder(&ffmpeg_path)
        };

        // Build FFmpeg command
        let mut cmd = Command::new(&ffmpeg_path);
        cmd.args(&input_args);

        // Build encoding args — uses GPU if available, falls back to CPU
        let encoding_args = Self::build_encoding_args(
            &hw_encoder,
            &self.config.bitrate,
            bufsize,
            self.config.fps,
            self.config.segment_duration,
        );
        cmd.args(&encoding_args);

        // HLS output args.  `-hls_flags` value documented on HLS_FLAGS_VALUE.
        cmd.args([
            "-f",
            "hls",
            "-hls_time",
            &self.config.segment_duration.to_string(),
            "-hls_list_size",
            &self.config.playlist_size.to_string(),
            "-hls_flags",
            HLS_FLAGS_VALUE,
            "-hls_segment_type",
            "mpegts",
            "-hls_segment_filename",
            &self
                .config
                .output_dir
                .join("segment_%05d.ts")
                .to_string_lossy(),
            &playlist_path.to_string_lossy(),
        ]);

        // Pipe stderr so we can drain it (prevents pipe deadlock) and
        // capture FFmpeg errors/warnings for diagnostics.  Stdout is unused.
        cmd.stdout(Stdio::null()).stderr(Stdio::piped());

        tracing::info!(
            "Starting FFmpeg for HLS generation on {} ({})",
            device_path,
            std::env::consts::OS
        );

        let mut child = cmd.spawn()?;

        // Spawn a thread to continuously drain FFmpeg's stderr.
        // Without this drain the OS pipe buffer fills up (~64 KB) and FFmpeg
        // blocks on the next write, silently halting segment production.
        let stderr = child.stderr.take();
        let stderr_thread = std::thread::spawn(move || {
            if let Some(stderr) = stderr {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) => {
                            // Log errors/warnings at warn level, suppress noisy progress lines
                            let trimmed = l.trim();
                            if trimmed.is_empty() {
                                continue;
                            }
                            if trimmed.contains("error") || trimmed.contains("Error")
                                || trimmed.contains("fatal") || trimmed.contains("Failed")
                                || trimmed.contains("No such") || trimmed.contains("Could not")
                                || trimmed.contains("Discarded") {
                                tracing::warn!("[FFmpeg] {}", trimmed);
                            } else {
                                tracing::debug!("[FFmpeg] {}", trimmed);
                            }
                        }
                        Err(_) => break,
                    }
                }
                tracing::warn!("[FFmpeg] stderr stream closed — process likely exited");
            }
        });

        self.ffmpeg_process = Some(child);
        self._stderr_thread = Some(stderr_thread);

        // Return the encoder that was actually chosen — hw name if we
        // picked hardware, else the software fallback.
        Ok(hw_encoder.unwrap_or_else(|| "libx264".to_string()))
    }

    /// Start HLS generation from raw frames (for test patterns).
    ///
    /// Returns the encoder name for symmetry with [`start_from_device`];
    /// the test-pattern path always uses `libx264` since there's no real
    /// camera to feed a hardware encoder.
    pub fn start_from_frames(&mut self, width: u32, height: u32, fps: u32) -> Result<String> {
        if self.running.load(Ordering::SeqCst) {
            return Err(crate::error::Error::Streaming("Already running".into()));
        }

        self.running.store(true, Ordering::SeqCst);

        let playlist_path = self.config.output_dir.join(&self.config.playlist_name);

        // Clean up old files
        self.cleanup()?;

        // Find FFmpeg executable
        let ffmpeg_path = Self::find_ffmpeg();

        // Generate test pattern using FFmpeg's testsrc
        let mut cmd = Command::new(&ffmpeg_path);
        cmd.args([
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size={}x{}:rate={}", width, height, fps),
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=86400",
            "-t",
            "86400", // 24 hours max
            "-c:v",
            "libx264",
            "-profile:v",
            "baseline",
            "-level",
            "3.0",
            "-preset",
            "veryfast",
            "-b:v",
            &self.config.bitrate,
            "-c:a",
            "aac",
            "-b:a",
            "128k",
            "-f",
            "hls",
            "-hls_time",
            &self.config.segment_duration.to_string(),
            "-hls_list_size",
            &self.config.playlist_size.to_string(),
            // Same `-hls_flags` value as the device path — see HLS_FLAGS_VALUE.
            "-hls_flags",
            HLS_FLAGS_VALUE,
            "-hls_segment_filename",
            &self
                .config
                .output_dir
                .join("segment_%05d.ts")
                .to_string_lossy(),
            &playlist_path.to_string_lossy(),
        ]);

        cmd.stdout(Stdio::null()).stderr(Stdio::piped());

        tracing::info!("Starting FFmpeg test pattern generator");

        let mut child = cmd.spawn()?;

        // Drain stderr — see start_from_device() for explanation
        let stderr = child.stderr.take();
        let stderr_thread = std::thread::spawn(move || {
            if let Some(stderr) = stderr {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) => tracing::debug!("[FFmpeg-test] {}", l.trim()),
                        Err(_) => break,
                    }
                }
            }
        });

        self.ffmpeg_process = Some(child);
        self._stderr_thread = Some(stderr_thread);

        Ok("libx264".to_string())
    }

    /// Stop HLS generation.
    ///
    /// Sends SIGKILL, then **waits for the child** so the PID is reaped.
    /// Without the `.wait()` call, a zombie process accumulates on Unix
    /// every time the supervisor kills a wedged FFmpeg — and with the
    /// stall-flag watchdog (see `supervisor.rs`) that can happen several
    /// times an hour on a thermally-stressed Pi, eventually exhausting
    /// the PID table.  The wait is bounded because we just sent SIGKILL
    /// and the kernel always delivers it.
    pub fn stop(&mut self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);

        if let Some(mut child) = self.ffmpeg_process.take() {
            tracing::info!("Stopping FFmpeg process");
            child.kill()?;
            // Reap immediately — SIGKILL can't be blocked, so this
            // returns as soon as the kernel runs the death signal.
            let _ = child.wait();
        }

        Ok(())
    }

    /// Check if generator is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst) && self.ffmpeg_process.is_some()
    }

    /// Check if the FFmpeg process has exited. Returns Some(exit_status) if
    /// it has exited, None if still running. Non-blocking (uses try_wait).
    pub fn check_process(&mut self) -> Option<std::process::ExitStatus> {
        if let Some(ref mut child) = self.ffmpeg_process {
            match child.try_wait() {
                Ok(Some(status)) => {
                    tracing::error!("[FFmpeg] Process exited with status: {}", status);
                    self.running.store(false, Ordering::SeqCst);
                    Some(status)
                }
                Ok(None) => None, // still running
                Err(e) => {
                    tracing::error!("[FFmpeg] Failed to check process status: {}", e);
                    None
                }
            }
        } else {
            None
        }
    }

    /// Get the current playlist path
    pub fn playlist_path(&self) -> PathBuf {
        self.config.output_dir.join(&self.config.playlist_name)
    }

    /// List current segments
    pub fn list_segments(&self) -> Result<Vec<HlsSegment>> {
        let mut segments = Vec::new();

        let entries = std::fs::read_dir(&self.config.output_dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();

            if path.extension().map(|e| e == "ts").unwrap_or(false) {
                let filename = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                // Extract sequence number from filename like "segment_00001.ts"
                let sequence = filename
                    .trim_start_matches("segment_")
                    .trim_end_matches(".ts")
                    .parse::<u64>()
                    .unwrap_or(0);

                segments.push(HlsSegment {
                    filename,
                    path,
                    duration: self.config.segment_duration as f64,
                    sequence,
                });
            }
        }

        // Sort by sequence number
        segments.sort_by_key(|s| s.sequence);

        Ok(segments)
    }

    /// Clean up all generated files
    fn cleanup(&self) -> Result<()> {
        let entries = std::fs::read_dir(&self.config.output_dir);
        if entries.is_err() {
            return Ok(());
        }

        for entry in entries? {
            let entry = entry?;
            let path = entry.path();

            // Remove .ts and .m3u8 files
            if path
                .extension()
                .map(|e| e == "ts" || e == "m3u8")
                .unwrap_or(false)
            {
                std::fs::remove_file(path)?;
            }
        }

        Ok(())
    }

    /// Get output directory
    pub fn output_dir(&self) -> &PathBuf {
        &self.config.output_dir
    }
}

impl Drop for HlsGenerator {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hls_config_default() {
        let config = HlsGeneratorConfig::default();
        assert_eq!(config.segment_duration, 1);
        assert_eq!(config.playlist_size, 15);
        assert_eq!(config.playlist_name, "stream.m3u8");
    }

    #[test]
    fn test_hls_generator_create() {
        let config = HlsGeneratorConfig::default();
        let generator = HlsGenerator::new(config);
        assert!(generator.is_ok());
        assert!(!generator.unwrap().is_running());
    }

    /// Round-trip sanity check: libx264 is the always-available safe path,
    /// so verify_encoder must return true for it on any machine that can
    /// run the rest of the test suite.  If this ever fails, something in
    /// the encode/decode command construction is broken — a much worse
    /// regression than the Pi-specific issue this was added to catch.
    ///
    /// Skipped when ``ffmpeg`` isn't on PATH — the pre-flight binary check
    /// is what we'd use in production to decide whether to even attempt
    /// streaming at all, not a test harness concern.
    #[test]
    fn libx264_passes_roundtrip_verification() {
        let ffmpeg_path = HlsGenerator::find_ffmpeg();

        let has_ffmpeg = Command::new(&ffmpeg_path)
            .args(["-hide_banner", "-version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !has_ffmpeg {
            eprintln!("ffmpeg not available — skipping libx264 verification test");
            return;
        }

        // Having ffmpeg does NOT imply having libx264: it's a separate
        // GPL-licensed encoder that several distros (Fedora's default
        // build, among others) ship without. Probing for the binary
        // alone left this test permanently red on those machines,
        // asserting a property the host genuinely couldn't satisfy.
        let has_libx264 = Command::new(&ffmpeg_path)
            .args(["-hide_banner", "-encoders"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("libx264"))
            .unwrap_or(false);

        if !has_libx264 {
            eprintln!("ffmpeg has no libx264 encoder — skipping libx264 verification test");
            return;
        }

        assert!(
            HlsGenerator::verify_encoder(&ffmpeg_path, "libx264"),
            "libx264 should pass round-trip verification when ffmpeg ships that encoder"
        );
    }

    /// Invented encoder name that FFmpeg will refuse to initialize.  Exists
    /// to prove the negative path — a failing encode must NOT return true,
    /// and must clean up its temp file.
    #[test]
    fn nonexistent_encoder_fails_verification() {
        let ffmpeg_path = HlsGenerator::find_ffmpeg();

        let has_ffmpeg = Command::new(&ffmpeg_path)
            .args(["-hide_banner", "-version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !has_ffmpeg {
            return;
        }

        assert!(
            !HlsGenerator::verify_encoder(&ffmpeg_path, "h264_this_codec_does_not_exist"),
            "an unknown encoder must not pass verification"
        );
    }

    // ─── Probe-output parser unit tests ──────────────────────────────
    //
    // These cover the real failure modes we've observed in the wild
    // without needing a working ffmpeg on the test machine.

    /// A clean libx264 encode looks like this — every field populated,
    /// level is a positive integer, profile is a recognizable string.
    #[test]
    fn probe_playable_accepts_well_formed_libx264_output() {
        let stdout = "profile=Constrained Baseline\nlevel=31\nwidth=1280\nheight=720\n";
        assert!(HlsGenerator::probe_output_is_playable("libx264", stdout));
    }

    /// The actual failure signature the Pi v0.1.11 node logged:
    /// `profile=None, level=Some(-99), size=None`.  ffprobe renders this
    /// as `level=-99` and unknown profile/dimensions.  Must reject.
    #[test]
    fn probe_playable_rejects_pi_v4l2m2m_signature() {
        let stdout = "profile=unknown\nlevel=-99\nwidth=N/A\nheight=N/A\n";
        assert!(!HlsGenerator::probe_output_is_playable("h264_v4l2m2m", stdout));
    }

    /// An encoder that produces zero-dimension output is broken even if
    /// the profile/level look OK.  Reject.
    #[test]
    fn probe_playable_rejects_zero_dimensions() {
        let stdout = "profile=High\nlevel=40\nwidth=0\nheight=0\n";
        assert!(!HlsGenerator::probe_output_is_playable("broken_hw", stdout));
    }

    /// ffprobe with a completely unparseable bitstream may emit only a
    /// subset of the requested keys.  Missing fields must count as a
    /// failure, not a success.
    #[test]
    fn probe_playable_rejects_missing_fields() {
        // Only width/height present, no profile/level at all.
        let stdout = "width=1920\nheight=1080\n";
        assert!(!HlsGenerator::probe_output_is_playable("half_broken", stdout));
    }

    /// Empty output (ffprobe found no streams at all) must reject.
    #[test]
    fn probe_playable_rejects_empty_output() {
        assert!(!HlsGenerator::probe_output_is_playable("silent", ""));
    }

    /// Profile case shouldn't matter — ffprobe sometimes outputs
    /// lowercase `unknown`, sometimes uppercase.  Both reject.
    #[test]
    fn probe_playable_rejects_unknown_case_insensitive() {
        let stdout = "profile=UNKNOWN\nlevel=30\nwidth=1280\nheight=720\n";
        assert!(!HlsGenerator::probe_output_is_playable("weird", stdout));
    }

    /// Level 0 is what older ffprobe builds emit for malformed SPS — we
    /// already handle that in the codec detector, and verify_encoder
    /// should treat it the same way: not a valid level.
    #[test]
    fn probe_playable_rejects_level_zero() {
        let stdout = "profile=Main\nlevel=0\nwidth=1280\nheight=720\n";
        assert!(!HlsGenerator::probe_output_is_playable("weird", stdout));
    }

    // ─── build_encoding_args regression tests ────────────────────────
    //
    // Pin the software-encoder arg list so we don't re-introduce the
    // `-level auto` bug (libx264 rejects "auto" and refuses to open,
    // producing "Could not open encoder before EOF" on the Pi in v0.1.14).

    /// Helper — find consecutive `flag, value` pair in the arg list.
    fn find_arg_pair<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.windows(2)
            .find(|w| w[0] == flag)
            .map(|w| w[1].as_str())
    }

    /// The libx264 branch must NOT emit `-level auto`.  x264 errors with
    /// "Error parsing option 'level' with value 'auto'" and refuses to
    /// open the encoder, which surfaces on the Pi 4 as an ffmpeg restart
    /// loop and "detecting…" forever in the dashboard.  Omitting the
    /// `-level` argument entirely is correct: libx264 auto-computes the
    /// level from resolution/bitrate and embeds it in the SPS.
    #[test]
    fn libx264_args_omit_level_flag() {
        let args = HlsGenerator::build_encoding_args(&None, "2500k", 5000, 30, 1);
        assert!(
            find_arg_pair(&args, "-level").is_none(),
            "libx264 branch must not emit `-level <anything>` — it doesn't accept 'auto' \
             and auto-selects the correct level from resolution when omitted.  Got: {:?}",
            args
        );
    }

    /// On the Pi 4, `-preset veryfast` uses ~2-3 cores per 1080p30 stream,
    /// which exhausts the 4-core budget with two cameras.  `ultrafast` is
    /// the documented correct trade — ~1.5 cores/stream with headroom for
    /// upload, dashboard, and WebSocket tasks.
    #[test]
    fn libx264_args_use_ultrafast_preset() {
        let args = HlsGenerator::build_encoding_args(&None, "2500k", 5000, 30, 1);
        assert_eq!(
            find_arg_pair(&args, "-preset"),
            Some("ultrafast"),
            "libx264 preset must be `ultrafast` to fit two 1080p30 streams on a Pi 4.  Got: {:?}",
            args
        );
    }

    /// Hardware encoders (NVENC/QSV/AMF) DO accept `-level auto`; this
    /// test makes sure a future "fix" doesn't over-apply the libx264
    /// change to the hardware branches.
    #[test]
    fn hw_encoder_branches_still_use_level_auto() {
        for enc in ["h264_nvenc", "h264_qsv", "h264_amf"] {
            let args = HlsGenerator::build_encoding_args(
                &Some(enc.to_string()),
                "2500k",
                5000,
                30,
                1,
            );
            assert_eq!(
                find_arg_pair(&args, "-level"),
                Some("auto"),
                "{} branch should keep `-level auto` — this encoder accepts it.  Got: {:?}",
                enc,
                args
            );
        }
    }

    /// libx264 must produce a complete, valid arg list — every piece
    /// downstream ffmpeg needs to actually start encoding.
    #[test]
    fn libx264_args_contain_required_pieces() {
        let args = HlsGenerator::build_encoding_args(&None, "2500k", 5000, 30, 1);
        assert_eq!(find_arg_pair(&args, "-c:v"), Some("libx264"));
        assert_eq!(find_arg_pair(&args, "-pix_fmt"), Some("yuv420p"));
        assert_eq!(find_arg_pair(&args, "-profile:v"), Some("main"));
        assert_eq!(find_arg_pair(&args, "-tune"), Some("zerolatency"));
        assert_eq!(find_arg_pair(&args, "-b:v"), Some("2500k"));
        assert_eq!(find_arg_pair(&args, "-c:a"), Some("aac"));
    }

    /// `-hls_flags` MUST include `append_list` or FFmpeg truncates the
    /// playlist on each write, breaking the uploader's discovery path.
    ///
    /// It MUST NOT include `delete_segments`: ownership of `.ts` cleanup
    /// lives in `sweep_orphan_segments` (hls_uploader.rs). On Windows,
    /// FFmpeg's own rotation-delete races AV scanners / NTFS lazy-close and
    /// fires `failed to delete old segment` warnings on every rotation.
    /// See the doc comment on `HLS_FLAGS_VALUE` for the full history —
    /// pre-v0.1.16 we had `delete_segments` and removed it in v0.1.17 once
    /// the sweeper's retention math matched FFmpeg's `hls_list_size` plus a
    /// generous safety buffer.
    #[test]
    fn hls_flags_append_list_without_delete_segments() {
        assert!(
            HLS_FLAGS_VALUE.contains("append_list"),
            "HLS_FLAGS_VALUE must include `append_list` or the uploader \
             can't watch the playlist for new segments.  Got: {:?}",
            HLS_FLAGS_VALUE
        );
        assert!(
            !HLS_FLAGS_VALUE.contains("delete_segments"),
            "HLS_FLAGS_VALUE must NOT include `delete_segments` — the \
             orphan sweeper owns .ts cleanup.  Got: {:?}",
            HLS_FLAGS_VALUE
        );
    }
}
