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
//! WebSocket client for persistent bidirectional communication with the backend.
//!
//! Connects to `ws(s)://<backend>/ws/node` and sends the node API key + node id
//! as HTTP-upgrade request headers (`X-Node-API-Key` / `X-Node-Id`).  Maintains
//! the connection with auto-reconnect, sending heartbeats over the socket and
//! listening for commands from the backend.
//!
//! The header path replaces the pre-v0.1.65 query-string path
//! (`?api_key=…&node_id=…`).  URLs land in many more log sinks than headers do
//! (uvicorn access log, Fly platform access log, log-shipping pipeline export,
//! browser referer headers if the URL ever escapes the process); even though
//! Sentry already scrubs the query string, defense-in-depth says keep the
//! credential out of the URL entirely.  The backend still accepts the
//! query-string form so a pre-v0.1.65 binary can keep working, but logs a
//! deprecation warning when it sees one.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
};
use serde::{Deserialize, Serialize};
use base64::prelude::*;
use crate::dashboard::Dashboard;
use crate::storage::NodeDatabase;

/// JSON message sent/received over the WebSocket.
#[derive(Debug, Serialize, Deserialize)]
pub struct WsMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Runs the WebSocket client loop with auto-reconnect.
///
/// This function never returns under normal operation — it reconnects
/// with exponential backoff whenever the connection drops.
pub async fn run_ws_client(
    api_url: String,
    api_key: String,
    node_id: String,
    camera_ids: Vec<String>,
    heartbeat_interval: u64,
    dash: Dashboard,
    hls_base_dir: PathBuf,
    db: NodeDatabase,
) {
    let ws_url = build_ws_url(&api_url);
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);

    loop {
        dash.log_info("WebSocket connecting…");

        // Build an upgrade request with the API key + node id as
        // headers.  `IntoClientRequest` fills in the standard WS
        // headers (Host, Sec-WebSocket-Key/Version, Upgrade,
        // Connection) for us so we just inject the auth headers
        // on top.  See the module docstring for why headers > query.
        let request = match build_ws_request(&ws_url, &api_key, &node_id) {
            Ok(r) => r,
            Err(e) => {
                dash.log_warn(format!("WebSocket request build failed: {}", e));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
                continue;
            }
        };

        match connect_async(request).await {
            Ok((ws_stream, _response)) => {
                dash.log_info("WebSocket connected");
                backoff = Duration::from_secs(1); // reset on success

                let (mut write, mut read) = ws_stream.split();

                // Send heartbeats on a timer, read commands from backend
                let mut heartbeat_ticker =
                    tokio::time::interval(Duration::from_secs(heartbeat_interval));

                loop {
                    tokio::select! {
                        // -- Heartbeat tick --
                        _ = heartbeat_ticker.tick() => {
                            let msg = build_heartbeat(&camera_ids, &dash);
                            let text = match serde_json::to_string(&msg) {
                                Ok(t) => t,
                                Err(e) => {
                                    tracing::warn!("Failed to serialize heartbeat: {}", e);
                                    continue;
                                }
                            };
                            if write.send(Message::Text(text)).await.is_err() {
                                dash.log_warn("WebSocket send failed — reconnecting");
                                break; // exit inner loop → reconnect
                            }
                        }

                        // (Pre-v0.1.61 a `Some(event) = motion_rx.recv()` branch
                        // forwarded motion events as `motion_detected` WS frames.
                        // `motion_rx` was never written to — `spawn_motion_detection`
                        // takes `_tx` as unused — so the branch fired never.
                        // Motion events are delivered HTTP-only via
                        // `api_client.report_motion`.  Branch + plumbing removed.)

                        // -- Incoming message from backend --
                        incoming = read.next() => {
                            match incoming {
                                Some(Ok(Message::Text(text))) => {
                                    if let Some(response) = handle_message(
                                        &text, &dash,
                                        &hls_base_dir, &db,
                                    ).await {
                                        let resp_text = serde_json::to_string(&response)
                                            .unwrap_or_default();
                                        if write.send(Message::Text(resp_text)).await.is_err() {
                                            dash.log_warn("WebSocket send failed — reconnecting");
                                            break;
                                        }
                                    }
                                }
                                Some(Ok(Message::Ping(data))) => {
                                    let _ = write.send(Message::Pong(data)).await;
                                }
                                Some(Ok(Message::Close(_))) => {
                                    dash.log_warn("WebSocket closed by server — reconnecting");
                                    break;
                                }
                                Some(Err(e)) => {
                                    dash.log_warn(format!(
                                        "WebSocket error: {} — reconnecting",
                                        redact_api_key(&e.to_string()),
                                    ));
                                    break;
                                }
                                None => {
                                    dash.log_warn("WebSocket stream ended — reconnecting");
                                    break;
                                }
                                _ => {} // Binary, Pong, etc.
                            }
                        }
                    }
                }
            }
            Err(e) => {
                // `tungstenite::Error::Display` can include the request URL
                // (and therefore the api_key query param). Redact before log.
                dash.log_warn(format!(
                    "WebSocket connect failed: {}",
                    redact_api_key(&e.to_string()),
                ));
            }
        }

        // Exponential backoff before reconnect
        dash.log_info(format!("Reconnecting in {}s…", backoff.as_secs()));
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(max_backoff);
    }
}

