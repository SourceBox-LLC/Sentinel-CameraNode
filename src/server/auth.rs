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
//! Local-admin auth for LAN-exposed installs.
//!
//! Applies whenever `server.bind != 127.0.0.1` — Local mode (always
//! LAN-bound) or Connected mode with `--lan-streaming`. Both paths make
//! setting a password mandatory (see `setup::tui` and
//! `setup::run_quick_setup`), so in steady state `requires_auth == true`
//! implies a password/secret are always configured; the guard below
//! still fails closed defensively if that invariant is ever violated.
//!
//! ## Session design
//!
//! Sessions are a stateless, HMAC-signed cookie — not a JWT library (a
//! single expiry claim doesn't need one) and not a server-side session
//! table (this is a single-admin appliance; a restart forcing a fresh
//! login would be an unnecessary UX regression given sessions are meant
//! to survive a wall-mounted display staying open for weeks). A cookie
//! (not a bearer token the frontend attaches manually) is what makes
//! this cover `/hls/*` too: the `<video>`/hls.js element's requests for
//! the live stream never pass through the SPA's own fetch wrapper, but
//! the browser attaches cookies to same-origin requests automatically
//! regardless of how they're issued.
//!
//! Token shape: `base64url(json{exp}) + "." + base64url(hmac_sha256(secret, payload))`.
//! `secret` is generated once (`generate_session_secret`) alongside the
//! password and persisted encrypted (see `config::AuthConfig`) — unlike
//! the one-way password hash, its disclosure lets an attacker forge a
//! session outright.

use base64::prelude::*;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use warp::{Filter, Rejection};

use crate::error::{Error, Result};

/// Cookie name carrying the signed session token.
pub const SESSION_COOKIE_NAME: &str = "sentinel_session";

/// Mirrors Command Center's own local-auth rationale: a security-camera
/// dashboard is plausibly left open unattended, so a session should
/// outlast any realistic browsing session rather than forcing repeat
/// logins.
const SESSION_LIFETIME_SECS: i64 = 30 * 24 * 3600;

type HmacSha256 = Hmac<Sha256>;

/// Hash a plaintext password for storage. Argon2 with the crate's
/// default parameters (not tuned further — this gates a single LAN
/// admin login, not a high-throughput auth server).
pub fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;

    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| Error::Server(format!("password hash error: {e}")))
}

/// Verify a plaintext password against a stored argon2 hash. Returns
/// `false` (never an error) for a malformed stored hash — that should
/// be unreachable given `hash_password` is the only writer, but a
/// corrupt config row must fail closed, not panic or 500.
pub fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;

    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// Generate a fresh 32-byte session-signing secret.
pub fn generate_session_secret() -> [u8; 32] {
    use rand::RngCore;
    let mut secret = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut secret);
    secret
}

/// Encode a session secret for storage in the encrypted config KV
/// (which stores strings, not raw bytes).
pub fn encode_secret(secret: &[u8; 32]) -> String {
    BASE64_STANDARD.encode(secret)
}

/// Decode a session secret previously written by [`encode_secret`].
/// `None` on any malformed/wrong-length value — a corrupt secret must
/// fail closed (every existing session stops verifying) rather than
/// panic.
pub fn decode_secret(encoded: &str) -> Option<[u8; 32]> {
    let bytes = BASE64_STANDARD.decode(encoded).ok()?;
    bytes.try_into().ok()
}

#[derive(Serialize, Deserialize)]
struct SessionPayload {
    /// Unix timestamp (seconds) after which the token is rejected.
    exp: i64,
}

/// Issue a new signed session token for the given secret.
pub fn issue_session_token(secret: &[u8; 32]) -> String {
    let exp = chrono::Utc::now().timestamp() + SESSION_LIFETIME_SECS;
    let payload = serde_json::to_vec(&SessionPayload { exp }).expect("serialize session payload");
    let payload_b64 = BASE64_URL_SAFE_NO_PAD.encode(payload);
    let sig = sign(secret, payload_b64.as_bytes());
    format!("{payload_b64}.{}", BASE64_URL_SAFE_NO_PAD.encode(sig))
}

/// Verify a session token against the given secret: checks the HMAC
/// signature (constant-time, via `hmac::Mac::verify_slice`) and the
/// expiry claim. Any malformed input — wrong shape, bad base64, bad
/// JSON, wrong/absent secret — returns `false`, never panics.
pub fn verify_session_token(token: &str, secret: &[u8; 32]) -> bool {
    let Some((payload_b64, sig_b64)) = token.split_once('.') else {
        return false;
    };
    let Ok(sig_bytes) = BASE64_URL_SAFE_NO_PAD.decode(sig_b64) else {
        return false;
    };
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    mac.update(payload_b64.as_bytes());
    if mac.verify_slice(&sig_bytes).is_err() {
        return false;
    }

    let Ok(payload_bytes) = BASE64_URL_SAFE_NO_PAD.decode(payload_b64) else {
        return false;
    };
    let Ok(payload) = serde_json::from_slice::<SessionPayload>(&payload_bytes) else {
        return false;
    };
    payload.exp > chrono::Utc::now().timestamp()
}

