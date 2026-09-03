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
//! Configuration loading and parsing

mod settings;

pub use settings::*;

use crate::error::{Error, Result};
use crate::storage::NodeDatabase;

/// CLI argument overrides (passed from main)
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub node_id: Option<String>,
    pub api_key: Option<String>,
    pub api_url: Option<String>,
}

/// Probe whether `preferred` is free to bind, and if not, scan forward
/// for the first free port. Used by both setup paths (interactive
/// wizard and scripted `run_quick_setup`) so a fresh install never
/// silently picks a port something else already holds, and defensively
/// by the runtime HTTP server (`server::http::HttpServer::run`) in case
/// the port was free at setup time but got taken before this boot.
///
/// Written for a real conflict hit during testing: a Docker container
/// (`docker-proxy`) was already publishing an unrelated service on
/// 8080, and the old code never checked — the setup wizard just
/// assumed 8080 was free, so the operator's browser loaded the
/// stranger's app instead of CloudNode's dashboard with no indication
/// anything was wrong.
///
/// Always probes `0.0.0.0` regardless of the node's own eventual bind
/// address: on Linux (and every other platform this ships for), a
/// process holding `0.0.0.0:<port>` reserves that port across every
/// local address, so a subsequent bind to `127.0.0.1:<port>` fails
/// too — checking the broadest address is therefore also the correct
/// check for a loopback-only bind, not just for Local mode's 0.0.0.0.
///
/// There's an inherent TOCTOU race between this check and the real
/// bind moments later (the probe listener is dropped immediately after
/// confirming the port is free) — acceptable for picking a good
/// default, not a hard guarantee, which is why the runtime server
/// checks again independently rather than trusting whatever setup
/// decided.
pub fn find_available_port(preferred: u16) -> u16 {
    use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

    let is_free = |port: u16| -> bool {
        TcpListener::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port)).is_ok()
    };

    if is_free(preferred) {
        return preferred;
    }

    let upper = preferred.saturating_add(500).min(65500);
    for candidate in preferred.saturating_add(1)..=upper {
        if is_free(candidate) {
            return candidate;
        }
    }

    // Scanning failed to find anything free in range — hand back the
    // original. The real bind attempt will fail loudly, which is no
    // worse than today's behavior and keeps this function infallible
    // for callers that don't want to thread a Result through UI code.
    preferred
}

impl Config {
    /// Load configuration: try DB first, then fall back to YAML/env.
    pub fn load(path: Option<&str>) -> Result<Self> {
        // 1. Try loading from the SQLite database (new path).
        //
        // Path resolution lives in `crate::paths::config_db_path()` so a
        // Windows-Service install (cwd = System32) finds its DB under
        // %ProgramData%\SourceBoxSentry\ instead of trying to write inside
        // System32. See paths::data_dir() for the full resolution order.
        let db_path = crate::paths::config_db_path();
        if db_path.exists() {
            if let Ok(db) = NodeDatabase::new(&db_path) {
                if db.has_config() {
                    tracing::info!("Loading config from database");
                    return Self::load_from_db(&db);
                }
            }
        }

        // 2. Fall back to YAML file + env vars (legacy path)
        let mut config = if let Some(p) = path {
            Self::from_file(p)?
        } else {
            Self::from_default_locations()?
        };

        config = config.with_env_overrides();

        // 3. If we loaded from legacy sources and have credentials,
        //    migrate them to the database for next time. Make sure the
        //    parent dir exists — on a fresh MSI install %ProgramData%\
        //    SourceBoxSentry\ may not exist yet because the WiX template
        //    only creates the install dir, not the data dir.
        if !config.cloud.api_key.is_empty() && config.node.node_id.is_some() {
            if let Some(parent) = db_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(db) = NodeDatabase::new(&db_path) {
                if let Err(e) = config.save_to_db(&db) {
                    tracing::warn!("Config migration to DB failed: {}", e);
                } else {
                    tracing::info!("Config migrated to database (API key encrypted)");
                }
            }
        }

        Ok(config)
    }

