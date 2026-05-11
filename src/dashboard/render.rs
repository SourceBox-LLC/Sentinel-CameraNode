//! `Dashboard::render` and the format / draw helpers it relies on.
//!
//! Single 376-line `render()` method plus ~190 lines of small string
//! formatting helpers (panel borders, settings divider, status pill,
//! truncation that respects ANSI escapes, etc.). Box-drawing constants
//! live here too — they're never used outside this file.
//!
//! Helpers are `pub(super)` so [`super::commands`] can reuse the ones it
//! needs (`format_bytes` for the `/status` output) without re-exposing
//! them to the rest of the crate.

use std::io::{self, Write};

use colored::Colorize;
use crossterm::terminal;

use super::handle::Dashboard;
use super::types::{CameraStatus, LogEntry, LogLevel, View};
use crate::config::NodeMode;

// ─── Box drawing ────────────────────────────────────────────────────────────
const TL: &str = "╔";
const TR: &str = "╗";
const BL: &str = "╚";
const BR: &str = "╝";
const H: &str = "═";
const V: &str = "║";
const ML: &str = "╠";
const MR: &str = "╣";

impl Dashboard {
    /// Render one full frame to stdout. Redraws from top each time.
    pub fn render(&self) {
        let state = match self.0.lock() {
            Ok(s) => s,
            Err(_) => return,
        };

        let w = term_width();
        let mut out = String::with_capacity(4096);

        // Hide cursor during redraw to prevent flicker, then move to top-left.
        // Do NOT clear screen (\x1B[2J) — overwrite lines in place instead.
        out.push_str("\x1B[?25l\x1B[H");

        // ── Header ───────────────────────────────────────────────────────────
        let title = " ▸ SOURCEBOX SENTRY CLOUDNODE ";
        let title_len = title.chars().count();
        let fill = w.saturating_sub(2 + title_len);
        out.push_str(&format!(
            "{}{}{}{}\x1B[K\n",
            cyan_bold(TL),
            cyan_bold(&format!("{}{}", H, title)),
            cyan_bold(&H.repeat(fill.saturating_sub(1))),
            cyan_bold(TR),
        ));

        // Status bar — mode-aware.
        // Local mode: [LOCAL] badge + clickable local web-UI URL.
        // Connected mode: plan pill + clickable local URL · clickable CC URL.
        // The "↑ segs" counter still ticks in Local mode because the
        // segment_uploader's success counter is operator-facing
        // ("segments handled"), not a CC-coupled metric — the Local
        // fast-path returns Ok(true) so the count goes up the same way.
        //
        // URLs are wrapped in OSC 8 hyperlinks so modern terminals
        // (Windows Terminal, iTerm2, kitty, WezTerm, GNOME Terminal,
        // tmux ≥3.4) render them as clickable.  Older terminals strip
        // the escape sequences and show the bare URL text as before.
        let total_bytes: u64 = state.cameras.iter().map(|c| c.bytes_uploaded).sum();
        let data_str = format_bytes(total_bytes);
        let is_local = matches!(state.settings.mode, NodeMode::Local);
        let middle_text = if is_local {
            // Local mode: just the LAN URL.  Empty until the runner
            // resolves it (one tick at boot) — fall back to a
            // placeholder so the layout doesn't shift mid-frame.
            if state.settings.local_url.is_empty() {
                truncate("starting…", 36)
            } else {
                hyperlink(
                    &state.settings.local_url,
                    &truncate(&state.settings.local_url, 36),
                )
            }
        } else {
            // Connected mode: surface BOTH the local web-UI URL and
            // the Command Center URL.  The local URL is the more
            // actionable one (it's how the operator opens the
            // node-local Snapshots / Recordings tabs that don't exist
            // anywhere else), so it goes first.
            let cc_display = state
                .api_url
                .replace("https://", "")
                .replace("http://", "");
            let cc_link = hyperlink(&state.api_url, &truncate(&cc_display, 26));
            if state.settings.local_url.is_empty() {
                cc_link
            } else {
                let local_link = hyperlink(
                    &state.settings.local_url,
                    &truncate(&state.settings.local_url, 22),
                );
                format!("{} {} {}", local_link, "·".dimmed(), cc_link)
            }
        };
        // Mode badge: [LOCAL] in Local mode (purple-ish to stand out
        // against the cyan node id), otherwise the plan pill from CC.
        let mode_part = if is_local {
            format!(" {}", "[LOCAL]".bright_magenta().bold())
        } else {
            state
                .plan
                .as_deref()
                .map(|p| format!(" {}", plan_badge(p)))
                .unwrap_or_default()
        };
        let status_content = format!(
            "  Node: {}{}   │   {}   │   ↑ {} segs  {}   │   ⏱ {}",
            state.node_id.cyan().bold(),
            mode_part,
            middle_text.white(),
            state.total_segments.to_string().cyan(),
            format!("({})", data_str).dimmed(),
            state.uptime().white(),
        );
        out.push_str(&panel_row_str(&status_content, w));
        out.push('\n');

        if state.current_view == View::Settings {
            // ── Settings page ────────────────────────────────────────────────
            out.push_str(&section_header("SETTINGS", w));
            out.push('\n');

            let (_, term_h) = terminal::size().unwrap_or((80, 30));
            let content_rows = (term_h as usize).saturating_sub(5);
            let divider_w = w.saturating_sub(10); // inner divider width

            let s = &state.settings;
            let kw = 20; // key column width
            let mut lines: Vec<String> = Vec::new();

            // ── NODE section
            lines.push(String::new());
            lines.push(settings_divider("NODE", divider_w));
            lines.push(settings_kv("Node ID", &state.node_id, kw));
            lines.push(settings_kv("Name", &s.node_name, kw));
            lines.push(settings_kv("API URL",
                &state.api_url.replace("https://", "").replace("http://", ""), kw));
            lines.push(settings_kv("Heartbeat", &format!("{} s", s.heartbeat_interval), kw));

            // ── STORAGE section
            lines.push(String::new());
            lines.push(settings_divider("STORAGE", divider_w));
            lines.push(settings_kv("Path", &s.storage_path, kw));
            lines.push(settings_kv("Max Size", &format!("{} GB", s.max_size_gb), kw));

            // ── STREAMING section
            lines.push(String::new());
            lines.push(settings_divider("STREAMING", divider_w));
            lines.push(settings_kv("Segment", &format!("{} s", s.segment_duration), kw));
            lines.push(settings_kv("FPS", &s.fps.to_string(), kw));
            lines.push(format!("     {}   {}",
                pad_right(&"Encoder".white().to_string(), 7, kw),
                if s.encoder.is_empty() { "auto-detect".dimmed().to_string() }
                else { s.encoder.bright_green().to_string() }));
            lines.push(format!("     {}   {}",
                pad_right(&"HLS".white().to_string(), 3, kw),
                if s.hls_enabled { "enabled".bright_green().to_string() }
                else { "disabled".bright_red().to_string() }));

            // ── MOTION section
            lines.push(String::new());
            lines.push(settings_divider("MOTION", divider_w));
            lines.push(format!("     {}   {}",
                pad_right(&"Detection".white().to_string(), 9, kw),
                if s.motion_enabled { "enabled".bright_green().to_string() }
                else { "disabled".dimmed().to_string() }));
            lines.push(settings_kv("Sensitivity", &format!("{:.1}", s.motion_sensitivity), kw));
            lines.push(settings_kv("Cooldown", &format!("{} s", s.motion_cooldown), kw));

            // ── CAMERAS section
            lines.push(String::new());
            lines.push(settings_divider(
                &format!("CAMERAS  {}", format!("({})", state.cameras.len()).dimmed()), divider_w));
            for cam in &state.cameras {
                let status_str = if state.disabled_cameras.contains(&cam.camera_id) {
                    "suspended (plan)".yellow().bold().to_string()
                } else {
                    match &cam.status {
                        CameraStatus::Streaming => "streaming".bright_green().to_string(),
                        CameraStatus::Starting  => "starting".yellow().to_string(),
                        CameraStatus::Offline   => "offline".dimmed().to_string(),
                        CameraStatus::Error(e)  => truncate(e, 16).bright_red().to_string(),
                        CameraStatus::Restarting { attempt, .. } =>
                            format!("restarting ({})", attempt).yellow().to_string(),
                        CameraStatus::Failed { last_error } =>
                            format!("failed: {}", truncate(last_error, 10)).bright_red().to_string(),
                    }
                };
                lines.push(format!("     {}  {}  {}",
                    pad_right(&cam.name.white().to_string(), visible_len(&cam.name), kw),
                    pad_right(&cam.resolution.dimmed().to_string(), visible_len(&cam.resolution), 12),
                    status_str,
                ));
            }

            // ── ACTIONS section
            lines.push(String::new());
            lines.push(settings_divider("ACTIONS", divider_w));
            lines.push(settings_action("/set <key> <val>", "Change a setting"));
            lines.push(settings_action("/export-logs", "Save all logs to a file"));
            lines.push(settings_action(
                "/wipe",
                "Unpair from Command Center and erase all local data",
            ));
            lines.push(settings_action("/reauth", "Clear credentials and re-run setup"));
            lines.push(String::new());

            // Render settings content
            for line in &lines {
                out.push_str(&panel_row_str(line, w));
                out.push('\n');
            }

            // ── Command output panel (persistent, above footer) ─────────────
            // Same as the Main view's command output panel. Without this, any
            // output set by /set /wipe /reauth /export-logs while on the
            // settings page is invisible — the user types the command, it
            // runs, but they see no feedback (looks like "nothing happened").
            let cmd_output_rows = if state.command_output.is_empty() {
                0
            } else {
                state.command_output.len() + 1 // +1 for the divider bar
            };
            if !state.command_output.is_empty() {
                out.push_str(&format!(
                    "{}{}{}\x1B[K\n",
                    cyan_bold(ML),
                    cyan_bold(&H.repeat(w.saturating_sub(2))),
                    cyan_bold(MR),
                ));
                for line in &state.command_output {
                    let content = format!("  {}", line);
                    let truncated = truncate_ansi(&content, w.saturating_sub(4));
                    out.push_str(&panel_row_str(&truncated, w));
                    out.push('\n');
                }
            }

            // Pad remaining vertical space so the footer lands at the bottom.
            let used = lines.len() + cmd_output_rows;
            for _ in used..content_rows {
                out.push_str(&panel_row_str("", w));
                out.push('\n');
            }
        } else {
            // ── Main view: Cameras + Log ─────────────────────────────────────
            out.push_str(&section_header("CAMERAS", w));
            out.push('\n');

            if state.cameras.is_empty() {
                out.push_str(&panel_row_str(
                    &"  No cameras detected".dimmed().to_string(),
                    w,
                ));
                out.push('\n');
            } else {
                // Column headers
                let header = format!(
                    "  {}   {}   {}   {}   {}",
                    pad_right(&"CAMERA".dimmed().to_string(), 6, 28),
                    pad_right(&"RESOLUTION".dimmed().to_string(), 10, 12),
                    pad_right(&"CODEC".dimmed().to_string(), 5, 30),
                    pad_right(&"STATUS".dimmed().to_string(), 6, 14),
                    "SEGS".dimmed(),
                );
                out.push_str(&panel_row_str(&header, w));
                out.push('\n');

                for cam in &state.cameras {
                    // Plan-cap suspension overrides the pipeline status —
                    // even if FFmpeg is happily producing segments locally,
                    // the backend is rejecting every push with 402, so the
                    // user needs to see "suspended (plan)" in the table
                    // rather than "streaming".
                    let status_str = if state.disabled_cameras.contains(&cam.camera_id) {
                        "⚠ suspended (plan)".yellow().bold().to_string()
                    } else {
                        match &cam.status {
                            CameraStatus::Streaming => "● streaming".bright_green().bold().to_string(),
                            CameraStatus::Starting => "◌ starting…".yellow().to_string(),
                            CameraStatus::Offline => "○ offline".dimmed().to_string(),
                            CameraStatus::Error(e) => {
                                format!("✗ {}", truncate(e, 18)).bright_red().to_string()
                            }
                            CameraStatus::Restarting { attempt, .. } => {
                                format!("↻ restarting ({})", attempt).yellow().bold().to_string()
                            }
                            CameraStatus::Failed { last_error } => {
                                format!("✗ failed: {}", truncate(last_error, 12))
                                    .bright_red()
                                    .bold()
                                    .to_string()
                            }
                        }
                    };
                    let codec = if cam.video_codec.is_empty() {
                        "detecting…".dimmed().to_string()
                    } else {
                        format!("{} / {}", cam.video_codec.cyan(), cam.audio_codec.cyan())
                    };
                    let line = format!(
                        "  {}   {}   {}   {}   {}",
                        pad_right(
                            &cam.name.white().bold().to_string(),
                            visible_len(&cam.name.white().bold().to_string()),
                            28,
                        ),
                        pad_right(
                            &cam.resolution.dimmed().to_string(),
                            visible_len(&cam.resolution),
                            12,
                        ),
                        pad_right(&codec, visible_len(&codec), 30),
                        pad_right(&status_str, visible_len(&status_str), 14),
                        cam.segments_uploaded.to_string().cyan(),
                    );
                    out.push_str(&panel_row_str(&line, w));
                    out.push('\n');
                }
            }

            // ── Log section ──────────────────────────────────────────────────
            out.push_str(&section_header("LOG", w));
            out.push('\n');

            // How many log lines fit?
            let cam_rows = state.cameras.len().max(1) + 1;
            let cmd_output_rows = if state.command_output.is_empty() {
                0
            } else {
                state.command_output.len() + 1
            };
            let reserved_rows = 7 + cam_rows + cmd_output_rows;
            let (_, term_h) = terminal::size().unwrap_or((80, 30));
            let log_rows = (term_h as usize).saturating_sub(reserved_rows).max(3);

            let visible_logs: Vec<&LogEntry> = state
                .logs
                .iter()
                .rev()
                .take(log_rows)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();

            // Pad with blank lines if fewer logs than space
            let blank_rows = log_rows.saturating_sub(visible_logs.len());
            for _ in 0..blank_rows {
                out.push_str(&panel_row_str("", w));
                out.push('\n');
            }

            for entry in &visible_logs {
                let (icon, colored_msg) = match entry.level {
                    LogLevel::Info => ("✓", entry.message.white().to_string()),
                    LogLevel::Warn => ("⚠", entry.message.yellow().to_string()),
                    LogLevel::Error => ("✗", entry.message.bright_red().to_string()),
                    LogLevel::Debug => ("·", entry.message.dimmed().to_string()),
                };
                let icon_colored = match entry.level {
                    LogLevel::Info => icon.bright_green().to_string(),
                    LogLevel::Warn => icon.yellow().to_string(),
                    LogLevel::Error => icon.bright_red().to_string(),
                    LogLevel::Debug => icon.dimmed().to_string(),
                };
                let line = format!(
                    "  {}  {}  {}",
                    entry.time.dimmed(),
                    icon_colored,
                    colored_msg,
                );
                let truncated = truncate_ansi(&line, w.saturating_sub(4));
                out.push_str(&panel_row_str(&truncated, w));
                out.push('\n');
            }

            // ── Command output panel (persistent, above footer) ─────────────
            if !state.command_output.is_empty() {
                out.push_str(&format!(
                    "{}{}{}\x1B[K\n",
                    cyan_bold(ML),
                    cyan_bold(&H.repeat(w.saturating_sub(2))),
                    cyan_bold(MR),
                ));
                for line in &state.command_output {
                    let content = format!("  {}", line);
                    let truncated = truncate_ansi(&content, w.saturating_sub(4));
                    out.push_str(&panel_row_str(&truncated, w));
                    out.push('\n');
                }
            }
        }

        // ── Footer ───────────────────────────────────────────────────────────
        out.push_str(&format!(
            "{}{}{}\x1B[K",
            cyan_bold(BL),
            cyan_bold(&H.repeat(w.saturating_sub(2))),
            cyan_bold(BR),
        ));

        // Input bar below the box
        if state.input_text.is_empty() {
            let hint = if state.current_view == View::Settings {
                "Esc to go back"
            } else {
                "Type / for commands"
            };
            out.push_str(&format!(
                "\n  {}  {}\x1B[K",
                ">".cyan().bold(),
                hint.dimmed(),
            ));
        } else {
            out.push_str(&format!(
                "\n  {}  {}\x1B[K",
                ">".cyan().bold(),
                state.input_text,
            ));
        }

        // Clear any remaining lines below the TUI from previous frames
        out.push_str("\x1B[J");

        // Save cursor position before dropping lock
        let cursor_col = 5 + state.input_cursor;

        // Drop lock before writing to stdout
        drop(state);

        // Replace \n with \r\n for raw mode compatibility
        let out = out.replace('\n', "\r\n");

        // Write frame, then position cursor at input bar and show it
        print!("{}\r\x1B[{}C\x1B[?25h", out, cursor_col);
        io::stdout().flush().ok();
    }