/// Build the WebSocket URL from the HTTP API URL.  No query string —
/// auth credentials ride in headers via [`build_ws_request`].
fn build_ws_url(api_url: &str) -> String {
    let base = api_url
        .replace("https://", "wss://")
        .replace("http://", "ws://")
        .trim_end_matches('/')
        .to_string();

    format!("{}/ws/node", base)
}

/// Build a WebSocket upgrade `Request` carrying the auth credentials in
/// HTTP request headers.
///
/// The header path replaces the pre-v0.1.65 query-string path — URLs land
/// in too many log sinks (uvicorn access log, Fly platform access log, log
/// shipper exports, browser referer chains if the URL ever escapes the
/// process), and headers don't.  The backend still accepts the old
/// query-string form for back-compat with binaries built before this
/// change, but logs a deprecation warning each time.
fn build_ws_request(
    url: &str,
    api_key: &str,
    node_id: &str,
) -> std::result::Result<tokio_tungstenite::tungstenite::handshake::client::Request, String> {
    // `IntoClientRequest` fills in the standard WebSocket upgrade
    // headers (Host, Sec-WebSocket-Key, Sec-WebSocket-Version, Upgrade,
    // Connection) for us; we only need to add the auth headers on top.
    let mut req = url
        .into_client_request()
        .map_err(|e| format!("invalid WS URL {}: {}", url, e))?;

    let key_value = HeaderValue::from_str(api_key)
        .map_err(|e| format!("api_key contains non-ASCII bytes: {}", e))?;
    let id_value = HeaderValue::from_str(node_id)
        .map_err(|e| format!("node_id contains non-ASCII bytes: {}", e))?;

    let headers = req.headers_mut();
    headers.insert("X-Node-API-Key", key_value);
    headers.insert("X-Node-Id", id_value);

    Ok(req)
}