    /// Load all config values from the SQLite database.
    pub fn load_from_db(db: &NodeDatabase) -> Result<Self> {
        let mut config = Config::default();

        // Mode (Local | Connected). Absent row → Connected (back-compat
        // for any DB written by a pre-mode-flag binary).
        if let Some(v) = db.get_config("mode")? {
            config.mode = NodeMode::from_str(&v);
        }
        if let Some(v) = db.get_config("node_id")? {
            config.node.node_id = Some(v);
        }
        if let Some(v) = db.get_config("node_name")? {
            config.node.name = v;
        }
        // API key is encrypted at rest
        if let Some(v) = db.get_config_encrypted("api_key")? {
            config.cloud.api_key = v;
        }
        if let Some(v) = db.get_config("api_url")? {
            config.cloud.api_url = v;
        }
        if let Some(v) = db.get_config("heartbeat_interval")? {
            config.cloud.heartbeat_interval = v.parse().unwrap_or(30);
        }
        // `storage_path` row is read-skipped: it was retired with the
        // v0.1.40 cleanup that removed StorageConfig.path. Existing DBs
        // may still have a stale row; harmless because nothing reads
        // it. Not bothering with a delete migration — the kv table
        // is cheap and a leftover row is invisible.
        if let Some(v) = db.get_config("max_size_gb")? {
            config.storage.max_size_gb = v.parse().unwrap_or(64);
        }
        if let Some(v) = db.get_config("fps")? {
            config.streaming.fps = v.parse().unwrap_or(30);
        }
        if let Some(v) = db.get_config("encoder")? {
            config.streaming.encoder = v;
        }
        if let Some(v) = db.get_config("segment_duration")? {
            config.streaming.hls.segment_duration = v.parse().unwrap_or(1);
        }
        if let Some(v) = db.get_config("hls_enabled")? {
            config.streaming.hls.enabled = v == "true";
        }
        if let Some(v) = db.get_config("bitrate")? {
            config.streaming.hls.bitrate = v;
        }
        if let Some(v) = db.get_config("server_port")? {
            config.server.port = v.parse().unwrap_or(8080);
        }
        // Phase A made `bind` mode-dependent (127.0.0.1 in Connected,
        // 0.0.0.0 in Local) — persist it so the wizard's choice survives
        // a restart.  Pre-Phase-A DBs have no row; defaults stay at
        // 127.0.0.1 via Config::default — safe for Connected installs.
        if let Some(v) = db.get_config("server_bind")? {
            config.server.bind = v;
        }
        if let Some(v) = db.get_config("log_level")? {
            config.logging.level = v;
        }
        if let Some(v) = db.get_config("motion_enabled")? {
            config.motion.enabled = v == "true";
        }
        if let Some(v) = db.get_config("motion_sensitivity")? {
            config.motion.threshold = v.parse().unwrap_or(0.02);
        }
        if let Some(v) = db.get_config("motion_cooldown")? {
            config.motion.cooldown_secs = v.parse().unwrap_or(30);
        }
        // Password hash is a one-way argon2 output — plain KV is fine,
        // same as storing any password hash. The session secret signs
        // forgeable session tokens if leaked, so it goes through the
        // encrypted path (same machine-id-derived AES key as the
        // Connected-mode API key).
        if let Some(v) = db.get_config("admin_password_hash")? {
            config.auth.password_hash = Some(v);
        }
        if let Some(v) = db.get_config_encrypted("session_secret")? {
            config.auth.session_secret = Some(v);
        }

        // Still allow env vars to override DB values (useful for debugging)
        config = config.with_env_overrides();

        Ok(config)
    }