    /// Export all logs to a text file.
    /// Pulls from the SQLite database for a complete history, falling back to
    /// the in-memory buffer if the DB is unavailable.
    pub fn export_logs(&self, path: &std::path::Path) {
        let state = match self.0.lock() {
            Ok(s) => s,
            Err(_) => return,
        };

        let mut lines = Vec::new();
        lines.push("SourceBox Sentry CloudNode — Log Export".to_string());
        lines.push(format!("Node: {}  |  API: {}", state.node_id, state.api_url));
        lines.push(format!("Total segments: {}  |  Uptime: {}", state.total_segments, state.uptime()));
        lines.push(String::new());

        // Try to load the full log history from the database
        let db_logs = state.db.as_ref().and_then(|db| db.load_recent_logs(10_000).ok());

        if let Some(rows) = db_logs {
            for (timestamp, level, message) in &rows {
                lines.push(format!("{} [{}] {}", timestamp, level, message));
            }
        } else {
            // Fallback: export in-memory buffer only
            for entry in &state.logs {
                let level = match entry.level {
                    LogLevel::Info  => "INFO ",
                    LogLevel::Warn  => "WARN ",
                    LogLevel::Error => "ERROR",
                    LogLevel::Debug => "DEBUG",
                };
                lines.push(format!("{} [{}] {}", entry.time, level, entry.message));
            }
        }

        drop(state);

        if let Err(e) = std::fs::write(path, lines.join("\n")) {
            eprintln!("Failed to export logs: {}", e);
        }
    }
}

