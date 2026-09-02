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
//! Error recovery and user-friendly error messages
//!
//! Full-width bordered panels that match the CloudNode setup wizard aesthetic
//! (`ui::panel_*`), themed red for failure and yellow/green for warning/success.

use colored::{Color, Colorize};

use super::ui::{
    panel_blank_color, panel_bottom_color, panel_center_color, panel_divider_color,
    panel_row_color, panel_top_color,
};
use crate::storage::NodeDatabase;

const FAIL: Color = Color::Red;
const WARN: Color = Color::Yellow;
const OK: Color = Color::Green;

/// Registration error types with recovery suggestions.
#[derive(Debug, Clone)]
pub enum RegistrationError {
    /// Node ID not found in Command Center
    InvalidNodeId { node_id: String, api_url: String },
    /// API Key is invalid or doesn't match the node
    InvalidApiKey { node_id: String, api_url: String },
    /// Network connectivity issue
    NetworkError { api_url: String, message: String },
    /// Server returned an error
    ServerError { code: u16, message: String },
    /// Camera codec detection failed
    CodecDetectionFailed { message: String },
    /// Configuration file missing or invalid
    ConfigError { message: String },
}

impl RegistrationError {
    /// Whether a "wipe credentials and re-run setup" offer makes sense for
    /// this error. Only credential/config problems qualify — wiping won't
    /// fix network outages or codec detection failures.
    pub fn offers_reset(&self) -> bool {
        matches!(
            self,
            Self::InvalidNodeId { .. }
                | Self::InvalidApiKey { .. }
                | Self::ConfigError { .. }
        )
    }
}

/// Render a full-width red-bordered registration-failure panel.
///
/// Layout: title bar → centered error code + caption → context key/values →
/// numbered next-steps → dimmed footer. Matches the cyan panel aesthetic of the
/// setup wizard (`ui::panel_*`), recolored red to signal failure.
pub fn show_registration_error(error: &RegistrationError) {
    let (code, caption, kv, steps) = section_for(error);
    let trouble_url = "https://github.com/SourceBox-LLC/Sentinel-CameraNode#troubleshooting";

    println!();
    panel_top_color("Registration Failed", FAIL);
    panel_blank_color(FAIL);

    // Centered error code + caption.
    panel_center_color(
        &format!(
            "{}  {}",
            "✗".bright_red().bold(),
            code.bright_red().bold(),
        ),
        FAIL,
    );
    panel_center_color(&caption.white().dimmed().to_string(), FAIL);

    panel_blank_color(FAIL);
    panel_divider_color(FAIL);
    panel_blank_color(FAIL);

    // Context: key/value block with aligned keys.
    if !kv.is_empty() {
        let key_width = kv.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
        for (k, v) in &kv {
            let padded_key = format!("{:<width$}", k, width = key_width);
            panel_row_color(
                &format!("     {}  :  {}", padded_key.white().bold(), v),
                FAIL,
            );
        }
        panel_blank_color(FAIL);
        panel_divider_color(FAIL);
        panel_blank_color(FAIL);
    }

    // Next steps.
    panel_row_color(&format!("     {}", "Next steps".white().bold()), FAIL);
    panel_blank_color(FAIL);
    for (i, step) in steps.iter().enumerate() {
        let num = format!("{}.", i + 1);
        panel_row_color(&format!("       {}  {}", num.cyan().bold(), step), FAIL);
    }
    panel_blank_color(FAIL);
    panel_divider_color(FAIL);
    panel_blank_color(FAIL);

    // Footer.
    panel_row_color(
        &format!(
            "       {}  Config is stored in {}",
            "·".bright_black(),
            "data/node.db".dimmed(),
        ),
        FAIL,
    );
    panel_row_color(
        &format!("       {}  Troubleshooting: {}", "·".bright_black(), trouble_url.dimmed()),
        FAIL,
    );
    panel_blank_color(FAIL);
    panel_bottom_color(FAIL);
    println!();
}

/// Show a warning panel (yellow). Full-width, matches setup wizard aesthetic.
pub fn show_warning(title: &str, message: &str) {
    panel_message(title, message, WARN, "⚠".yellow().bold().to_string());
}