fn sign(secret: &[u8; 32], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(secret).expect("32-byte key is always valid for HMAC-SHA256");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Rejection used when the guard denies a request — recovered into a
/// 401 JSON response by the caller (see `server::http`).
#[derive(Debug)]
pub struct Unauthorized;
impl warp::reject::Reject for Unauthorized {}

/// Build the warp guard filter. When `requires_auth` is `false` (the
/// server is loopback-only), every request passes through untouched —
/// this is the Connected-mode-without-`--lan-streaming` / plain
/// same-host-Local-testing case, where the existing "only same-host
/// processes can reach it" threat model already applies.
///
/// When `requires_auth` is `true`, a request must carry a valid session
/// cookie. `session_secret` being `None` here (which the mandatory
/// password prompt should make unreachable in practice) fails closed —
/// every request is rejected rather than silently let through.
pub fn guard(
    requires_auth: bool,
    session_secret: Option<[u8; 32]>,
) -> impl Filter<Extract = (), Error = Rejection> + Clone {
    warp::cookie::optional(SESSION_COOKIE_NAME)
        .and_then(move |cookie: Option<String>| {
            let ok = !requires_auth
                || match (&cookie, &session_secret) {
                    (Some(token), Some(secret)) => verify_session_token(token, secret),
                    _ => false,
                };
            async move {
                if ok {
                    Ok(())
                } else {
                    Err(warp::reject::custom(Unauthorized))
                }
            }
        })
        .untuple_one()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_round_trip() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
    }

    #[test]
    fn verify_password_rejects_malformed_hash() {
        assert!(!verify_password("anything", "not-a-real-argon2-hash"));
    }

    #[test]
    fn secret_encode_decode_round_trip() {
        let secret = generate_session_secret();
        let encoded = encode_secret(&secret);
        assert_eq!(decode_secret(&encoded), Some(secret));
    }

    #[test]
    fn decode_secret_rejects_garbage() {
        assert_eq!(decode_secret("not base64 at all!!"), None);
        assert_eq!(decode_secret(&BASE64_STANDARD.encode(b"too short")), None);
    }

    #[test]
    fn session_token_round_trip() {
        let secret = generate_session_secret();
        let token = issue_session_token(&secret);
        assert!(verify_session_token(&token, &secret));
    }

    #[test]
    fn session_token_rejects_wrong_secret() {
        let token = issue_session_token(&generate_session_secret());
        assert!(!verify_session_token(&token, &generate_session_secret()));
    }

    #[test]
    fn session_token_rejects_tampered_payload() {
        let secret = generate_session_secret();
        let token = issue_session_token(&secret);
        let (payload, sig) = token.split_once('.').unwrap();
        // Flip the payload but keep the original signature.
        let tampered = format!("{}extra.{}", payload, sig);
        assert!(!verify_session_token(&tampered, &secret));
    }

    #[test]
    fn session_token_rejects_malformed_input() {
        let secret = generate_session_secret();
        assert!(!verify_session_token("not-even-two-parts", &secret));
        assert!(!verify_session_token("", &secret));
        assert!(!verify_session_token(".", &secret));
    }

    // ── Guard filter: actual HTTP behaviour ─────────────────────────

    fn protected_filter(
        requires_auth: bool,
        session_secret: Option<[u8; 32]>,
    ) -> warp::filters::BoxedFilter<(warp::http::Response<Vec<u8>>,)> {
        let ok = warp::any().map(|| {
            warp::http::Response::builder()
                .status(200)
                .body(b"protected ok".to_vec())
                .unwrap()
        });
        guard(requires_auth, session_secret)
            .and(ok)
            .recover(|err: Rejection| async move {
                if err.find::<Unauthorized>().is_some() {
                    Ok(warp::http::Response::builder()
                        .status(401)
                        .body(b"unauthorized".to_vec())
                        .unwrap())
                } else {
                    Err(err)
                }
            })
            .unify()
            .boxed()
    }

    #[tokio::test]
    async fn guard_passes_through_when_auth_not_required() {
        let filter = protected_filter(false, None);
        let resp = warp::test::request().reply(&filter).await;
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn guard_rejects_missing_cookie_when_required() {
        let secret = generate_session_secret();
        let filter = protected_filter(true, Some(secret));
        let resp = warp::test::request().reply(&filter).await;
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn guard_accepts_valid_session_cookie() {
        let secret = generate_session_secret();
        let token = issue_session_token(&secret);
        let filter = protected_filter(true, Some(secret));
        let resp = warp::test::request()
            .header("cookie", format!("{SESSION_COOKIE_NAME}={token}"))
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn guard_rejects_cookie_signed_with_different_secret() {
        let token = issue_session_token(&generate_session_secret());
        let filter = protected_filter(true, Some(generate_session_secret()));
        let resp = warp::test::request()
            .header("cookie", format!("{SESSION_COOKIE_NAME}={token}"))
            .reply(&filter)
            .await;
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn guard_fails_closed_when_secret_missing_despite_requiring_auth() {
        // Defensive case: requires_auth=true with no configured secret
        // should be unreachable in practice (mandatory-password setup),
        // but must never silently allow access if it happens anyway.
        let filter = protected_filter(true, None);
        let resp = warp::test::request().reply(&filter).await;
        assert_eq!(resp.status(), 401);
    }

    #[test]
    fn session_token_rejects_expired() {
        // Directly construct an already-expired payload/signature pair
        // rather than sleeping in a test.
        let secret = generate_session_secret();
        let payload = serde_json::to_vec(&SessionPayload {
            exp: chrono::Utc::now().timestamp() - 1,
        })
        .unwrap();
        let payload_b64 = BASE64_URL_SAFE_NO_PAD.encode(payload);
        let sig = sign(&secret, payload_b64.as_bytes());
        let token = format!("{payload_b64}.{}", BASE64_URL_SAFE_NO_PAD.encode(sig));
        assert!(!verify_session_token(&token, &secret));
    }
}