// ─── Rendering helpers ────────────────────────────────────────────────────────

pub(super) fn term_width() -> usize {
    terminal::size().map(|(w, _)| w).unwrap_or(80).max(60) as usize
}

pub(super) fn cyan_bold(s: &str) -> String {
    s.cyan().bold().to_string()
}

/// Settings page: thin divider with a section label.
pub(super) fn settings_divider(label: &str, fill_w: usize) -> String {
    let label_vis = visible_len(label);
    let fill = fill_w.saturating_sub(label_vis + 2);
    format!("   {} {}",
        label.cyan().bold(),
        "\u{2500}".repeat(fill).dimmed())
}

/// Settings page: key-value row.
pub(super) fn settings_kv(key: &str, value: &str, key_width: usize) -> String {
    format!("     {}   {}",
        pad_right(&key.white().to_string(), visible_len(key), key_width),
        value.cyan())
}

/// Settings page: action row.
pub(super) fn settings_action(cmd: &str, desc: &str) -> String {
    format!("     {}   {}",
        pad_right(&cmd.cyan().bold().to_string(), visible_len(cmd), 16),
        desc.dimmed())
}

/// Render a colored pill badge for a subscription plan, matching the
/// `[ LABEL ]` pill style of the setup-wizard progress bar.  Purely
/// informational — see the doc comment on `api::types::RegisterResponse::plan`
/// for why we don't enforce anything on this string.
pub(super) fn plan_badge(plan: &str) -> String {
    // The backend strips the `_org` suffix via wire_plan_slug, so the values we
    // see here are "free" / "pro" / "pro_plus". "business" is kept as a
    // transitional alias so a node running against a just-upgraded backend
    // still colours its pill correctly. For the upper-case display we swap
    // underscores to spaces so the pill reads `[ PRO PLUS ]`, not `[ PRO_PLUS ]`.
    let trimmed = plan.trim();
    let display = trimmed.to_uppercase().replace('_', " ");
    let pill = format!("[ {} ]", display);
    match trimmed.to_lowercase().as_str() {
        "pro" => pill.cyan().bold().to_string(),
        "pro_plus" | "business" => pill.magenta().bold().to_string(),
        "free" => pill.white().dimmed().to_string(),
        // Unknown plan strings from the backend still render (dimmed) so a
        // future `"enterprise"` tier shows up in the UI before we ship a
        // node update to colour it specially.
        _ => pill.white().dimmed().to_string(),
    }
}