/// Render a yellow full-width panel surfacing a backend-reported plan-cap
/// breach. Called from `Node::run` right after registration succeeds when
/// the backend included a `plan_limit_hit` object in the response — the
/// registration itself went through, but one or more cameras were dropped
/// because the org is at its plan's camera cap.
///
/// The panel is purely informational; enforcement happened server-side
/// when the affected cameras were omitted from the `cameras` mapping.
pub fn show_plan_limit_hit(hit: &crate::api::PlanLimitHit, api_url: &str) {
    println!();
    panel_top_color("Plan Limit Reached", WARN);
    panel_blank_color(WARN);

    // Centered headline + caption.
    panel_center_color(
        &format!(
            "{}  {} plan — {} camera cap",
            "⚠".yellow().bold(),
            hit.plan.bold(),
            hit.max_cameras.to_string().yellow().bold(),
        ),
        WARN,
    );
    panel_center_color(
        &"Some cameras were not registered with the cloud"
            .white()
            .dimmed()
            .to_string(),
        WARN,
    );

    panel_blank_color(WARN);
    panel_divider_color(WARN);
    panel_blank_color(WARN);

    // Aligned key/value context.  Renders a trailing "…" if the list would
    // overflow the terminal; the full list is always in `hit.detail`, which
    // the backend already formats one-line.
    let kv: Vec<(String, String)> = vec![
        ("Current plan".to_string(), hit.plan.white().bold().to_string()),
        (
            "Camera cap".to_string(),
            hit.max_cameras.to_string().yellow().bold().to_string(),
        ),
        (
            "Skipped".to_string(),
            format_skipped(&hit.skipped).yellow().to_string(),
        ),
    ];
    let key_width = kv.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
    for (k, v) in &kv {
        let padded = format!("{:<width$}", k, width = key_width);
        panel_row_color(
            &format!("     {}  :  {}", padded.white().bold(), v),
            WARN,
        );
    }

    panel_blank_color(WARN);
    panel_divider_color(WARN);
    panel_blank_color(WARN);

    // Next steps.
    panel_row_color(&format!("     {}", "To stream these cameras".white().bold()), WARN);
    panel_blank_color(WARN);
    let arrow = "→".cyan();
    panel_row_color(
        &format!(
            "       {}  Upgrade your plan at {}",
            arrow,
            api_url.cyan(),
        ),
        WARN,
    );
    panel_row_color(
        &format!(
            "       {}  Pro allows 25 cameras  {}  Pro Plus allows 200",
            arrow,
            "·".bright_black(),
        ),
        WARN,
    );

    panel_blank_color(WARN);
    panel_bottom_color(WARN);
    println!();
}

/// Truncate the skipped-camera list if it would overflow a reasonable
/// panel row. The full detail stays in `hit.detail`, which the backend
/// already pre-formats for the log.
fn format_skipped(names: &[String]) -> String {
    const MAX_CHARS: usize = 80;
    if names.is_empty() {
        return "(none)".to_string();
    }
    let full = names.join(", ");
    if full.chars().count() <= MAX_CHARS {
        return full;
    }
    // Drop names from the end until we fit, then append a count marker.
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for name in names {
        let cost = name.chars().count() + if kept.is_empty() { 0 } else { 2 }; // ", "
        if used + cost > MAX_CHARS - 8 {
            break;
        }
        used += cost;
        kept.push(name);
    }
    let dropped = names.len() - kept.len();
    if kept.is_empty() {
        format!("{} cameras", names.len())
    } else {
        format!("{} (+{} more)", kept.join(", "), dropped)
    }
}

/// Show a success panel (green). Full-width, matches setup wizard aesthetic.
pub fn show_success(title: &str, message: &str) {
    panel_message(title, message, OK, "✓".bright_green().bold().to_string());
}

fn panel_message(title: &str, message: &str, color: Color, icon: String) {
    println!();
    panel_top_color(title, color);
    panel_blank_color(color);

    panel_center_color(
        &format!("{}  {}", icon, title.bold()),
        color,
    );

    panel_blank_color(color);
    panel_divider_color(color);
    panel_blank_color(color);

    for line in message.lines() {
        panel_row_color(&format!("     {}", line), color);
    }

    panel_blank_color(color);
    panel_bottom_color(color);
    println!();
}

