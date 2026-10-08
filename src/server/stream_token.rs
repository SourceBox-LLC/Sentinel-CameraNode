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
//! Signed stream tokens: how Home Assistant plays a password-protected
//! node's live video.
//!
//! A node reachable on the LAN requires a session cookie for `/hls/*`
//! (see `server::auth`), which a browser gets by logging in. Home
//! Assistant's stream player can't log in, so it was refused, and the
//! integration's live view could never work. Command Center now hands
//! Home Assistant each camera's local playlist URL with `?st=<token>`:
//!
//! ```text
//! token = <exp> "." hex(HMAC-SHA256(key, "sentinel-hls-v1:" camera_id ":" exp))
//! key   = hex(SHA-256(node API key))   -- as lowercase ASCII
//! ```
//!
//! The key is the node key's SHA-256 fingerprint, which Command Center
//! already stores (`camera_nodes.api_key_hash`) and the node can compute
//! from its own key, so nothing new is shared or stored. A token opens
//! one camera's HLS files (playlist and segments) until `exp`, and
//! nothing else: not `/api/*`, not another camera. Rotating the node key
//! revokes every token at once.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// The query parameter carrying the token.
pub const PARAM: &str = "st";

/// The signing key for a node: its API key's SHA-256, as lowercase hex.
pub fn key_for(api_key: &str) -> String {
    Sha256::digest(api_key.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn message(camera_id: &str, exp: i64) -> String {
    format!("sentinel-hls-v1:{camera_id}:{exp}")
}

/// A token for `camera_id`, valid until `exp` (Unix seconds).
pub fn sign(key: &str, camera_id: &str, exp: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC takes any key size");
    mac.update(message(camera_id, exp).as_bytes());
    let sig: String = mac
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{exp}.{sig}")
}

/// Whether `token` opens `camera_id` at `now` (Unix seconds).
pub fn verify(key: &str, camera_id: &str, token: &str, now: i64) -> bool {
    let Some((exp, sig_hex)) = token.split_once('.') else {
        return false;
    };
    let Ok(exp) = exp.parse::<i64>() else {
        return false;
    };
    if exp < now || sig_hex.len() != 64 {
        return false;
    }
    let Some(sig) = decode_hex(sig_hex) else {
        return false;
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC takes any key size");
    mac.update(message(camera_id, exp).as_bytes());
    // Constant-time comparison.
    mac.verify_slice(&sig).is_ok()
}

/// The `st` value from a raw query string, if present.
pub fn from_query(raw_query: &str) -> Option<&str> {
    raw_query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == PARAM && !value.is_empty()).then_some(value)
    })
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Computed independently with Python's hmac module; Command Center's
    // `integration::stream_token` asserts the same vector, so the two
    // sides cannot drift apart.
    const KEY: &str = "f3702f9692e7bce4e7dc0b10fe460daf0bdc6c2c741d4c2fabcdb6df44dbb4c9";
    const TOKEN: &str =
        "1700000000.de416edd8a624bdbff2c73bde90873ca60c6b28d3eede7d0c1dc7bb2c0814573";

    #[test]
    fn matches_the_shared_test_vector() {
        assert_eq!(key_for("test-node-key"), KEY);
        assert_eq!(sign(KEY, "cam1", 1_700_000_000), TOKEN);
    }

    #[test]
    fn opens_its_camera_until_it_expires() {
        assert!(verify(KEY, "cam1", TOKEN, 1_699_999_999));
        assert!(verify(KEY, "cam1", TOKEN, 1_700_000_000));
        assert!(!verify(KEY, "cam1", TOKEN, 1_700_000_001), "expired");
    }

    #[test]
    fn opens_nothing_else() {
        assert!(!verify(KEY, "cam2", TOKEN, 0), "another camera");
        assert!(
            !verify(&key_for("another-node-key"), "cam1", TOKEN, 0),
            "another node"
        );
        let tampered = TOKEN.replace("1700000000.", "1800000000.");
        assert!(!verify(KEY, "cam1", &tampered, 0), "a later expiry");
        for junk in ["", ".", "x.y", "1700000000", "1700000000.zz"] {
            assert!(!verify(KEY, "cam1", junk, 0), "{junk:?}");
        }
    }

    #[test]
    fn the_token_is_read_from_the_query() {
        assert_eq!(from_query("st=abc"), Some("abc"));
        assert_eq!(from_query("x=1&st=abc&y=2"), Some("abc"));
        assert_eq!(from_query("st="), None);
        assert_eq!(from_query("other=1"), None);
    }
}