pub(super) fn section_header(label: &str, w: usize) -> String {
    let label_str = format!(" {} ", label);
    let label_len = label_str.chars().count();
    let fill = w.saturating_sub(2 + label_len);
    format!(
        "{}{}{}{}\x1B[K",
        cyan_bold(ML),
        cyan_bold(&label_str),
        cyan_bold(&H.repeat(fill)),
        cyan_bold(MR),
    )
}

pub(super) fn panel_row_str(content: &str, w: usize) -> String {
    // Border: ║ + space + content + clear-to-EOL + jump-to-col-w + ║
    let inner = w.saturating_sub(4);
    let fitted = truncate_ansi(content, inner);
    // Clear the line FIRST, then use cursor positioning to place
    // the right border. This avoids \x1B[K erasing the border
    // (which happens on terminals with deferred line wrap).
    format!(
        "{} {}\x1B[K\x1B[{}G{}",
        cyan_bold(V),
        fitted,
        w,
        cyan_bold(V),
    )
}

/// Visible character count (strips ANSI escape sequences).
/// Handles all CSI sequences (not just SGR/color codes).
pub(super) fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1B' {
            // Skip the escape sequence
            match chars.next() {
                Some('[') => {
                    // CSI sequence — consume until a letter (final byte 0x40–0x7E)
                    for nc in chars.by_ref() {
                        if nc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC sequence — consume until ST (BEL or ESC\)
                    for nc in chars.by_ref() {
                        if nc == '\x07' || nc == '\x1B' {
                            break;
                        }
                    }
                }
                _ => {} // other escape — skip one char
            }
        } else {
            len += 1;
        }
    }
    len
}

