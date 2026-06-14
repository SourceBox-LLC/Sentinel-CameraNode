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
//! Segment Uploader - Pushes HLS segments to the backend
//!
//! Reads segment files from disk and pushes them to the Command Center
//! via POST /push-segment. Retries transient errors with exponential backoff.

use std::path::PathBuf;
use std::time::Duration;

use crate::api::ApiClient;
use crate::error::{Error, Result};

/// Upload task
pub struct UploadTask {
    /// Camera ID
    pub camera_id: String,
    /// Segment file path
    pub segment_path: PathBuf,
    /// Segment sequence number
    pub sequence: u64,
}

/// Segment uploader configuration
#[derive(Debug, Clone)]
pub struct UploaderConfig {
    /// Upload retry count
    pub retry_count: u32,
}

impl Default for UploaderConfig {
    fn default() -> Self {
        // Four attempts gives us a ~3.75s total budget — long enough for a
        // Fly.io machine cold-start (which is typically 1-2s) without
        // building a per-camera backlog at the 1s segment cadence.
        Self { retry_count: 4 }
    }
}

/// Segment uploader — pushes segments to the backend's in-memory cache.
pub struct SegmentUploader {
    config: UploaderConfig,
}

impl SegmentUploader {
    /// Create a new segment uploader
    pub fn new(config: UploaderConfig) -> Self {
        Self { config }
    }