/// Build the presentation bundle for a given registration error.
///
/// Returns `(error_code, caption, key_values, next_steps)`. Strings may include
/// ANSI color codes (they're composed with `colored`).
fn section_for(error: &RegistrationError) -> (String, String, Vec<(String, String)>, Vec<String>) {
    let arrow = |s: &str| format!("{}", s.cyan());
    match error {
        RegistrationError::InvalidNodeId { node_id, api_url } => (
            "ERROR 404 — Node not registered".to_string(),
            "This node does not exist in the Command Center yet".to_string(),
            vec![
                (
                    "Node ID".to_string(),
                    node_id.yellow().bold().to_string(),
                ),
                (
                    "Command Center".to_string(),
                    api_url.cyan().to_string(),
                ),
            ],
            vec![
                format!("Open {} in your browser", arrow(api_url)),
                "Navigate to Settings → Nodes and click Add Node".to_string(),
                "Copy the generated Node ID".to_string(),
                "Re-run setup with the new Node ID".to_string(),
            ],
        ),
        RegistrationError::InvalidApiKey { node_id, api_url } => (
            "ERROR 401 — API key rejected".to_string(),
            "The stored API key is missing or does not match this node".to_string(),
            vec![
                (
                    "Node ID".to_string(),
                    node_id.yellow().bold().to_string(),
                ),
                (
                    "Command Center".to_string(),
                    api_url.cyan().to_string(),
                ),
            ],
            vec![
                format!("Open {} in your browser", arrow(api_url)),
                "Go to Settings → Nodes and open your node".to_string(),
                "Copy the API Key (full UUID)".to_string(),
                "Re-run setup and paste the new key".to_string(),
            ],
        ),
        RegistrationError::NetworkError { api_url, message } => (
            "NETWORK ERROR — Command Center unreachable".to_string(),
            message.lines().next().unwrap_or("connection failed").to_string(),
            vec![
                (
                    "Command Center".to_string(),
                    api_url.cyan().to_string(),
                ),
            ],
            vec![
                "Check your internet connection".to_string(),
                format!("Verify {} is reachable from this machine", arrow(api_url)),
                "Retry in a moment — transient outages usually resolve quickly".to_string(),
            ],
        ),
        RegistrationError::ServerError { code, message } => (
            format!("SERVER ERROR {} — Command Center returned an error", code),
            message.lines().next().unwrap_or("server error").to_string(),
            Vec::new(),
            vec![
                "Wait a moment and try again".to_string(),
                "Check the Command Center status page".to_string(),
                "Contact support if the error persists".to_string(),
            ],
        ),
        RegistrationError::CodecDetectionFailed { message } => (
            "CODEC DETECTION FAILED".to_string(),
            "Falling back to H.264 Baseline (avc1.42e01e)".to_string(),
            vec![(
                "Reason".to_string(),
                message
                    .lines()
                    .next()
                    .unwrap_or("unknown")
                    .white()
                    .dimmed()
                    .to_string(),
            )],
            vec![
                "Most USB cameras work with the H.264 fallback".to_string(),
                "If playback fails, check camera firmware for supported profiles".to_string(),
            ],
        ),
        RegistrationError::ConfigError { message } => (
            "CONFIGURATION ERROR".to_string(),
            message.lines().next().unwrap_or("missing or invalid config").to_string(),
            Vec::new(),
            vec![
                "Re-run the setup wizard to create a fresh configuration".to_string(),
                "Confirm the Node ID and API Key come from the same node in Command Center"
                    .to_string(),
            ],
        ),
    }
}

/// Outcome of the credential-reset prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetOutcome {
    /// User confirmed, credentials were wiped. Caller should exit so the user
    /// can re-launch and be picked up by the setup wizard.
    Reset,
    /// User declined, or stdin isn't a tty so we didn't prompt.
    Declined,
}

