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
//! Local HTTP server.
//!
//! Serves a `/health` endpoint (consumed by the Docker HEALTHCHECK) and the
//! locally-written HLS output so that a user on the same machine can preview
//! a camera without going through Command Center.
//!
//! Security model: no session is required when `bind` is the
//! `127.0.0.1` default ([`ServerConfig::default`]) — only processes on
//! the same host can reach it.  Whenever `bind` is `0.0.0.0` (Local
//! mode, always — or Connected mode with `--lan-streaming`), a valid
//! session cookie is required for `/hls/*` and `/api/*` (other than
//! `/api/auth/login`/`/api/auth/logout`) — see `server::auth` for the
//! guard and session design, and `super::api`'s module doc for the
//! full threat model.
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
        // Last-resort guard only. `Node::new` (node/runner.rs) already
        // resolved this port at boot and every displayed URL was built
        // from its answer, so normally this probe agrees and changes
        // nothing. If it DOESN'T, something grabbed the port in the
        // seconds since — we still bind somewhere usable rather than
        // let warp's internal bind panic take out this task silently
        // (it's spawned detached, so a panic would leave the node
        // running with no dashboard and nothing obvious in the UI),
        // but the operator's status bar is now advertising the wrong
        // port, which is worth an error-level log, not a warning.
        let port = crate::config::find_available_port(self.config.port);
        if port != self.config.port {
            tracing::error!(
                "Port {} was taken between startup and bind — serving on {} \
                 instead. The dashboard URL shown in the status bar is WRONG; \
                 use http://<node-IP>:{} and restart to resync.",
                self.config.port,
                port,
                port,
            );
        }
        let bind_addr = SocketAddr::new(ip, port);
        tracing::info!("Starting HTTP server on {}", bind_addr);

        // Health check endpoint — also used by the Docker HEALTHCHECK.
        // Builds the same concrete ApiReply type as every other route so
        // the whole chain below can `.unify()` into one Reply type.
        let health = warp::path("health")
            .and(warp::get())
            .map(|| build_response(200, None, None, b"OK\n".to_vec()));

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

        // Compose the route chain.  Order matters: `/health` and
        // `/api/auth/login`/`/api/auth/logout` (`auth_routes`) resolve
        // first and are NEVER gated — health for liveness, login
        // because you need to reach it precisely when you don't have a
        // session yet, logout because clearing a cookie shouldn't
        // require one already being valid.  `/hls/*` and the rest of
        // `/api/*` — including `/api/auth/refresh`, which deliberately
        // IS gated, since a valid existing session is exactly what
        // proves you're allowed to refresh it — sit behind the auth
        // guard (see `server::auth`): a request needs a valid session
        // cookie whenever `requires_auth` is true.  The static SPA
        // bundle (Phase C)
        // stays ungated last, so the login page itself always loads.
        // The pre-Phase-B fallback (no api_state) keeps just the typed
        // routes — used by tests and run_quick_setup.
        //
        // `.recover(handle_rejection)` is applied to `guarded` itself,
        // NOT the whole chain — this matters. warp's `.or()` tries the
        // next alternative on ANY rejection, and static_routes' own GET
        // catch-all unconditionally "succeeds" (its own hardcoded 404)
        // for any path starting with api/hls/health — a success, not a
        // rejection, as far as warp is concerned. A `.recover()` at the
        // end of the whole chain never even sees the guard's rejection
        // for a GET request, because static_routes already won by then.
        // Resolving Unauthorized into a concrete reply before it's ever
        // combined with `.or(static_routes)` is what actually fixes it.
        let api_state = self.api_state.clone();
        if let Some(state) = api_state {
            let guard = super::auth::guard(state.requires_auth, state.session_secret);
            let auth_routes = super::api::auth_routes(state.clone());
            let api_routes = super::api::routes(state);
            let static_routes = super::api::static_routes();

            let protected = hls_playlist.or(hls_segment).unify().or(api_routes).unify();
            let guarded = guard.and(protected).recover(handle_rejection).unify();

            let routes = health
                .or(auth_routes)
                .unify()
                .or(guarded)
                .unify()
                .or(static_routes)
                .unify();
            warp::serve(routes).run(bind_addr).await;
        } else {
            let routes = health.or(hls_playlist).or(hls_segment);
            warp::serve(routes).run(bind_addr).await;
        }

        Ok(())
    }
}

/// Convert the auth guard's rejection into a 401 JSON body. Every other
/// rejection (404s from unmatched paths, etc.) passes through
/// unchanged — warp's own default handling still applies to those.
///
/// Returns the same concrete `warp::http::Response<Vec<u8>>` every
/// other route in this file produces (not `impl warp::Reply`) so it
/// unifies with `protected`'s extract type when `.recover()` wraps
/// `guard.and(protected)` — see the call site's comment for why this
/// must wrap `guarded` specifically, not the whole route chain.
async fn handle_rejection(
    err: warp::Rejection,
) -> std::result::Result<warp::http::Response<Vec<u8>>, warp::Rejection> {
    if err.find::<super::auth::Unauthorized>().is_some() {
        Ok(build_response(
            401,
            Some(("Content-Type", "application/json")),
            None,
            br#"{"error":"unauthorized","message":"Login required."}"#.to_vec(),
        ))
    } else {
        Err(err)
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

    /// Regression test for the actual bug found in review: build the
    /// SAME route shape `run()` does — health / auth / guarded-protected
    /// / static — using the real `static_routes()`, and confirm an
    /// unauthenticated GET to a protected path gets 401, not a 404 from
    /// static_routes' own defensive catch-all. Testing `guard()` in
    /// isolation (see server::auth's tests) wasn't enough to catch this:
    /// the bug was specifically in how `guarded` interacts with
    /// `static_routes` via `.or()`, which only shows up when both are
    /// actually composed together like this.
    #[tokio::test]
    async fn unauthenticated_get_to_protected_path_is_401_not_404() {
        use warp::Filter;

        let health = warp::path("health")
            .and(warp::get())
            .map(|| build_response(200, None, None, b"OK\n".to_vec()));
        let fake_protected = warp::path("api")
            .and(warp::path("test"))
            .and(warp::get())
            .map(|| build_response(200, None, None, b"secret".to_vec()));
        let auth_routes = warp::path!("api" / "auth" / "login")
            .map(|| build_response(200, None, None, Vec::new()));
        let static_routes = super::super::api::static_routes();

        let secret = super::super::auth::generate_session_secret();
        let guard = super::super::auth::guard(true, Some(secret));
        let guarded = guard.and(fake_protected).recover(handle_rejection).unify();

        let routes = health
            .or(auth_routes)
            .unify()
            .or(guarded)
            .unify()
            .or(static_routes)
            .unify();

        let resp = warp::test::request()
            .path("/api/test")
            .reply(&routes)
            .await;
        assert_eq!(
            resp.status(),
            401,
            "unauthenticated GET to a protected path must 401, not fall through to \
             static_routes' 404 catch-all"
        );

        // And the SPA shell must still load for an unrelated path with
        // no session at all — the guard must not have swallowed those.
        let resp = warp::test::request().path("/login").reply(&routes).await;
        assert_ne!(resp.status(), 401, "unrelated paths must never be guarded");
    }
}