/// Strip `api_key=<value>` from any string before it hits a log sink.
///
/// Pre-v0.1.65 the WS client put the API key in the URL query string, so
/// tungstenite error messages and dashboard log lines could contain
/// `api_key=…` substrings that leaked the credential into the plaintext
/// SQLite `logs` table.  As of v0.1.65 we put the key in a header instead,
/// so this redaction is mostly defense in depth — but it stays because:
///
///   1. The backend still accepts the old query-string path for back-compat,
///      so an operator hand-typing a debugging URL might still include the
///      `api_key=` form.
///   2. Any third-party code path that happens to format a URL with the
///      legacy shape (config dumps, error context strings) stays safe.
fn redact_api_key(s: &str) -> String {
    const KEY: &str = "api_key=";
    let mut out = String::with_capacity(s.len());
    let mut remainder = s;
    while let Some(idx) = remainder.find(KEY) {
        out.push_str(&remainder[..idx + KEY.len()]);
        let after = &remainder[idx + KEY.len()..];
        // Value ends at the next reasonable delimiter. We're deliberately
        // liberal — any char that wouldn't plausibly appear in a percent-
        // encoded API key value terminates the redaction.
        let end = after
            .find(|c: char| {
                matches!(
                    c,
                    '&' | '#' | ',' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>'
                ) || c.is_whitespace()
            })
            .unwrap_or(after.len());
        out.push_str("[REDACTED]");
        remainder = &after[end..];
    }
    out.push_str(remainder);
    out
}

/// Build a heartbeat message.
///
/// Pulls the real per-camera pipeline state from the dashboard
/// (populated by the FFmpeg supervisor). If an ID hasn't been written
/// to the dashboard yet — which can happen in the short window between
/// registration and the supervisor's first successful start — we fall
/// back to "streaming" so the backend doesn't see a suddenly-empty
/// payload and treat the node as having no cameras.
fn build_heartbeat(camera_ids: &[String], dash: &Dashboard) -> WsMessage {
    let cameras: Vec<serde_json::Value> = camera_ids
        .iter()
        .map(|id| match dash.get_camera_status_by_id(id) {
            Some(s) => {
                let (wire, err) = s.to_wire();
                match err {
                    Some(e) => serde_json::json!({
                        "camera_id": id,
                        "status": wire,
                        "last_error": e,
                    }),
                    None => serde_json::json!({
                        "camera_id": id,
                        "status": wire,
                    }),
                }
            }
            None => serde_json::json!({
                "camera_id": id,
                "status": "streaming",
            }),
        })
        .collect();

    let local_ip = get_local_ip();

    WsMessage {
        msg_type: "heartbeat".to_string(),
        id: None,
        command: None,
        payload: serde_json::json!({
            "cameras": cameras,
            "local_ip": local_ip,
            // Same field as the HTTP heartbeat — backend uses it to gate
            // too-old nodes and to surface "update available" hints.  Read
            // from the build at compile time so it always matches the
            // running binary.
            "node_version": env!("CARGO_PKG_VERSION"),
        }),
    }
}

/// Process the version-compatibility hints the backend can attach to a
/// heartbeat ack and surface them in the dashboard.
///
/// `update_available` fires once per *distinct* version we see (so a long
/// uptime doesn't repeat the same nudge every 30s).  `unsupported` fires
/// at most once per process — once the operator has been told to update
/// they don't need to keep being told until they restart CloudNode.
fn handle_ack_version_hints(payload: &serde_json::Value, dash: &Dashboard) {
    static LAST_HINT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    static UNSUPPORTED_LOGGED: OnceLock<Mutex<bool>> = OnceLock::new();
    let last_hint = LAST_HINT.get_or_init(|| Mutex::new(None));
    let unsupported_logged = UNSUPPORTED_LOGGED.get_or_init(|| Mutex::new(false));

    let update_hint = payload
        .get("update_available")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    if let Ok(mut guard) = last_hint.lock() {
        if *guard != update_hint {
            if let Some(latest) = &update_hint {
                dash.log_warn(format!(
                    "CloudNode update available: {} (running {}). Re-run the installer to update.",
                    latest,
                    env!("CARGO_PKG_VERSION"),
                ));
            }
            *guard = update_hint;
        }
    }

    let unsupported = payload
        .get("unsupported")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if unsupported {
        if let Ok(mut guard) = unsupported_logged.lock() {
            if !*guard {
                dash.log_warn(format!(
                    "Backend reports CloudNode {} is below the minimum supported version. Update to keep streaming.",
                    env!("CARGO_PKG_VERSION"),
                ));
                *guard = true;
            }
        }
    }
}