/// Whether stdin + stdout are both connected to an interactive terminal.
fn is_interactive_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Prompt the user to wipe stored credentials after a registration failure.
///
/// Takes the live `NodeDatabase` handle rather than deleting the file: on
/// Windows the node process holds an open SQLite handle for logging, and
/// `std::fs::remove_file` fails with `os error 32` (sharing violation). Wiping
/// the credential rows via the open connection works on every platform and
/// preserves diagnostic logs the user may still want to inspect.
///
/// Returns `ResetOutcome::Reset` if the user confirmed and the credential
/// rows were cleared (caller should `std::process::exit(0)` so the next run
/// re-enters the setup wizard). Returns `Declined` if the user said no, the
/// db write failed, or there's no interactive terminal.
pub fn prompt_for_reset(db: &NodeDatabase) -> ResetOutcome {
    if !is_interactive_tty() {
        return ResetOutcome::Declined;
    }

    let prompt = format!(
        "  {}  Wipe credentials and re-launch the setup wizard now?",
        "↻".yellow().bold(),
    );

    let confirmed = match inquire::Confirm::new(&prompt)
        .with_default(false)
        .with_help_message(
            "Clears your Node ID and API key from data/node.db, then immediately starts the setup wizard. Logs are preserved.",
        )
        .prompt()
    {
        Ok(v) => v,
        Err(_) => return ResetOutcome::Declined, // user hit Esc / Ctrl-C
    };

    if !confirmed {
        return ResetOutcome::Declined;
    }

    match wipe_credential_rows(db) {
        Ok(()) => {
            show_reset_complete();
            ResetOutcome::Reset
        }
        Err(e) => {
            show_warning(
                "Reset failed",
                &format!(
                    "Could not clear credentials from data/node.db:\n  {}\n\nYou may need to remove the file manually after stopping the node.",
                    e
                ),
            );
            ResetOutcome::Declined
        }
    }
}

/// Clear just the credential rows (`node_id`, `api_key`) from the live
/// database connection. The setup wizard will repopulate them on next launch;
/// all other config — including logs and snapshots — is preserved.
///
/// We deliberately don't delete the file itself: on Windows the running
/// process still holds an open handle, so `remove_file` would fail with a
/// sharing violation (`os error 32`). `DELETE FROM config` works regardless.
fn wipe_credential_rows(db: &NodeDatabase) -> crate::error::Result<()> {
    db.delete_config("node_id")?;
    db.delete_config("api_key")?;
    Ok(())
}

