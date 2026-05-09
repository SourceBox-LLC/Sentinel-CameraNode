// SourceBox Sentry CloudNode - Camera streaming node for SourceBox Sentry Cloud
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
//! Local HTTP server.
//!
//! Serves a `/health` endpoint (consumed by the Docker HEALTHCHECK) and the
//! locally-written HLS output so that a user on the same machine can preview
//! a camera without going through Command Center.
//!
//! Security model: the server has **no authentication**.  It binds to
//! `127.0.0.1` by default ([`ServerConfig::default`]) so only processes on
//! the same host can reach it.  Changing `bind` to `0.0.0.0` exposes live
//! video to anyone on the LAN — don't do that unless you mean to.
//!
//! Recordings and snapshots used to live on disk and had `/recordings/*`
//! and `/snapshots/*` routes here; they moved into the encrypted SQLite DB
//! a while back, and the routes were serving empty directories.  They were
//! removed to match reality and to shrink the attack surface.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use warp::Filter;

use crate::config::ServerConfig;
use crate::error::Result;

pub struct HttpServer {
    config: ServerConfig,
    hls_cameras: HashMap<String, PathBuf>,
    api_state: Option<super::api::LocalApiState>,
}

impl HttpServer {
    /// Create HTTP server with HLS camera map.  No `/api/*` routes —
    /// kept for callers that haven't been migrated to the Phase B
    /// `LocalApiState` plumbing yet.  Phase D will retire this.
    pub fn new_with_hls(config: ServerConfig, hls_cameras: HashMap<String, PathBuf>) -> Self {
        Self {
            config,
            hls_cameras,
            api_state: None,
        }
    }

    /// Create HTTP server with HLS map + Phase B `/api/*` routes.
    /// `api_state` carries the dashboard handle, DB, recording-state
    /// set, mode, and HLS base dir.  See `super::api::routes` for the
    /// full endpoint list and threat model.
    pub fn new_with_api(
        config: ServerConfig,
        hls_cameras: HashMap<String, PathBuf>,
        api_state: super::api::LocalApiState,
    ) -> Self {
        Self {
            config,
            hls_cameras,
            api_state: Some(api_state),
        }
    }

    /// Start the HTTP server.
    pub async fn run(self) -> Result<()> {
        let ip: IpAddr = self.config.bind.parse().unwrap_or_else(|_| {
            tracing::warn!(
                "Invalid server.bind {:?}, falling back to 127.0.0.1",
                self.config.bind
            );
            IpAddr::from([127, 0, 0, 1])
        });
        let bind_addr = SocketAddr::new(ip, self.config.port);
        tracing::info!("Starting HTTP server on {}", bind_addr);

        // Health check endpoint — also used by the Docker HEALTHCHECK.
        let health = warp::path("health").and(warp::get()).map(|| "OK\n");

        // HLS stream endpoints — only serve files we own, only with a
        // strict filename shape (`segment_<digits>.ts` or `stream.m3u8`).
        let hls_cameras = Arc::new(self.hls_cameras.clone());

        // GET /hls/{camera_id}/stream.m3u8
        let hls_cameras_playlist = hls_cameras.clone();
        let hls_playlist = warp::path!("hls" / String / "stream.m3u8")
            .and(warp::get())
            .map(move |camera_id: String| {
                let cameras = hls_cameras_playlist.clone();
                match cameras.get(&camera_id) {
                    Some(hls_dir) => {
                        let playlist_path = hls_dir.join("stream.m3u8");
                        match std::fs::read(&playlist_path) {
                            Ok(content) => build_response(
                                200,
                                Some(("Content-Type", "application/vnd.apple.mpegurl")),
                                Some(("Cache-Control", "no-cache")),
                                content,
                            ),
                            Err(e) => {
                                tracing::error!("Failed to read playlist for {}: {}", camera_id, e);
                                build_response(404, None, None, Vec::new())
                            }
                        }
                    }
                    None => build_response(404, None, None, Vec::new()),
                }
            });

        // GET /hls/{camera_id}/segment_{n}.ts
        let hls_cameras_segment = hls_cameras;
        let hls_segment = warp::path!("hls" / String / String)
            .and(warp::get())
            .map(move |camera_id: String, filename: String| {
                let cameras = hls_cameras_segment.clone();

                // Strict shape: segment_<digits>.ts.  Rejects any traversal
                // attempts (`..`, `/`, encoded slashes) because none of those
                // characters belong in this filename anyway.
                if !is_valid_segment_filename(&filename) {
                    return build_response(400, None, None, Vec::new());
                }

                match cameras.get(&camera_id) {
                    Some(hls_dir) => {
                        let segment_path = hls_dir.join(&filename);
                        match std::fs::read(&segment_path) {
                            Ok(content) => build_response(
                                200,
                                Some(("Content-Type", "video/mp2t")),
                                Some(("Cache-Control", "public, max-age=3600")),
                                content,
                            ),
                            Err(e) => {
                                tracing::debug!("Segment not found {}: {}", filename, e);
                                build_response(404, None, None, Vec::new())
                            }
                        }
                    }
                    None => build_response(404, None, None, Vec::new()),
                }
            });

        // Compose the route chain.  Order matters: typed handlers for
        // `/health`, `/hls/*`, and `/api/*` resolve first, then the
        // static SPA bundle (Phase C) catches everything else and
        // also serves the SPA-fallback for client-side routes.  The
        // pre-Phase-B fallback (no api_state) keeps just the typed
        // routes — used by tests and run_quick_setup.
        let api_state = self.api_state.clone();
        if let Some(state) = api_state {
            let api_routes = super::api::routes(state);
            let static_routes = super::api::static_routes();
            let routes = health
                .or(hls_playlist)
                .or(hls_segment)
                .or(api_routes)
                .or(static_routes);
            warp::serve(routes).run(bind_addr).await;
        } else {
            let routes = health.or(hls_playlist).or(hls_segment);
            warp::serve(routes).run(bind_addr).await;
        }

        Ok(())
    }
}

