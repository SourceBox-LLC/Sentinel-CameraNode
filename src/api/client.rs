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
//! HTTP Client for the Sentinel Command Center API

use reqwest::Client;

use crate::config::NodeMode;
use crate::error::{Error, Result};
use super::types::*;

/// API Client for communicating with Sentinel Command Center.
///
/// In `NodeMode::Connected` (default constructor `new`) every method
/// makes its real HTTP call.  In `NodeMode::Local` (constructor
/// `local_stub`) every CC-bound method short-circuits to a typed error
/// without ever touching the network — `node::runner::run_internal`
/// gates the call sites instead, so the stub is mostly belt-and-
/// suspenders against future call sites being added without a mode
/// check.
#[derive(Clone)]
pub struct ApiClient {
    client: Client,
    base_url: String,
    api_key: String,
    node_id: Option<String>,
    mode: NodeMode,
}

impl ApiClient {
    /// Create a new API client for Connected mode.
    pub fn new(base_url: &str, api_key: &str) -> Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .pool_max_idle_per_host(4)
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| Error::Api(format!("Failed to create HTTP client: {}", e)))?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            node_id: None,
            mode: NodeMode::Connected,
        })
    }

    /// Create a no-op API client for Local mode.
    ///
    /// Every Cloud-bound method on this client returns
    /// `Error::Api("local mode")` without making a network call.  Code
    /// that holds an `ApiClient` (Dashboard's `/wipe` flow, the
    /// uploader, ...) keeps the same shape as Connected mode without
    /// needing `Option<ApiClient>` plumbing throughout.
    pub fn local_stub() -> Result<Self> {
        // We still build a real reqwest client because `Clone` is cheap
        // and the type signatures elsewhere assume one exists.  The
        // empty base_url / api_key make accidental real requests fail
        // loudly even if a code path slips past the mode guard.
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| Error::Api(format!("Failed to create HTTP client: {}", e)))?;
        Ok(Self {
            client,
            base_url: String::new(),
            api_key: String::new(),
            node_id: None,
            mode: NodeMode::Local,
        })
    }

    /// Returns true when this client is a no-op stub for Local mode.
    /// Used by callers that want to early-exit before constructing a
    /// request body.
    pub fn is_local(&self) -> bool {
        self.mode.is_local()
    }

    /// Set the node ID (for heartbeat after registration)
    pub fn set_node_id(&mut self, node_id: String) {
        self.node_id = Some(node_id);
    }

    /// Register this node with the cloud
    ///
    /// Sends detected cameras and receives node ID and authentication secret.
    pub async fn register(
        &mut self,
        node_id: &str,
        name: &str,
        cameras: Vec<CameraInfo>,
        video_codec: Option<&str>,
        audio_codec: Option<&str>,
        http_port: u16,
        lan_streaming: bool,
    ) -> Result<RegisterResponse> {
        tracing::info!("Registering node {} with {}...", node_id, self.base_url);

        let request = RegisterRequest {
            node_id: node_id.to_string(),
            name: name.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            cameras,
            video_codec: video_codec.map(|s| s.to_string()),
            audio_codec: audio_codec.map(|s| s.to_string()),
            http_port,
            lan_streaming,
        };

        let response = self.client
            .post(format!("{}/api/nodes/register", self.base_url))
            .header("X-Node-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!("Registration failed ({}): {}", status, body)));
        }

        let registration: RegisterResponse = response.json().await?;

        self.node_id = Some(registration.node_id.clone());

        tracing::info!("Node registered successfully: {}", registration.node_id);

        Ok(registration)
    }

    /// Send heartbeat to cloud
    ///
    /// Should be called every 30 seconds to indicate node is still alive.
    /// Takes pre-built `CameraStatus` structs so callers can include the
    /// pipeline's `last_error` when FFmpeg is restarting/failed — used to
    /// be `Vec<(String, String)>` which forced a hardcoded "streaming".
    ///
    /// `storage_stats` rides along when the caller has fresh numbers;
    /// `None` is fine on early heartbeats before the first stats
    /// collection has run (the backend just doesn't update its
    /// last-known values, no harm).
    pub async fn heartbeat(
        &self,
        local_ip: Option<&str>,
        lan_streaming: bool,
        camera_statuses: Vec<CameraStatus>,
        storage_stats: Option<crate::storage::StorageStats>,
    ) -> Result<HeartbeatResponse> {
        let node_id = self.node_id.as_ref()
            .ok_or_else(|| Error::Api("Node not registered".into()))?;

        tracing::debug!("Sending heartbeat for node {}", node_id);

        let cameras = if camera_statuses.is_empty() {
            None
        } else {
            Some(camera_statuses)
        };

        let request = HeartbeatRequest {
            node_id: node_id.clone(),
            local_ip: local_ip.map(|s| s.to_string()),
            lan_streaming,
            cameras,
            // Sent on every heartbeat (not just register) so the backend
            // sees in-place CloudNode upgrades without us needing to
            // re-register.  Read from the build at compile time so a
            // tampered runtime config can't claim a fake version.
            version: env!("CARGO_PKG_VERSION").to_string(),
            storage_stats,
        };

        let response = self.client
            .post(format!("{}/api/nodes/heartbeat", self.base_url))
            .header("X-Node-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            // 401/403 on a previously-working heartbeat almost always
            // means the operator rotated/revoked this node's key in
            // Command Center — there is no in-band rotation notice
            // (auth rejects before any response body), so without this
            // hint the node just logs bare 403s every 30s forever.
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(Error::Api(format!(
                    "Heartbeat failed ({}): {} — the node's API key may have \
                     been rotated or revoked in Command Center; re-run setup \
                     with a fresh key to reconnect",
                    status, body
                )));
            }
            return Err(Error::Api(format!("Heartbeat failed ({}): {}", status, body)));
        }

        let heartbeat_response: HeartbeatResponse = response.json().await?;

        tracing::debug!("Heartbeat acknowledged at {}", heartbeat_response.timestamp);

        Ok(heartbeat_response)
    }

    /// Get node ID (after registration)
    pub fn node_id(&self) -> Option<&str> {
        self.node_id.as_deref()
    }

    /// Send heartbeat with retry logic
    pub async fn heartbeat_with_retry(
        &self,
        local_ip: Option<&str>,
        lan_streaming: bool,
        camera_statuses: Vec<CameraStatus>,
        storage_stats: Option<crate::storage::StorageStats>,
        max_retries: u32,
    ) -> Result<HeartbeatResponse> {
        let mut attempts = 0;
        let mut delay = std::time::Duration::from_secs(1);

        loop {
            match self.heartbeat(local_ip, lan_streaming, camera_statuses.clone(), storage_stats.clone()).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    attempts += 1;
                    if attempts >= max_retries {
                        return Err(e);
                    }
                    tracing::warn!("Heartbeat failed (attempt {}/{}): {}. Retrying in {:?}...",
                        attempts, max_retries, e, delay);
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, std::time::Duration::from_secs(30));
                }
            }
        }
    }

    /// Report detected codec for a camera stream
    pub async fn report_codec(
        &self,
        camera_id: &str,
        video_codec: &str,
        audio_codec: &str,
    ) -> Result<()> {
        // Local mode has no Command Center to report to — succeed
        // silently so the caller's "first segment pushed" branch
        // doesn't keep retrying on every segment.
        if self.is_local() {
            return Ok(());
        }
        let _node_id = self
            .node_id
            .as_ref()
            .ok_or_else(|| Error::Api("Node not registered".into()))?;

        tracing::info!(
            "Reporting codec for camera {}: video={}, audio={}",
            camera_id,
            video_codec,
            audio_codec
        );

        let codec_data = serde_json::json!({
            "video_codec": video_codec,
            "audio_codec": audio_codec,
        });

        let response = self
            .client
            .post(format!(
                "{}/api/cameras/{}/codec",
                self.base_url, camera_id
            ))
            .header("X-Node-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&codec_data)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!(
                "Report codec failed ({}): {}",
                status, body
            )));
        }

        tracing::info!("Codec reported for camera {}", camera_id);

        Ok(())
    }

    /// Push an HLS segment to the backend's in-memory cache where the
    /// browser picks it up via the same-origin proxy.
    ///
    /// Returns ``Err(Error::ApiStatus)`` on any non-2xx response so the
    /// caller (``SegmentUploader``) can make retry decisions on the numeric
    /// status rather than string-matching the error message.
    pub async fn push_segment(
        &self,
        camera_id: &str,
        filename: &str,
        data: bytes::Bytes,
    ) -> Result<()> {
        let _node_id = self.node_id.as_ref()
            .ok_or_else(|| Error::Api("Node not registered".into()))?;

        let response = self.client
            .post(format!(
                "{}/api/cameras/{}/push-segment?filename={}",
                self.base_url, camera_id, filename
            ))
            .header("X-Node-API-Key", &self.api_key)
            .header("Content-Type", "video/mp2t")
            .body(data)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::ApiStatus {
                status,
                message: format!("Push segment failed: {}", body),
            });
        }

        Ok(())
    }

    /// Report a motion detection event to the backend via HTTP POST.
    /// This is the reliable delivery path — works even when WebSocket is down.
    pub async fn report_motion(
        &self,
        camera_id: &str,
        score: u32,
        timestamp: &str,
        segment_seq: u64,
    ) -> Result<()> {
        // Local mode: motion events stay on-box.  No CC to notify, so
        // succeed silently rather than logging a failure for every
        // motion-detected segment.
        if self.is_local() {
            return Ok(());
        }
        let _node_id = self.node_id.as_ref()
            .ok_or_else(|| Error::Api("Node not registered".into()))?;

        let body = serde_json::json!({
            "score": score,
            "timestamp": timestamp,
            "segment_seq": segment_seq,
        });

        let response = self.client
            .post(format!(
                "{}/api/cameras/{}/motion",
                self.base_url, camera_id
            ))
            .header("X-Node-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!("Report motion failed ({}): {}", status, body)));
        }

        Ok(())
    }

    /// Decommission this node server-side (node-initiated factory reset).
    ///
    /// Called by `/wipe confirm` in the TUI before the local wipe runs.
    /// The backend identifies us by API key (same auth as heartbeat/push)
    /// and deletes the `CameraNode` row, cascading to cameras and any
    /// in-memory segment cache.
    ///
    /// Deliberately has no retry logic — the caller wants a quick
    /// thumbs-up/thumbs-down so they can either report "backend
    /// unpaired" or "backend unreachable, wiped local only" in the same
    /// TUI frame. Retrying here would just delay the inevitable local
    /// wipe (which the operator already confirmed).
    pub async fn decommission(&self) -> Result<()> {
        // Local mode: nothing to unpair, return Ok so the TUI's
        // `/wipe confirm` flow doesn't surface a misleading "Backend
        // unpair failed" line.  base_url is empty in Local installs
        // and reqwest would otherwise fail with "URL is missing scheme".
        if self.is_local() {
            return Ok(());
        }
        let response = self
            .client
            .post(format!("{}/api/nodes/self/decommission", self.base_url))
            .header("X-Node-API-Key", &self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!(
                "Decommission failed ({}): {}",
                status, body
            )));
        }

        tracing::info!("Node decommissioned on backend");
        Ok(())
    }

    /// Update the HLS playlist on the server
    pub async fn update_playlist(&self, camera_id: &str, playlist_content: &str) -> Result<()> {
        // Local mode: no CC to push to.  Returning Ok keeps the
        // uploader's spawned playlist-push loop quiet — without this
        // every segment write logs "Playlist push failed after 3
        // attempts: API error: Node not registered".
        if self.is_local() {
            return Ok(());
        }
        let _node_id = self.node_id.as_ref()
            .ok_or_else(|| Error::Api("Node not registered".into()))?;

        tracing::debug!("Updating playlist for camera {}", camera_id);

        let response = self.client
            .post(format!("{}/api/cameras/{}/playlist", self.base_url, camera_id))
            .header("X-Node-API-Key", &self.api_key)
            .header("Content-Type", "text/plain")
            .body(playlist_content.to_string())
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Api(format!("Update playlist failed ({}): {}", status, body)));
        }

        tracing::debug!("Playlist updated for camera {}", camera_id);

        Ok(())
    }
}

#[cfg(test)]
mod local_stub_tests {
    //! Local-mode ApiClient stub.  These tests pin two contracts:
    //!  1. `is_local()` answers truthfully so the segment-uploader's
    //!     fast path can short-circuit network IO.
    //!  2. The constructor doesn't require valid creds (Local-mode
    //!     installs have empty api_key / api_url).
    use super::*;

    #[test]
    fn local_stub_reports_local() {
        let client = ApiClient::local_stub().expect("local_stub builds");
        assert!(client.is_local());
    }

    #[test]
    fn new_reports_connected_by_default() {
        let client = ApiClient::new("http://localhost:8000", "key").expect("new builds");
        assert!(!client.is_local());
    }
}