/// Pad a string to `width` visible characters.
pub(super) fn pad_right(s: &str, visible: usize, width: usize) -> String {
    if visible >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - visible))
    }
}

/// Wrap `display` text in an OSC 8 hyperlink targeting `url`.
///
/// Modern terminals (Windows Terminal, iTerm2, kitty, WezTerm,
/// gnome-terminal, konsole, foot, tmux ≥3.4 with passthrough) render
/// this as a clickable link.  Terminals that don't understand OSC 8
/// strip the escape sequences and just show `display` as plain text —
/// the exact graceful fallback we want.
///
/// Wire format: `ESC ] 8 ; ; <url> ST <display> ESC ] 8 ; ; ST`,
/// where ST (string terminator) is BEL (`\x07`) or `ESC \`.  We use
/// `ESC \` (the safer of the two — BEL sometimes triggers bell sounds
/// in older terminals).
pub(super) fn hyperlink(url: &str, display: &str) -> String {
    format!("\x1B]8;;{}\x1B\\{}\x1B]8;;\x1B\\", url, display)
}

/// Format a byte count as a human-readable string.
pub(super) fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{:.1} GB", bytes as f64 / 1_073_741_824.0)
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1_024 {
        format!("{:.1} KB", bytes as f64 / 1_024.0)
    } else {
        format!("{} B", bytes)
    }
}