/// Render the post-reset success panel before the setup wizard takes over.
fn show_reset_complete() {
    println!();
    panel_top_color("Reset Complete", OK);
    panel_blank_color(OK);
    panel_center_color(
        &format!("{}  {}", "✓".bright_green().bold(), "Credentials cleared".bold()),
        OK,
    );
    panel_center_color(
        &"Launching the setup wizard…".white().dimmed().to_string(),
        OK,
    );
    panel_blank_color(OK);
    panel_bottom_color(OK);
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_node_id_includes_node_id_and_api_url() {
        let error = RegistrationError::InvalidNodeId {
            node_id: "abc123".to_string(),
            api_url: "https://command.example.com".to_string(),
        };
        let (code, _, kv, steps) = section_for(&error);
        assert!(code.contains("404"));
        assert!(kv.iter().any(|(_, v)| v.contains("abc123")));
        assert!(steps.iter().any(|s| s.contains("command.example.com")));
    }

    #[test]
    fn invalid_api_key_mentions_401() {
        let error = RegistrationError::InvalidApiKey {
            node_id: "n".to_string(),
            api_url: "u".to_string(),
        };
        let (code, _, _, _) = section_for(&error);
        assert!(code.contains("401"));
    }

    #[test]
    fn server_error_includes_status_code() {
        let error = RegistrationError::ServerError {
            code: 503,
            message: "service unavailable".to_string(),
        };
        let (code, _, _, _) = section_for(&error);
        assert!(code.contains("503"));
    }

    /// Visual preview — run with `cargo test setup::recovery::tests::preview --release -- --nocapture --ignored`.
    #[test]
    #[ignore]
    fn preview() {
        show_registration_error(&RegistrationError::InvalidNodeId {
            node_id: "6c27177d".to_string(),
            api_url: "https://command.example.com".to_string(),
        });
        show_registration_error(&RegistrationError::InvalidApiKey {
            node_id: "6c27177d".to_string(),
            api_url: "https://command.example.com".to_string(),
        });
        show_registration_error(&RegistrationError::NetworkError {
            api_url: "https://command.example.com".to_string(),
            message: "dns resolution failed: no such host".to_string(),
        });
        show_registration_error(&RegistrationError::ServerError {
            code: 503,
            message: "upstream unavailable".to_string(),
        });
        show_warning("Codec mismatch", "The camera advertises MJPEG but\nthe stream fell back to YUYV.");
        show_success("Setup complete", "Configuration saved.\nReady to launch.");
        show_reset_complete();

        // Plan-limit panel — backend drops extra cameras on Free tier.
        let hit = crate::api::PlanLimitHit {
            plan: "Free".to_string(),
            max_cameras: 5,
            skipped: vec![
                "Backyard Cam".to_string(),
                "Kitchen".to_string(),
            ],
            detail: "Plan limit reached (5 on Free). Upgrade to add: Backyard Cam, Kitchen.".to_string(),
        };
        show_plan_limit_hit(&hit, "https://sentinel-command.com");

        // And the overflow case — more skipped cameras than a panel row fits.
        let hit_many = crate::api::PlanLimitHit {
            plan: "Free".to_string(),
            max_cameras: 5,
            skipped: (3..=12).map(|i| format!("Camera {}", i)).collect(),
            detail: "Plan limit reached (5 on Free). Upgrade to add 10 cameras.".to_string(),
        };
        show_plan_limit_hit(&hit_many, "https://sentinel-command.com");
    }

    #[test]
    fn offers_reset_covers_credential_and_config_errors() {
        assert!(RegistrationError::InvalidNodeId {
            node_id: "n".into(),
            api_url: "u".into(),
        }
        .offers_reset());
        assert!(RegistrationError::InvalidApiKey {
            node_id: "n".into(),
            api_url: "u".into(),
        }
        .offers_reset());
        assert!(RegistrationError::ConfigError {
            message: "m".into(),
        }
        .offers_reset());
    }

    #[test]
    fn offers_reset_skips_network_and_server_errors() {
        assert!(!RegistrationError::NetworkError {
            api_url: "u".into(),
            message: "m".into(),
        }
        .offers_reset());
        assert!(!RegistrationError::ServerError {
            code: 500,
            message: "m".into(),
        }
        .offers_reset());
        assert!(!RegistrationError::CodecDetectionFailed {
            message: "m".into(),
        }
        .offers_reset());
    }

    #[test]
    fn format_skipped_handles_empty_and_short_and_overflow() {
        assert_eq!(format_skipped(&[]), "(none)");
        assert_eq!(
            format_skipped(&["A".to_string(), "B".to_string()]),
            "A, B"
        );
        let many: Vec<String> = (0..40).map(|i| format!("Cam_{}", i)).collect();
        let out = format_skipped(&many);
        assert!(out.contains("more"), "overflow should mark truncation: {}", out);
        assert!(out.chars().count() <= 120, "overflow should cap width: {}", out);
    }

    #[test]
    fn plan_limit_hit_deserializes_with_extra_fields() {
        // Pydantic's extra=ignore semantics on the backend: we must tolerate
        // new optional fields the backend may add without a node release.
        let raw = r#"{
            "plan": "Pro",
            "max_cameras": 10,
            "skipped": ["Cam 11", "Cam 12"],
            "detail": "Plan limit reached.",
            "future_field": 42
        }"#;
        let hit: crate::api::PlanLimitHit = serde_json::from_str(raw).unwrap();
        assert_eq!(hit.plan, "Pro");
        assert_eq!(hit.max_cameras, 10);
        assert_eq!(hit.skipped.len(), 2);
    }

    #[test]
    fn long_warning_title_does_not_panic() {
        // Regression: old show_warning computed `51 - title.len()` and panicked
        // on long titles. The new panel-based implementation shouldn't care.
        let long = "A very long warning title that is definitely longer than fifty-one cells";
        show_warning(long, "body line 1\nbody line 2");
    }
}