/// `segment_<digits>.ts` — nothing else.  No `..`, no `/`, no encoded bytes.
fn is_valid_segment_filename(filename: &str) -> bool {
    let Some(middle) = filename
        .strip_prefix("segment_")
        .and_then(|s| s.strip_suffix(".ts"))
    else {
        return false;
    };
    !middle.is_empty() && middle.bytes().all(|b| b.is_ascii_digit())
}

/// Build HTTP response without panicking.
///
/// We control all inputs (status codes and headers), so this should never
/// fail.  In the unlikely event it does, return an empty 500.
fn build_response(
    status: u16,
    header1: Option<(&str, &str)>,
    header2: Option<(&str, &str)>,
    body: Vec<u8>,
) -> warp::http::Response<Vec<u8>> {
    let mut builder = warp::http::Response::builder().status(status);

    if let Some((name, value)) = header1 {
        builder = builder.header(name, value);
    }

    if let Some((name, value)) = header2 {
        builder = builder.header(name, value);
    }

    builder.body(body).unwrap_or_else(|e| {
        tracing::error!("Failed to build HTTP response: {}", e);
        warp::http::Response::builder()
            .status(500)
            .body(Vec::new())
            .expect("Fallback response builder should never fail")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_segment_filename_accepts_well_formed() {
        assert!(is_valid_segment_filename("segment_0.ts"));
        assert!(is_valid_segment_filename("segment_00042.ts"));
        assert!(is_valid_segment_filename("segment_99999999.ts"));
    }

    #[test]
    fn valid_segment_filename_rejects_traversal_and_junk() {
        // Traversal attempts
        assert!(!is_valid_segment_filename("segment_../etc/passwd.ts"));
        assert!(!is_valid_segment_filename("segment_..%2Fpasswd.ts"));
        assert!(!is_valid_segment_filename("../segment_1.ts"));
        assert!(!is_valid_segment_filename("segment_1/../stream.m3u8"));

        // Wrong prefix / suffix
        assert!(!is_valid_segment_filename("segmen_1.ts"));
        assert!(!is_valid_segment_filename("segment_1.mp4"));
        assert!(!is_valid_segment_filename("stream.m3u8"));

        // Non-digit body
        assert!(!is_valid_segment_filename("segment_abc.ts"));
        assert!(!is_valid_segment_filename("segment_1a.ts"));
        assert!(!is_valid_segment_filename("segment_.ts"));

        // Empty-ish
        assert!(!is_valid_segment_filename(""));
        assert!(!is_valid_segment_filename("segment_.ts"));
    }
}