/// Handle an incoming message from the backend.
/// Returns an optional response to send back over the WebSocket.
async fn handle_message(
    text: &str,
    dash: &Dashboard,
    hls_base_dir: &Path,
    db: &NodeDatabase,
) -> Option<WsMessage> {
    let msg: WsMessage = match serde_json::from_str(text) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("Invalid WS message: {}", e);
            return None;
        }
    };

    match msg.msg_type.as_str() {
        "ack" => {
            // Heartbeat acknowledged — silent on the happy path, but the
            // backend may piggyback version-compat hints in the payload.
            // Both are de-duped at process scope so a long-running node
            // doesn't repeat the warning every ~30s.
            handle_ack_version_hints(&msg.payload, dash);
            None
        }
        "command" => {
            let cmd = msg.command.as_deref().unwrap_or("unknown");
            let msg_id = msg.id.clone();
            dash.log_info(format!("Command received: {}", cmd));

            let result = dispatch_command(
                cmd, &msg.payload, hls_base_dir, db,
            ).await;

            let payload = match &result {
                Ok(data) => serde_json::json!({
                    "status": "success",
                    "data": data,
                }),
                Err(err) => {
                    dash.log_warn(format!("Command '{}' failed: {}", cmd, err));
                    serde_json::json!({
                        "status": "error",
                        "error": err,
                    })
                }
            };

            Some(WsMessage {
                msg_type: "command_result".to_string(),
                id: msg_id,
                command: Some(cmd.to_string()),
                payload,
            })
        }
        "error" => {
            let detail = msg.payload.get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            dash.log_warn(format!("Server error: {}", detail));
            None
        }
        other => {
            tracing::debug!("Unhandled WS message type: {}", other);
            None
        }
    }
}

/// Route a command to the appropriate handler.
async fn dispatch_command(
    cmd: &str,
    payload: &serde_json::Value,
    hls_base_dir: &Path,
    db: &NodeDatabase,
) -> std::result::Result<serde_json::Value, String> {
    match cmd {
        "take_snapshot" => {
            let camera_id = payload.get("camera_id")
                .and_then(|v| v.as_str())
                .ok_or("missing camera_id")?;
            cmd_take_snapshot(camera_id, hls_base_dir, db).await
        }
        // `start_recording` / `stop_recording` WS commands were retired in
        // v0.1.43 when the heartbeat-driven recording_state reconciler
        // replaced them.  Recording state now flows: operator click →
        // Camera.continuous_24_7 in DB → heartbeat response → CloudNode
        // recording_state set.  See `node::runner::start_heartbeat_loop`
        // for the reconciler.  The backend stopped sending these commands
        // in the same release; we keep the unknown-command branch below
        // as the catch-all for any operator running a stale backend.
        "list_snapshots" => {
            let camera_id = payload.get("camera_id").and_then(|v| v.as_str());
            let snaps = db.list_snapshots(camera_id).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(&snaps).unwrap_or_default())
        }
        "list_recordings" => {
            let camera_id = payload.get("camera_id").and_then(|v| v.as_str());
            let recs = db.list_recordings(camera_id).map_err(|e| e.to_string())?;
            Ok(serde_json::to_value(&recs).unwrap_or_default())
        }
        "wipe_data" => {
            db.wipe_all().map_err(|e| e.to_string())?;
            tracing::warn!("All local data wiped by backend command");
            Ok(serde_json::json!({"wiped": true}))
        }
        other => Err(format!("unknown command: {}", other)),
    }
}

// ── Command handlers ─────────────────────────────────────────────────────────