    /// Save all config values to the SQLite database.
    /// The API key is encrypted at rest.
    pub fn save_to_db(&self, db: &NodeDatabase) -> Result<()> {
        // Persist mode so a freshly-installed Local-mode node's
        // run_cloudnode_once doesn't fall back to Connected validation.
        db.set_config("mode", self.mode.as_str())?;
        if let Some(ref id) = self.node.node_id {
            db.set_config("node_id", id)?;
        }
        db.set_config("node_name", &self.node.name)?;
        db.set_config("api_url", &self.cloud.api_url)?;
        db.set_config("heartbeat_interval", &self.cloud.heartbeat_interval.to_string())?;
        // No `storage_path` write — field retired in v0.1.40, see note
        // in load_from_db. paths::data_dir() owns location now.
        db.set_config("max_size_gb", &self.storage.max_size_gb.to_string())?;
        db.set_config("fps", &self.streaming.fps.to_string())?;
        if !self.streaming.encoder.is_empty() {
            db.set_config("encoder", &self.streaming.encoder)?;
        }
        db.set_config("segment_duration", &self.streaming.hls.segment_duration.to_string())?;
        db.set_config("hls_enabled", if self.streaming.hls.enabled { "true" } else { "false" })?;
        db.set_config("bitrate", &self.streaming.hls.bitrate)?;
        db.set_config("server_port", &self.server.port.to_string())?;
        db.set_config("server_bind", &self.server.bind)?;
        db.set_config("log_level", &self.logging.level)?;
        db.set_config("motion_enabled", if self.motion.enabled { "true" } else { "false" })?;
        db.set_config("motion_sensitivity", &self.motion.threshold.to_string())?;
        db.set_config("motion_cooldown", &self.motion.cooldown_secs.to_string())?;

        // Encrypt the API key
        if !self.cloud.api_key.is_empty() {
            db.set_config_encrypted("api_key", &self.cloud.api_key)?;
        }

        if let Some(ref hash) = self.auth.password_hash {
            db.set_config("admin_password_hash", hash)?;
        }
        if let Some(ref secret) = self.auth.session_secret {
            db.set_config_encrypted("session_secret", secret)?;
        }

        Ok(())
    }

    fn from_file(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("Failed to read config file {}: {}", path, e)))?;

        Self::parse_yaml(&content)
    }

    fn from_default_locations() -> Result<Self> {
        let default_paths = vec![
            "./config.yaml",
            "./config.yml",
            "/etc/sourcebox-sentry/config.yaml",
            "/etc/sourcebox-sentry/config.yml",
        ];

        for path in default_paths {
            if std::path::Path::new(path).exists() {
                tracing::info!("Loading config from {}", path);
                return Self::from_file(path);
            }
        }

        tracing::info!("No config file found, using defaults");
        Ok(Self::default())
    }

    fn parse_yaml(content: &str) -> Result<Self> {
        // serde_yaml::Value indexing returns Value::Null for missing
        // keys, and the typed accessors (.as_str/.as_i64/.as_bool/
        // .as_sequence) return None on type mismatch.  That gives us
        // the same "best-effort, ignore-missing" parsing the
        // yaml-rust version had — see the migration note in
        // Cargo.toml for why we moved off yaml-rust.
        let doc: serde_yaml::Value = serde_yaml::from_str(content)?;
        if doc.is_null() {
            return Err(Error::Config("Empty config file".into()));
        }

        let mut config = Config::default();

        // Parse node config
        let node = &doc["node"];
        if !node.is_null() {
            if let Some(name) = node["name"].as_str() {
                config.node.name = name.to_string();
            }
        }

        // Parse cloud config
        let cloud = &doc["cloud"];
        if !cloud.is_null() {
            if let Some(api_url) = cloud["api_url"].as_str() {
                config.cloud.api_url = api_url.to_string();
            }
            if let Some(heartbeat_interval) = cloud["heartbeat_interval"].as_i64() {
                config.cloud.heartbeat_interval = heartbeat_interval as u64;
            }
        }

        // Parse cameras config
        let cameras = &doc["cameras"];
        if !cameras.is_null() {
            if let Some(auto_detect) = cameras["auto_detect"].as_bool() {
                config.cameras.auto_detect = auto_detect;
            }
            if let Some(devices) = cameras["devices"].as_sequence() {
                config.cameras.devices = devices
                    .iter()
                    .filter_map(|d| d.as_str().map(|s| s.to_string()))
                    .collect();
            }
        }

        // Parse streaming config
        let streaming = &doc["streaming"];
        if !streaming.is_null() {
            if let Some(fps) = streaming["fps"].as_i64() {
                config.streaming.fps = fps as u32;
            }
            if let Some(jpeg_quality) = streaming["jpeg_quality"].as_i64() {
                config.streaming.jpeg_quality = jpeg_quality as u8;
            }
            // Parse HLS config
            let hls = &streaming["hls"];
            if !hls.is_null() {
                if let Some(enabled) = hls["enabled"].as_bool() {
                    config.streaming.hls.enabled = enabled;
                }
                if let Some(segment_duration) = hls["segment_duration"].as_i64() {
                    config.streaming.hls.segment_duration = segment_duration as u32;
                }
                if let Some(playlist_size) = hls["playlist_size"].as_i64() {
                    config.streaming.hls.playlist_size = playlist_size as u32;
                }
                if let Some(bitrate) = hls["bitrate"].as_str() {
                    config.streaming.hls.bitrate = bitrate.to_string();
                }
            }
        }

        // Parse storage config.  Note: a `path` key in legacy YAML is
        // silently ignored — see StorageConfig docs in settings.rs for
        // why the field was retired.
        let storage = &doc["storage"];
        if !storage.is_null() {
            if let Some(max_size_gb) = storage["max_size_gb"].as_i64() {
                config.storage.max_size_gb = max_size_gb as u64;
            }
        }

        // Parse server config
        let server = &doc["server"];
        if !server.is_null() {
            if let Some(port) = server["port"].as_i64() {
                config.server.port = port as u16;
            }
            if let Some(bind) = server["bind"].as_str() {
                config.server.bind = bind.to_string();
            }
        }

        // Parse logging config
        let logging = &doc["logging"];
        if !logging.is_null() {
            if let Some(level) = logging["level"].as_str() {
                config.logging.level = level.to_string();
            }
        }

        Ok(config)
    }

    fn with_env_overrides(mut self) -> Self {
        // Node ID from env
        if let Ok(node_id) = std::env::var("SOURCEBOX_SENTRY_NODE_ID") {
            self.node.node_id = Some(node_id);
        }

        // API key from env
        if let Ok(key) = std::env::var("SOURCEBOX_SENTRY_API_KEY") {
            self.cloud.api_key = key;
        }

        // API URL from env
        if let Ok(url) = std::env::var("SOURCEBOX_SENTRY_API_URL") {
            self.cloud.api_url = url;
        }

        // Encoder from env (legacy: was only in .env, not in Config)
        if let Ok(enc) = std::env::var("SOURCEBOX_SENTRY_ENCODER") {
            self.streaming.encoder = enc;
        }

        // Log level from env
        if let Ok(level) = std::env::var("RUST_LOG") {
            self.logging.level = level;
        }

        self
    }

    /// Apply CLI argument overrides
    pub fn with_overrides(mut self, overrides: CliOverrides) -> Self {
        if let Some(node_id) = overrides.node_id {
            self.node.node_id = Some(node_id);
        }
        if let Some(api_key) = overrides.api_key {
            self.cloud.api_key = api_key;
        }
        if let Some(api_url) = overrides.api_url {
            self.cloud.api_url = api_url;
        }
        self
    }
}

