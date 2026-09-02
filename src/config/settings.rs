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
//! Configuration settings structures

use serde::{Deserialize, Serialize};

/// Operating mode chosen at install time.
///
/// `Connected` (default for back-compat): the node registers with a
/// Sentinel Command Center, opens a WebSocket for inbound
/// commands, sends heartbeats, and pushes HLS segments. The node is
/// just one half of a SaaS product.
///
/// `Local`: standalone install with no Command Center pairing. The
/// node still discovers cameras, runs FFmpeg pipelines, serves HLS
/// locally, and records to the encrypted SQLite — but every
/// CC-coupled path in `node::runner::run_internal` is gated off.
/// Users open `http://<node-ip>:8080` in a browser and use the local
/// web UI for snapshots / recording / playback.
///
/// New nodes pick this at the setup wizard's first prompt. Existing
/// installs upgrade in-place: an absent `mode` row in `node.db`
/// defaults to `Connected`, so behaviour is unchanged after binary
/// swap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeMode {
    Local,
    Connected,
}

impl Default for NodeMode {
    fn default() -> Self {
        Self::Connected
    }
}

impl NodeMode {
    /// String form persisted in the SQLite KV (`config` table) and used
    /// by serde when serialising config. Stable wire value — don't
    /// rename without a migration.
    pub fn as_str(&self) -> &'static str {
        match self {
            NodeMode::Local => "local",
            NodeMode::Connected => "connected",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "local" => Self::Local,
            // Treat any other (or absent) value as Connected — that's
            // the only safe default for a node already running, since
            // turning off CC paths on a running install would silently
            // stop heartbeats / segment push.
            _ => Self::Connected,
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, NodeMode::Local)
    }

    pub fn is_connected(&self) -> bool {
        matches!(self, NodeMode::Connected)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Local-only vs Connected install (see `NodeMode`).  Defaults to
    /// Connected for existing-install back-compat — see the type's
    /// rustdoc.
    #[serde(default)]
    pub mode: NodeMode,
    pub node: NodeConfig,
    pub cloud: CloudConfig,
    pub cameras: CamerasConfig,
    pub streaming: StreamingConfig,
    pub recording: RecordingConfig,
    pub storage: StorageConfig,
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    pub motion: MotionConfig,
    #[serde(default)]
    pub auth: AuthConfig,
}