/// WS-side `take_snapshot` adapter.
///
/// Delegates the FFmpeg-grab + DB-save flow to
/// [`crate::api::commands::take_snapshot`] (shared with the local
/// `/api/cameras/{id}/snapshot` HTTP route in Phase B), then re-reads
/// the saved JPEG and base64-encodes it for transport over the WS
/// JSON envelope.  Pre-Phase-B this whole flow lived inline here.
async fn cmd_take_snapshot(
    camera_id: &str,
    hls_base_dir: &Path,
    db: &NodeDatabase,
) -> std::result::Result<serde_json::Value, String> {
    let meta = crate::api::commands::take_snapshot(camera_id, hls_base_dir, db).await?;

    // Re-fetch the JPEG bytes for WS transport.  When the safety floor
    // skipped the DB write, `id == 0` and we have no bytes to return —
    // surface that as a clear error so operator sees a real failure
    // rather than an empty image.
    let jpeg = crate::api::commands::fetch_snapshot_jpeg(db, meta.id).await?;
    let image_b64 = BASE64_STANDARD.encode(&jpeg);

    Ok(serde_json::json!({
        "filename": meta.filename,
        "size_bytes": meta.size_bytes,
        "timestamp": meta.timestamp,
        "image_b64": image_b64,
    }))
}

// ── Helpers ──────────────────────────────────────────────────────────────────
//
// `find_latest_segment` + helpers + their tests moved to
// `crate::api::commands` in Phase B so the local /api/* HTTP routes can
// share the same logic.  Both the WS dispatcher (above) and the HTTP
// snapshot route call `commands::take_snapshot` which uses them.