#[cfg(test)]
mod auth_db_roundtrip_tests {
    //! Pin down that AuthConfig actually round-trips through the real
    //! SQLite KV table via save_to_db/load_from_db — the one path unit
    //! tests on server::auth don't cover, since those only exercise
    //! the crypto helpers and the in-memory config struct directly.

    use super::*;

    #[test]
    fn password_hash_and_session_secret_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = NodeDatabase::new(&tmp.path().join("node.db")).expect("db opens");

        let secret = crate::server::auth::generate_session_secret();
        let mut config = Config::default();
        config.mode = NodeMode::Local;
        config.auth.password_hash =
            Some(crate::server::auth::hash_password("test password 123").unwrap());
        config.auth.session_secret = Some(crate::server::auth::encode_secret(&secret));
        config.save_to_db(&db).expect("save");

        let loaded = Config::load_from_db(&db).expect("load");
        assert!(crate::server::auth::verify_password(
            "test password 123",
            loaded.auth.password_hash.as_deref().expect("hash present"),
        ));
        assert_eq!(
            loaded
                .auth
                .session_secret
                .as_deref()
                .and_then(crate::server::auth::decode_secret),
            Some(secret),
        );
    }

    #[test]
    fn absent_auth_fields_load_as_none() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db = NodeDatabase::new(&tmp.path().join("node.db")).expect("db opens");
        let config = Config::default();
        config.save_to_db(&db).expect("save");

        let loaded = Config::load_from_db(&db).expect("load");
        assert!(loaded.auth.password_hash.is_none());
        assert!(loaded.auth.session_secret.is_none());
    }
}

