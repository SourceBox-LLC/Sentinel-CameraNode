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
//! API Request/Response Types

use serde::{Deserialize, Serialize};

/// Camera information sent during registration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraInfo {
    /// Device path (e.g., /dev/video0)
    pub device_path: String,

    /// Camera name
    pub name: String,

    /// Width in pixels
    pub width: u32,

    /// Height in pixels
    pub height: u32,

    /// Supported capabilities
    pub capabilities: Vec<String>,
}

impl From<crate::camera::DetectedCamera> for CameraInfo {
    fn from(cam: crate::camera::DetectedCamera) -> Self {
        Self {
            device_path: cam.device_path,
            name: cam.name,
            width: cam.preferred_resolution.0,
            height: cam.preferred_resolution.1,
            capabilities: vec!["streaming".to_string()],
        }
    }
}

/// Node registration request
#[derive(Debug, Serialize)]
pub struct RegisterRequest {
    /// Node ID (assigned by cloud)
    pub node_id: String,

    /// Node name
    pub name: String,

    /// Node software version.  Wire name is `node_version` to match the
    /// backend's Pydantic schema — Pydantic's default `extra="ignore"`
    /// would silently drop a field called `version`, which is how this
    /// was broken before.
    #[serde(rename = "node_version")]
    pub version: String,

    /// Detected cameras
    pub cameras: Vec<CameraInfo>,

    /// Video codec (detected during setup)
    pub video_codec: Option<String>,

    /// Audio codec (detected during setup)
    pub audio_codec: Option<String>,

    /// Port the local HLS/dashboard HTTP server listens on.  The
    /// backend stores it and builds Home Assistant's LAN-direct
    /// stream URLs from it (it used to assume 8080 for every node).
    pub http_port: u16,

    /// Whether that server is reachable from the LAN (bind is not
    /// loopback).  The backend clears its stored `local_ip` when this
    /// is false so Home Assistant is never handed a stream URL the
    /// node will refuse.  Old backends ignore the field (Pydantic
    /// `extra="ignore"`).
    pub lan_streaming: bool,
}

/// Node registration response
#[derive(Debug, Deserialize)]
pub struct RegisterResponse {
    /// Assigned node ID
    pub node_id: String,

    /// Node secret for subsequent API calls
    #[serde(default)]
    pub node_secret: String,

    /// Status (updated, pending, etc.)
    #[serde(default)]
    pub status: String,

    /// Camera ID mapping (device_path -> camera_id)
    #[serde(default)]
    pub cameras: std::collections::HashMap<String, String>,

    /// Subscription plan of the owning org (e.g. `"free"`, `"pro"`, `"pro_plus"`).
    /// The backend's transitional `"business"` alias is also accepted by the
    /// dashboard's pill renderer — both colour the same.
    ///
    /// **Advisory only — the node must not enforce policy based on this field.**
    /// Any limit the backend cares about (camera counts, retention, upload rate)
    /// lives server-side. We surface the plan in the TUI banner as a status
    /// indicator for the operator; the source of truth is the Command Center.
    /// `None` when the backend doesn't send the field (old backend or pre-auth
    /// response), in which case no badge is rendered.
    #[serde(default)]
    pub plan: Option<String>,

    /// Present when the backend skipped one or more cameras during this
    /// registration because the org's plan is at its camera cap.  The node
    /// renders a yellow warning panel surfacing the detail so the operator
    /// sees immediately which cameras failed to stream (rather than
    /// wondering why their new camera tile never appeared in the dashboard).
    /// `None` in the happy path.
    #[serde(default)]
    pub plan_limit_hit: Option<PlanLimitHit>,
}

/// Backend-reported plan-cap breach during registration.
///
/// The node uses this purely for display — enforcement happened server-side
/// when the affected cameras were left out of the `cameras` mapping. Matches
/// the shape of the backend's `plan_limit_hit` dict in
/// `backend/app/api/nodes.py::register_node`.
#[derive(Debug, Clone, Deserialize)]
pub struct PlanLimitHit {
    /// Human-readable plan name, e.g. `"Free"` / `"Pro"` / `"Pro Plus"`.
    pub plan: String,

    /// Camera cap for the active plan (the reason the skip happened).
    pub max_cameras: u32,

    /// Names of the cameras that were *not* registered this round.
    /// Ordered as the node reported them in the register request.
    #[serde(default)]
    pub skipped: Vec<String>,