fn get_local_ip() -> Option<String> {
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    socket.local_addr().ok().map(|a| a.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ws_url_swaps_scheme_and_drops_query_string() {
        // v0.1.65+: no credentials in URL.  Scheme swap + path is all
        // build_ws_url does now.  The api_key / node_id go in headers
        // via build_ws_request below.
        assert_eq!(
            build_ws_url("https://api.example.com"),
            "wss://api.example.com/ws/node",
        );
        assert_eq!(
            build_ws_url("http://localhost:8000"),
            "ws://localhost:8000/ws/node",
        );
    }

    #[test]
    fn build_ws_url_trims_trailing_slash() {
        assert_eq!(
            build_ws_url("http://localhost:8000/"),
            "ws://localhost:8000/ws/node",
        );
    }

    #[test]
    fn build_ws_request_sets_auth_headers() {
        // The whole point of the v0.1.65 change: credentials in
        // headers, not in the URL.  Header lookup uses lower-case
        // canonical names per HTTP spec — `headers().get("x-...")`
        // works regardless of how we inserted them.
        let req = build_ws_request(
            "wss://api.example.com/ws/node",
            "nak_test_key_abc123",
            "abc12345",
        )
        .expect("valid request builds");
        assert_eq!(
            req.headers().get("x-node-api-key").map(|v| v.to_str().unwrap()),
            Some("nak_test_key_abc123"),
        );
        assert_eq!(
            req.headers().get("x-node-id").map(|v| v.to_str().unwrap()),
            Some("abc12345"),
        );
        // Standard WS upgrade headers got auto-filled by
        // `IntoClientRequest` — pin the most important one so a future
        // refactor that swaps the helper away from
        // `IntoClientRequest` doesn't silently lose the spec headers.
        assert_eq!(
            req.headers().get("connection").map(|v| v.to_str().unwrap()),
            Some("Upgrade"),
        );
    }

    #[test]
    fn build_ws_request_rejects_header_injection_in_credentials() {
        // HeaderValue accepts any opaque byte ≥ 0x20 (HTTP/1.1 allows
        // non-ASCII octets in header values), but rejects ASCII control
        // chars including CR/LF — which is the actual security-relevant
        // rejection.  A newline in the api_key would let an attacker
        // inject additional headers via the WS upgrade request.  We
        // generate keys as hex so this is theoretical, but the guardrail
        // matters in case a future code path lets user input flow into
        // either credential.
        let err = build_ws_request("wss://x/ws/node", "key\r\nX-Evil: 1", "node-1")
            .expect_err("CRLF in api_key should reject");
        assert!(err.contains("api_key"), "error should mention api_key: {}", err);

        let err = build_ws_request("wss://x/ws/node", "ok", "node\nX-Evil: 1")
            .expect_err("LF in node_id should reject");
        assert!(err.contains("node_id"), "error should mention node_id: {}", err);
    }

    #[test]
    fn redact_api_key_strips_value_before_ampersand() {
        let s = "connect failed at wss://x/ws/node?api_key=sk_live_abc123&node_id=nd_7";
        assert_eq!(
            redact_api_key(s),
            "connect failed at wss://x/ws/node?api_key=[REDACTED]&node_id=nd_7",
        );
    }

    #[test]
    fn redact_api_key_strips_value_at_end_of_string() {
        let s = "url=wss://x/ws/node?api_key=supersecret";
        assert_eq!(redact_api_key(s), "url=wss://x/ws/node?api_key=[REDACTED]");
    }

    #[test]
    fn redact_api_key_strips_before_fragment_or_whitespace() {
        assert_eq!(
            redact_api_key("api_key=secret#frag"),
            "api_key=[REDACTED]#frag",
        );
        assert_eq!(
            redact_api_key("see api_key=secret for details"),
            "see api_key=[REDACTED] for details",
        );
    }

    #[test]
    fn redact_api_key_handles_multiple_occurrences() {
        let s = "tried api_key=one, then api_key=two";
        assert_eq!(
            redact_api_key(s),
            "tried api_key=[REDACTED], then api_key=[REDACTED]",
        );
    }

    #[test]
    fn redact_api_key_is_noop_when_absent() {
        assert_eq!(redact_api_key(""), "");
        assert_eq!(redact_api_key("nothing secret here"), "nothing secret here");
        assert_eq!(
            redact_api_key("X-Node-API-Key header used"),
            "X-Node-API-Key header used",
        );
    }

    #[test]
    fn redact_api_key_handles_empty_value() {
        // Malformed but shouldn't panic.
        assert_eq!(redact_api_key("api_key=&next=1"), "api_key=[REDACTED]&next=1");
        assert_eq!(redact_api_key("api_key="), "api_key=[REDACTED]");
    }

    // ── Version-reporting tests ───────────────────────────────────

    #[test]
    fn build_heartbeat_includes_node_version() {
        // Backend's wire-protocol gate keys off this field.  If a refactor
        // accidentally drops it from the WS payload, every node looks like
        // an unknown-version legacy node — silently degrades the
        // "update available" UX without breaking anything else.  Test
        // catches that regression.
        let dash = Dashboard::new("nd_test", "http://localhost");
        let msg = build_heartbeat(&[], &dash);
        assert_eq!(msg.msg_type, "heartbeat");
        let version = msg.payload.get("node_version").and_then(|v| v.as_str());
        assert_eq!(version, Some(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn handle_ack_version_hints_tolerates_empty_payload() {
        // Old backends won't set update_available / unsupported.  The hint
        // handler must not panic or log spurious warnings on a bare ack.
        let dash = Dashboard::new("nd_test", "http://localhost");
        handle_ack_version_hints(&serde_json::json!({}), &dash);
        handle_ack_version_hints(&serde_json::json!({"timestamp": "now"}), &dash);
    }

    #[test]
    fn handle_ack_version_hints_accepts_update_payload() {
        // Smoke test that a well-formed hint payload is accepted without
        // panicking.  We can't easily assert the dashboard log content
        // without a fake sink (the dashboard buffers internally), but
        // exercising the path catches type / lock-poisoning regressions.
        let dash = Dashboard::new("nd_test", "http://localhost");
        handle_ack_version_hints(
            &serde_json::json!({"update_available": "9.9.9"}),
            &dash,
        );
        handle_ack_version_hints(
            &serde_json::json!({"unsupported": true}),
            &dash,
        );
    }

    // Segment-selection tests moved to crate::api::commands tests in
    // Phase B alongside the helpers themselves.
}