#[cfg(test)]
mod yaml_parse_tests {
    //! Pin down the YAML parser's behaviour after the v0.1.40+
    //! yaml-rust → serde_yaml migration.  The legacy code path is
    //! rarely hit (every node migrates to SQLite on first load) but
    //! a regression here would silently drop config on any node
    //! still relying on `config.yaml`.

    use super::*;

    #[test]
    fn parse_yaml_picks_up_node_and_cloud_fields() {
        let yaml = r#"
node:
  name: test-node
cloud:
  api_url: https://example.com
  heartbeat_interval: 45
"#;
        let config = Config::parse_yaml(yaml).expect("yaml parses");
        assert_eq!(config.node.name, "test-node");
        assert_eq!(config.cloud.api_url, "https://example.com");
        assert_eq!(config.cloud.heartbeat_interval, 45);
    }

    #[test]
    fn parse_yaml_uses_defaults_for_missing_sections() {
        // Only 'node' present — every other section should fall back
        // to Config::default().  This is the "best-effort" semantics
        // the yaml-rust path had; serde_yaml needs to match.
        let yaml = "node:\n  name: minimal\n";
        let config = Config::parse_yaml(yaml).expect("yaml parses");
        let defaults = Config::default();
        assert_eq!(config.node.name, "minimal");
        // Cloud + storage should be untouched defaults.
        assert_eq!(config.cloud.heartbeat_interval, defaults.cloud.heartbeat_interval);
        assert_eq!(config.storage.max_size_gb, defaults.storage.max_size_gb);
    }

    #[test]
    fn parse_yaml_handles_camera_devices_list() {
        let yaml = r#"
cameras:
  auto_detect: false
  devices:
    - /dev/video0
    - /dev/video2
"#;
        let config = Config::parse_yaml(yaml).expect("yaml parses");
        assert!(!config.cameras.auto_detect);
        assert_eq!(
            config.cameras.devices,
            vec!["/dev/video0".to_string(), "/dev/video2".to_string()]
        );
    }

    #[test]
    fn parse_yaml_silently_ignores_legacy_storage_path() {
        // `storage.path` was removed in v0.1.40 (see StorageConfig
        // docs).  YAML files in the wild may still set it; the
        // parser must not error on it.
        let yaml = r#"
storage:
  path: ./data
  max_size_gb: 32
"#;
        let config = Config::parse_yaml(yaml).expect("yaml parses");
        assert_eq!(config.storage.max_size_gb, 32);
    }

    #[test]
    fn parse_yaml_rejects_empty_input() {
        let err = Config::parse_yaml("").unwrap_err();
        assert!(format!("{}", err).contains("Empty"));
    }

    #[test]
    fn parse_yaml_rejects_garbage() {
        // Malformed YAML should surface as a Config error, not a
        // panic or a silent default.
        let err = Config::parse_yaml("\t\t: not valid: yaml :::").unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    // ── find_available_port ─────────────────────────────────────────

    #[test]
    fn find_available_port_returns_preferred_when_free() {
        // Bind something transient first to get a genuinely free port
        // from the OS, then release it immediately.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let free_port = listener.local_addr().unwrap().port();
        drop(listener);

        assert_eq!(find_available_port(free_port), free_port);
    }

    #[test]
    fn find_available_port_falls_back_when_preferred_is_taken() {
        // Hold the preferred port open for the whole check.
        let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let preferred = held.local_addr().unwrap().port();

        let resolved = find_available_port(preferred);

        assert_ne!(resolved, preferred);
        // The fallback must actually be free — prove it by binding it.
        std::net::TcpListener::bind(("0.0.0.0", resolved)).unwrap();
    }

    #[test]
    fn find_available_port_fallback_is_still_bindable_on_loopback() {
        // Regression: the function probes 0.0.0.0 (matches how
        // docker-proxy and most services actually squat a port), but
        // the value it returns must be just as free on a
        // loopback-only bind — which it will be, since a 0.0.0.0
        // holder blocks every local address for that port already.
        let held = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let preferred = held.local_addr().unwrap().port();

        let resolved = find_available_port(preferred);

        std::net::TcpListener::bind(("127.0.0.1", resolved)).unwrap();
    }
}