/// Truncate plain text to `max` chars.
pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

/// Truncate a string with ANSI codes to `max` visible characters.
///
/// Handles both CSI sequences (`\x1B[…<letter>`) and OSC sequences
/// (`\x1B]…<ST>` where ST is `\x07` BEL or `\x1B\\`).  OSC 8
/// hyperlinks land in the second branch — without it the URL bytes
/// would be counted as visible characters and break panel width
/// math + truncation.
pub(super) fn truncate_ansi(s: &str, max: usize) -> String {
    let mut result = String::new();
    let mut visible = 0;
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1B' {
            result.push(c);
            match chars.next() {
                Some('[') => {
                    // CSI — consume until a letter (final byte 0x40–0x7E).
                    result.push('[');
                    for nc in chars.by_ref() {
                        result.push(nc);
                        if nc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC — consume until ST (BEL `\x07` or ESC\ `\x1B\\`).
                    // OSC 8 hyperlinks use this form; if we tracked URL
                    // bytes as visible, a single link blew the panel.
                    result.push(']');
                    while let Some(nc) = chars.next() {
                        result.push(nc);
                        if nc == '\x07' {
                            break;
                        }
                        if nc == '\x1B' {
                            // ESC \ — consume the trailing backslash too.
                            if let Some(bs) = chars.next() {
                                result.push(bs);
                            }
                            break;
                        }
                    }
                }
                Some(other) => {
                    result.push(other);
                }
                None => break,
            }
        } else if visible < max {
            result.push(c);
            visible += 1;
        } else {
            // Truncated — close any open color sequences
            result.push_str("\x1B[0m");
            break;
        }
    }

    result
}