impl Default for Config {
    fn default() -> Self {
        let _hostname =
            sysinfo::System::host_name().unwrap_or_else(|| "sourcebox-sentry-node".to_string());

        Self {
            mode: NodeMode::default(),
            node: NodeConfig::default(),
            cloud: CloudConfig::default(),
            cameras: CamerasConfig::default(),
            streaming: StreamingConfig::default(),
            recording: RecordingConfig::default(),
            storage: StorageConfig::default(),
            server: ServerConfig::default(),
            logging: LoggingConfig::default(),
            motion: MotionConfig::default(),
            auth: AuthConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    /// Friendly name shown in dashboard
    pub name: String,

    /// Node ID (assigned by cloud)
    #[serde(skip_serializing)]
    pub node_id: Option<String>,
}

impl Default for NodeConfig {
    fn default() -> Self {
        let _hostname =
            sysinfo::System::host_name().unwrap_or_else(|| "sourcebox-sentry-node".to_string());

        Self {
            name: format!("Node-{}", _hostname),
            node_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudConfig {
    /// Sentinel Command Center URL
    pub api_url: String,

    /// Organization API key for authentication
    #[serde(skip_serializing)]
    pub api_key: String,

    /// Heartbeat interval in seconds
    pub heartbeat_interval: u64,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            api_url: "http://localhost:8000".to_string(),
            api_key: String::new(),
            heartbeat_interval: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CamerasConfig {
    /// Auto-detect USB cameras on startup
    pub auto_detect: bool,

    /// Manual camera device paths (used if auto_detect is false)
    #[serde(default)]
    pub devices: Vec<String>,
}

impl Default for CamerasConfig {
    fn default() -> Self {
        Self {
            auto_detect: true,
            devices: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingConfig {
    /// Target frames per second
    pub fps: u32,

    /// JPEG quality for snapshots/stream (1-100)
    pub jpeg_quality: u8,

    /// Video encoder (e.g. "h264_nvenc", "libx264", or empty for auto-detect)
    pub encoder: String,

    /// HLS streaming configuration
    pub hls: HlsConfig,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            jpeg_quality: 85,
            encoder: String::new(),
            hls: HlsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HlsConfig {
    /// Enable HLS streaming
    pub enabled: bool,

    /// Segment duration in seconds
    pub segment_duration: u32,

    /// Number of segments to keep in playlist
    pub playlist_size: u32,

    /// Video bitrate (e.g., "2500k")
    pub bitrate: String,
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            segment_duration: 1,
            playlist_size: 15,
            bitrate: "2500k".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingConfig {
    /// Enable local recording
    pub enabled: bool,

    /// Recording format: "mp4" or "mkv"
    pub format: String,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            format: "mp4".to_string(),
        }
    }
}

/// Storage limits.  The actual storage *location* is not configurable
/// — `paths::data_dir()` is the single source of truth for where the
/// SQLite DB, HLS segments, and recordings live (env var override
/// `SOURCEBOX_SENTRY_DATA_DIR` exists for Docker, otherwise platform
/// default).  A `path` field used to live here, defaulting to
/// `./data`, but it caused the v0.1.39 bug where `Node::new` resolved
/// it relative to cwd and segments landed in unexpected directories
/// depending on launch context (Start menu shortcut → Program Files,
/// admin PowerShell from System32 → C:\Windows\System32, …).
/// Removing the field forecloses that whole bug class.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Maximum storage size in GB (oldest deleted when exceeded)
    pub max_size_gb: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            max_size_gb: 64,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MotionConfig {
    /// Enable motion detection (on by default)
    pub enabled: bool,

    /// Scene-change threshold (0.0 – 1.0). Lower = more sensitive.
    /// FFmpeg's scene score: 0.0 = identical frames, 1.0 = completely different.
    pub threshold: f64,

    /// Minimum seconds between motion events per camera
    pub cooldown_secs: u64,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold: 0.02,
            cooldown_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// HTTP server port
    pub port: u16,

    /// HTTP server bind address.
    ///
    /// Defaults to `127.0.0.1` — a `0.0.0.0` bind (Local mode, always —
    /// or Connected mode with `--lan-streaming`) lets anyone on the LAN
    /// reach live HLS/snapshots/recordings, so the setup wizard requires
    /// a local-admin password (see `AuthConfig`) whenever this is set
    /// to anything but the loopback default.
    pub bind: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: 8080,
            bind: "127.0.0.1".to_string(),
        }
    }
}

/// Local-admin auth for LAN-exposed installs (`server.bind != 127.0.0.1`
/// — Local mode always, or Connected mode with `--lan-streaming`).
///
/// `password_hash` is a one-way argon2 hash — safe to store in the
/// plain (unencrypted) `config` KV table, same as any password hash.
/// `session_secret` signs stateless session tokens (see
/// `server::auth`); unlike the password hash, its disclosure lets an
/// attacker forge a valid session, so it's persisted through the
/// existing machine-id-derived-AES `set_config_encrypted` path — the
/// same mechanism already protecting the Connected-mode API key.
///
/// Both fields are `None` exactly when the server is loopback-only
/// (auth isn't needed and the wizard never collected a password) — see
/// `ServerConfig::bind` and the wizard's mandatory-password prompt for
/// any LAN-exposing configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(skip_serializing)]
    pub password_hash: Option<String>,
    #[serde(skip_serializing)]
    pub session_secret: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level: trace, debug, info, warn, error
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

#[cfg(test)]
mod node_mode_tests {
    //! Pin the wire format + back-compat behaviour of the install-time
    //! mode flag.  Default = Connected because absent rows in
    //! `node.db` (older binaries that wrote no `mode` row) must keep
    //! their existing CC behaviour after binary swap.
    use super::*;

    #[test]
    fn default_is_connected() {
        assert_eq!(NodeMode::default(), NodeMode::Connected);
        let cfg = Config::default();
        assert!(cfg.mode.is_connected());
        assert!(!cfg.mode.is_local());
    }

    #[test]
    fn from_str_round_trip() {
        assert_eq!(NodeMode::from_str("local"), NodeMode::Local);
        assert_eq!(NodeMode::from_str("connected"), NodeMode::Connected);
        // Anything else (typo, missing row, future value) decays to
        // Connected so a running install never silently flips to
        // Local on a malformed read.
        assert_eq!(NodeMode::from_str(""), NodeMode::Connected);
        assert_eq!(NodeMode::from_str("LOCAL"), NodeMode::Connected);
        assert_eq!(NodeMode::from_str("future_mode"), NodeMode::Connected);
    }

    #[test]
    fn as_str_matches_persisted_format() {
        assert_eq!(NodeMode::Local.as_str(), "local");
        assert_eq!(NodeMode::Connected.as_str(), "connected");
    }

    #[test]
    fn predicate_helpers_are_consistent() {
        assert!(NodeMode::Local.is_local());
        assert!(!NodeMode::Local.is_connected());
        assert!(NodeMode::Connected.is_connected());
        assert!(!NodeMode::Connected.is_local());
    }

    #[test]
    fn config_serde_writes_mode_field() {
        // Pin the wire format: an explicitly-Local config serialises
        // to YAML with `mode: local`.  The reverse direction (full
        // round-trip) doesn't work because CloudConfig::api_key has
        // `skip_serializing` and is required on deserialize — that's
        // a pre-existing quirk, exercised by the back-compat test
        // below that handcrafts the input YAML.
        let cfg = Config {
            mode: NodeMode::Local,
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&cfg).expect("serialize");
        assert!(yaml.contains("mode: local"), "yaml was: {}", yaml);

        let cfg_connected = Config::default();
        let yaml = serde_yaml::to_string(&cfg_connected).expect("serialize");
        assert!(yaml.contains("mode: connected"), "yaml was: {}", yaml);
    }

    #[test]
    fn config_serde_back_compat_missing_mode_field() {
        // Yaml from a pre-mode-flag binary won't have a `mode` field.
        // The `#[serde(default)]` attribute on `Config::mode` should
        // make it default to Connected.
        let yaml = r#"
node:
  name: legacy-node
cloud:
  api_url: https://example.com
  api_key: ""
  heartbeat_interval: 30
cameras:
  auto_detect: true
streaming:
  fps: 30
  jpeg_quality: 85
  encoder: ""
  hls:
    enabled: true
    segment_duration: 1
    playlist_size: 15
    bitrate: "2500k"
recording:
  enabled: true
  format: "mp4"
storage:
  max_size_gb: 64
server:
  port: 8080
  bind: "127.0.0.1"
logging:
  level: "info"
motion:
  enabled: true
  threshold: 0.02
  cooldown_secs: 30
"#;
        let parsed: Config = serde_yaml::from_str(yaml).expect("legacy yaml parses");
        assert!(parsed.mode.is_connected(), "missing mode → Connected");
    }
}