    /// Pre-formatted one-line detail string from the backend.  Safe to
    /// display verbatim; the node renders it as the panel caption.
    #[serde(default)]
    pub detail: String,
}

/// Node registration info (stored locally)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegistration {
    /// Node ID
    pub node_id: String,

    /// Node name
    pub name: String,

    /// Node secret
    pub secret: String,

    /// Organization ID
    pub org_id: String,

    /// Registration timestamp
    pub registered_at: i64,
}

/// Heartbeat request
#[derive(Debug, Serialize)]
pub struct HeartbeatRequest {
    /// Node ID
    pub node_id: String,

    /// Local IP address
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_ip: Option<String>,

    /// Whether the local HLS server is LAN-reachable (bind is not
    /// loopback).  Re-sent every heartbeat so a bind change after
    /// re-enrolment propagates without a re-register; the backend
    /// clears its stored `local_ip` when false so Home Assistant
    /// never gets a dead LAN stream URL.  See RegisterRequest.
    pub lan_streaming: bool,

    /// Camera statuses
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cameras: Option<Vec<CameraStatus>>,

    /// CloudNode build version (`env!("CARGO_PKG_VERSION")`).
    ///
    /// The backend uses this to gate too-old nodes (HTTP 426) and to flag
    /// "update available" when we ship a newer release.  Always sent — old
    /// backends that don't know the field just ignore it via Pydantic's
    /// extra-field tolerance.
    ///
    /// Wire name is `node_version` to match the backend schema.  If this
    /// serializes as plain `version`, Pydantic drops it and every node
    /// looks legacy to the gate.
    #[serde(rename = "node_version")]
    pub version: String,

    /// Filesystem-aware storage snapshot.  Backend persists these so
    /// the dashboard can render a per-node usage bar and warn the
    /// operator when the host disk is filling up.  Optional so older
    /// backends that don't know the field tolerate the heartbeat.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_stats: Option<crate::storage::StorageStats>,
}