    /// Push a segment to the backend. Returns true if pushed, false if skipped (too small).
    ///
    /// In `NodeMode::Local` (`api_client.is_local() == true`) this returns
    /// `Ok(true)` without ever opening a network connection — the
    /// caller's local-recording archive path (which writes the same
    /// segment to encrypted SQLite for later playback) runs whether
    /// or not we'd push to a Command Center.
    pub async fn push_segment(&self, task: UploadTask, api_client: &ApiClient) -> Result<bool> {
        // Local-only fast path: skip the upload, return success so
        // the caller's "uploaded" counter still ticks (operator-facing
        // metric — counts segments successfully handled, not segments
        // that crossed the network).  The local archive is handled
        // separately by HlsUploader::start_with_dashboard.
        if api_client.is_local() {
            return Ok(true);
        }

        tracing::debug!(
            "Pushing segment {} for camera {}",
            task.sequence,
            task.camera_id
        );

        let data = tokio::fs::read(&task.segment_path).await?;

        if data.len() < 188 {
            tracing::warn!(
                "Segment {} too small ({} bytes), skipping",
                task.sequence,
                data.len()
            );
            return Ok(false);
        }

        tracing::debug!("Segment {} size: {} bytes", task.sequence, data.len());

        let filename = format!("segment_{:05}.ts", task.sequence);
        let data_bytes: bytes::Bytes = data.into();
        let mut attempts = 0u32;
        let max_attempts = self.config.retry_count;

        loop {
            match api_client.push_segment(&task.camera_id, &filename, data_bytes.clone()).await {
                Ok(()) => break,
                Err(e) if Self::is_retryable(&e) && attempts < max_attempts => {
                    attempts += 1;
                    let delay_ms = Self::backoff_ms(attempts);
                    tracing::warn!(
                        "Push attempt {}/{} failed ({}), retrying in {}ms...",
                        attempts, max_attempts, e, delay_ms,
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                Err(e) => {
                    // 413 is a CONFIG problem, not a transient: the
                    // operator raised segment_duration/bitrate past
                    // Command Center's per-push cap (2 MiB default),
                    // every segment now bounces, and cloud live view
                    // goes dark while heartbeats stay green.  Explain
                    // once per process instead of leaving a bare 413
                    // mystery in the log stream.
                    if let Error::ApiStatus { status: 413, .. } = &e {
                        use std::sync::atomic::{AtomicBool, Ordering};
                        static EXPLAINED_413: AtomicBool = AtomicBool::new(false);
                        if !EXPLAINED_413.swap(true, Ordering::Relaxed) {
                            tracing::warn!(
                                "Camera {}: Command Center rejected the segment as too large \
                                 (HTTP 413).  Cloud live view will stay dark until segments \
                                 shrink under the per-push cap — lower the segment duration \
                                 and/or bitrate (defaults 1s @ 2500k ≈ 312 KB are safe).",
                                task.camera_id,
                            );
                        }
                    }
                    return Err(e);
                }
            }
        }

        // debug, not info: the DashboardLayer persists INFO+ tracing
        // events to the SQLite log table — at one segment/second/camera
        // forever, this line alone was a continuous INSERT stream on
        // the same DB mutex the archive path needs.  The TUI's
        // record_upload counter already shows per-segment progress.
        tracing::debug!(
            "Pushed segment {} for camera {}",
            task.sequence,
            task.camera_id
        );

        Ok(true)
    }

    /// Backoff schedule: 250 → 500 → 1000 → 2000ms, capped at 2s.
    ///
    /// Long enough to ride out a Fly cold-start but short enough that
    /// four attempts total under 4s — we'd rather drop a segment than
    /// let a per-camera queue grow at 1 segment/second.
    fn backoff_ms(attempt: u32) -> u64 {
        let base = 250u64;
        base.saturating_mul(1u64 << (attempt.saturating_sub(1)).min(3))
            .min(2_000)
    }

    /// Check if an error is retryable (transient network/server error).
    ///
    /// Matches on the numeric HTTP status from ``Error::ApiStatus``.  The
    /// previous implementation did prefix-string matching against the
    /// human-readable error message and silently broke when the producer's
    /// format changed — see the ``ApiStatus`` docstring.
    ///
    /// HTTP 402 (plan-limit-hit) is intentionally absent from the retry
    /// list. The backend returns 402 from ``POST /push-segment`` when a
    /// camera is over the org's plan cap (see Command Center's
    /// ``app.api.hls.push_segment``); retrying just hammers the cloud
    /// with rejections that are guaranteed to fail until either the org
    /// upgrades or the next heartbeat populates ``disabled_cameras`` and
    /// the dashboard skips the upload entirely. Same logic applies to
    /// 401/403/404/422 — all caller-side mistakes that retry can't fix.
    fn is_retryable(err: &Error) -> bool {
        match err {
            Error::ApiStatus { status, .. } => matches!(
                *status,
                // Request timeout / rate limit / 5xx family — all transient.
                408 | 429 | 500 | 502 | 503 | 504,
            ),
            // reqwest transport failures (DNS, TCP reset, TLS handshake,
            // read timeout) — almost always worth one more attempt.
            Error::HttpClient(_) => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uploader_config_default() {
        let config = UploaderConfig::default();
        assert_eq!(config.retry_count, 4);
    }

    #[test]
    fn test_upload_task_fields() {
        let task = UploadTask {
            camera_id: "camera_123".to_string(),
            segment_path: PathBuf::from("/data/hls/camera_123/segment_00042.ts"),
            sequence: 42,
        };
        assert_eq!(task.camera_id, "camera_123");
        assert_eq!(task.sequence, 42);
    }

    // ── Retry classification ────────────────────────────────────────

    #[test]
    fn retryable_for_backend_429() {
        let e = Error::ApiStatus { status: 429, message: "rate limit".into() };
        assert!(SegmentUploader::is_retryable(&e));
    }

    #[test]
    fn retryable_for_all_5xx_and_408() {
        for status in [408u16, 429, 500, 502, 503, 504] {
            let e = Error::ApiStatus { status, message: "x".into() };
            assert!(SegmentUploader::is_retryable(&e), "status {status} should retry");
        }
    }

    #[test]
    fn not_retryable_for_client_errors() {
        // 401/403 mean the node's key is wrong — retrying won't help.
        // 400/404 mean the request is malformed — ditto.
        // 402 means the camera is plan-suspended — retrying hammers the
        //   cloud with rejections; the dashboard will pick up the
        //   suspension on the next heartbeat and skip the upload anyway.
        // 422 means the body validation failed.
        for status in [400u16, 401, 402, 403, 404, 422] {
            let e = Error::ApiStatus { status, message: "x".into() };
            assert!(!SegmentUploader::is_retryable(&e), "status {status} should NOT retry");
        }
    }

    #[test]
    fn plan_limit_hit_402_is_terminal() {
        // Locks in the contract from is_retryable's docstring: a 402
        // (plan_limit_hit) returned by the push-segment endpoint must
        // surface immediately, not get retried. The dashboard's
        // disabled_cameras tracking handles the rest.
        let e = Error::ApiStatus {
            status: 402,
            message: "plan_limit_hit: camera over Free-tier cap".into(),
        };
        assert!(!SegmentUploader::is_retryable(&e));
    }

    #[test]
    fn plain_api_error_not_retryable() {
        // A stringly-typed error (non-HTTP) shouldn't be retried blindly.
        let e = Error::Api("Node not registered".into());
        assert!(!SegmentUploader::is_retryable(&e));
    }

    // ── Backoff schedule ────────────────────────────────────────────

    #[test]
    fn backoff_schedule_doubles_and_caps() {
        assert_eq!(SegmentUploader::backoff_ms(1), 250);
        assert_eq!(SegmentUploader::backoff_ms(2), 500);
        assert_eq!(SegmentUploader::backoff_ms(3), 1_000);
        assert_eq!(SegmentUploader::backoff_ms(4), 2_000);
        // Cap holds even if a future dev bumps the retry count.
        assert_eq!(SegmentUploader::backoff_ms(5), 2_000);
        assert_eq!(SegmentUploader::backoff_ms(99), 2_000);
    }
}