/// Camera status for heartbeat
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraStatus {
    /// Camera ID
    pub camera_id: String,

    /// Pipeline state: one of `starting`, `streaming`, `restarting`,
    /// `failed`, `error`, `offline`. Replaces the hardcoded "streaming"
    /// that used to make every node look healthy even with a dead
    /// FFmpeg pipeline.
    pub status: String,

    /// Human-readable failure reason for `restarting` / `failed` /
    /// `error` states. `None` when healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Heartbeat response
#[derive(Debug, Deserialize)]
pub struct HeartbeatResponse {
    /// Success
    pub success: bool,

    /// Server timestamp
    pub timestamp: String,

    // NOTE: there is deliberately no key-rotation field here.  The
    // backend invalidates the old key the moment the operator rotates
    // it, so a rotated node's next heartbeat 403s BEFORE any response
    // body could carry a new key — an in-band `key_rotated` flag is
    // unreceivable by construction.  Recovery is "re-run setup with
    // the fresh key" (what CC's rotation modal instructs); the 403
    // error path in client.rs carries that hint.

    /// Newer CloudNode release available (e.g. "0.2.0").
    ///
    /// Set when the backend's `LATEST_NODE_VERSION` is ahead of what we
    /// reported.  CloudNode logs a one-line "update available" warning when
    /// this changes; we deliberately do NOT auto-update because operators
    /// are running this on their own hardware.  `None` means we're current.
    #[serde(default)]
    pub update_available: Option<String>,

    /// Current subscription plan of the owning org (see
    /// `RegisterResponse::plan`).  Heartbeats carry this too so plan
    /// upgrades / downgrades reflect in the node TUI without requiring
    /// a re-register.  `None` on older backends; the node leaves the
    /// badge unchanged in that case.
    #[serde(default)]
    pub plan: Option<String>,

    /// Camera IDs on *this* node that the backend has suspended by the
    /// plan cap (see `backend/app/core/plans.py::enforce_camera_cap`).
    /// The node uses this to (a) mark those camera rows `suspended` in
    /// the TUI and (b) stop pushing segments for them — pushes would
    /// otherwise return 402 on every cycle and flood the log.
    ///
    /// Empty on the happy path.  Missing on older backends (defaults
    /// to empty via serde).  Authoritative within one heartbeat of a
    /// plan change.
    #[serde(default)]
    pub disabled_cameras: Vec<String>,

    /// Per-camera recording state, authoritative.  `{camera_id: bool}`.
    /// Computed server-side from each camera's `continuous_24_7` /
    /// `scheduled_recording` policy + the current wall-clock time, so
    /// the answer is fresh as of THIS heartbeat tick.  CloudNode
    /// reconciles its in-memory `recording_state: HashSet<camera_id>`
    /// to exactly match this map: cameras with `true` get inserted,
    /// cameras with `false` (or omitted) get removed.
    ///
    /// Self-healing: a node that crashes loses its in-memory set, but
    /// the next heartbeat re-asserts the correct state from the
    /// backend's source of truth.  No imperative WebSocket commands
    /// involved; the `start_recording` / `stop_recording` WS arms were
    /// retired in v0.1.43 — see websocket.rs for the removal note.
    ///
    /// Missing on older backends (defaults to empty via serde) — the
    /// node treats "missing field" as "no info, leave state alone"
    /// rather than "all cameras stop recording," so a backend
    /// rollback that drops the field doesn't silently disable the
    /// archive on every node.
    #[serde(default)]
    pub recording_state: Option<std::collections::HashMap<String, bool>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_request_serializes_version_field() {
        // Backend's Pydantic schema declares `node_version` and defaults to
        // extra="ignore", so if we ever drop the #[serde(rename)] this
        // payload would serialize as `version`, Pydantic would silently
        // drop it, and every node would look legacy to the update gate.
        // Pin the exact wire key here.
        let req = HeartbeatRequest {
            node_id: "nd_42".into(),
            local_ip: None,
            lan_streaming: false,
            cameras: None,
            version: "0.1.0".into(),
            storage_stats: None,
        };
        let json: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(json.get("node_version").and_then(|v| v.as_str()), Some("0.1.0"));
        // lan_streaming is ALWAYS serialized (never skipped): the
        // backend clears its stored local_ip on false, so omitting it
        // would freeze stale LAN state server-side.
        assert_eq!(json.get("lan_streaming").and_then(|v| v.as_bool()), Some(false));
        assert!(json.get("version").is_none(), "must serialize as node_version, not version");
        assert_eq!(json.get("node_id").and_then(|v| v.as_str()), Some("nd_42"));
        // Optional storage_stats omitted from wire when None — older
        // backends without the field tolerate the heartbeat.
        assert!(json.get("storage_stats").is_none());
    }

    #[test]
    fn heartbeat_response_parses_with_update_available() {
        let raw = r#"{
            "success": true,
            "timestamp": "2026-04-14T12:00:00",
            "update_available": "0.2.0"
        }"#;
        let parsed: HeartbeatResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.update_available.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn heartbeat_response_parses_without_update_available() {
        // Backwards compat: an old backend that doesn't set the new field
        // must still produce a valid response.  #[serde(default)] makes
        // this work — this test pins the contract.
        let raw = r#"{
            "success": true,
            "timestamp": "2026-04-14T12:00:00"
        }"#;
        let parsed: HeartbeatResponse = serde_json::from_str(raw).unwrap();
        assert!(parsed.success);
        assert!(parsed.update_available.is_none());
    }

    #[test]
    fn register_request_includes_version() {
        // Wire key MUST be `node_version` so the backend's Pydantic schema
        // picks it up.  The historical `version` key was silently dropped
        // by Pydantic's default extra="ignore", so every CloudNode looked
        // legacy at register time and the 426 gate never fired.  Pin the
        // correct name here so the bug can't come back.
        let req = RegisterRequest {
            node_id: "nd_42".into(),
            name: "Test".into(),
            version: "0.1.0".into(),
            cameras: vec![],
            video_codec: None,
            audio_codec: None,
            http_port: 8080,
            lan_streaming: false,
        };
        let json: serde_json::Value = serde_json::to_value(&req).unwrap();
        assert_eq!(json.get("node_version").and_then(|v| v.as_str()), Some("0.1.0"));
        assert!(json.get("version").is_none(), "must serialize as node_version, not version");
        // Both LAN-advertising fields always ride the register wire —
        // the backend stores http_port and gates HA's LAN stream URL
        // on lan_streaming.
        assert_eq!(json.get("http_port").and_then(|v| v.as_u64()), Some(8080));
        assert_eq!(json.get("lan_streaming").and_then(|v| v.as_bool()), Some(false));
    }
}

