//! Slash-command dispatch + the input event loop.
//!
//! This is what makes the dashboard interactive. `run_render_loop` is the
//! main blocking entry point; it owns the crossterm raw-mode lifecycle and
//! the keystroke → input-buffer → command pipeline. `execute_command` is
//! the slash-command dispatcher.
//!
//! The destructive-command confirm flow (`/wipe`, `/reauth` need a
//! confirmation press within 30s) and the tests for it live here too — the
//! tests reach into private methods (`check_or_arm_confirm`,
//! `clear_pending_confirm`) and the `pending_confirm` state field, which
//! is why this module keeps them in scope rather than re-exposing them
//! through `Dashboard`'s public API.

use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Local;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal,
};

use super::handle::Dashboard;
use super::render::format_bytes;
use super::state::CONFIRM_TIMEOUT;
use super::types::View;

impl Dashboard {
    /// Start the render loop in the current thread. Redraws every `interval`.
    /// Enables raw mode for character-by-character input. Blocks until `stop`.
    pub fn run_render_loop(&self, interval: Duration, stop: Arc<std::sync::atomic::AtomicBool>) {
        let _ = terminal::enable_raw_mode();

        // Clear screen initially
        print!("\x1B[2J\x1B[H");
        io::stdout().flush().ok();

        let mut input = String::new();
        // Cursor position in CHARS, not bytes.  Every edit below converts
        // through `byte_at` before touching the String — String::insert/
        // remove take byte indices and PANIC on a non-char-boundary, so a
        // byte-indexed cursor advanced 1-per-keystroke corrupts (then
        // kills) this thread on the first keypress after any multi-byte
        // char (é, em dash, smart quote — terminal paste delivers these
        // as ordinary Char events).  This thread dying leaves raw mode
        // stuck on with Ctrl+C dead, so the panic was effectively a
        // terminal lock-up.  Char-counting is also what the renderer
        // wants: render.rs uses input_cursor as a terminal column.
        let mut cursor_pos: usize = 0;
        // Char-index → byte-offset for String::insert/remove.
        fn byte_at(s: &str, char_idx: usize) -> usize {
            s.char_indices()
                .nth(char_idx)
                .map(|(b, _)| b)
                .unwrap_or(s.len())
        }
        let mut history: Vec<String> = Vec::new();
        let mut history_idx: Option<usize> = None;

        while !stop.load(std::sync::atomic::Ordering::Relaxed) {
            // Push input state for rendering
            if let Ok(mut s) = self.0.lock() {
                s.input_text.clone_from(&input);
                s.input_cursor = cursor_pos;
            }

            self.render();

            // Poll for keyboard events (replaces thread::sleep)
            if event::poll(interval).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    // Ignore key release events (Windows sends both press + release)
                    if key.kind == KeyEventKind::Release {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            stop.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        KeyCode::Char(c) => {
                            input.insert(byte_at(&input, cursor_pos), c);
                            cursor_pos += 1;
                            history_idx = None;
                        }
                        KeyCode::Backspace => {
                            if cursor_pos > 0 {
                                cursor_pos -= 1;
                                input.remove(byte_at(&input, cursor_pos));
                            }
                        }
                        KeyCode::Delete => {
                            if cursor_pos < input.chars().count() {
                                input.remove(byte_at(&input, cursor_pos));
                            }
                        }
                        KeyCode::Left => {
                            cursor_pos = cursor_pos.saturating_sub(1);
                        }
                        KeyCode::Right => {
                            if cursor_pos < input.chars().count() {
                                cursor_pos += 1;
                            }
                        }
                        KeyCode::Home => cursor_pos = 0,
                        KeyCode::End => cursor_pos = input.chars().count(),
                        KeyCode::Up => {
                            if !history.is_empty() {
                                let idx = match history_idx {
                                    Some(i) => i.saturating_sub(1),
                                    None => history.len() - 1,
                                };
                                input = history[idx].clone();
                                cursor_pos = input.chars().count();
                                history_idx = Some(idx);
                            }
                        }
                        KeyCode::Down => {
                            if let Some(idx) = history_idx {
                                if idx + 1 < history.len() {
                                    let new_idx = idx + 1;
                                    input = history[new_idx].clone();
                                    cursor_pos = input.chars().count();
                                    history_idx = Some(new_idx);
                                } else {
                                    input.clear();
                                    cursor_pos = 0;
                                    history_idx = None;
                                }
                            }
                        }
                        KeyCode::Enter => {
                            if !input.is_empty() {
                                let cmd = input.clone();
                                history.push(cmd.clone());
                                history_idx = None;
                                input.clear();
                                cursor_pos = 0;
                                self.execute_command(&cmd, &stop);
                            }
                        }
                        KeyCode::Esc => {
                            input.clear();
                            cursor_pos = 0;
                            history_idx = None;
                            // Navigate back from settings, or clear output on main
                            if let Ok(mut s) = self.0.lock() {
                                if s.current_view != View::Main {
                                    s.current_view = View::Main;
                                } else {
                                    s.command_output.clear();
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Cleanup
        let _ = terminal::disable_raw_mode();

        // Clear input for final render
        if let Ok(mut s) = self.0.lock() {
            s.input_text.clear();
            s.input_cursor = 0;
        }
        self.render();

        print!("\r\n");
        io::stdout().flush().ok();
    }

    /// Set the persistent command output panel content.
    fn set_output(&self, lines: Vec<String>) {
        if let Ok(mut s) = self.0.lock() {
            s.command_output = lines;
        }
    }

    /// Clear the command output panel.
    fn clear_output(&self) {
        if let Ok(mut s) = self.0.lock() {
            s.command_output.clear();
        }
    }

    /// Check whether a destructive command should run, using either
    /// an explicit `/<cmd> confirm` argument OR a repeat *bare* press
    /// of the same command within [`CONFIRM_TIMEOUT`] of the first.
    ///
    /// - `explicit_arg` — true iff the user typed `/<cmd> confirm`.
    /// - `bare`        — true iff the user typed `/<cmd>` with no args.
    ///
    /// Only bare repeats count as confirmation.  A call with unrecognized
    /// arguments (e.g. `/wipe dry-run`) re-arms the prompt but does not
    /// consume a pending confirmation, so a previous `/wipe` doesn't get
    /// turned into destruction by a typo.
    ///
    /// Returns `true` if the command should proceed now; in that case
    /// the pending-confirm slot is cleared.  Returns `false` if the
    /// caller should show the warning prompt; in that case the slot is
    /// (re-)armed so the next bare press of the same command confirms.
    fn check_or_arm_confirm(&self, cmd: &str, explicit_arg: bool, bare: bool) -> bool {
        let Ok(mut s) = self.0.lock() else {
            // Lock poisoned — fail closed (require re-arming).
            return false;
        };

        let repeat_confirm = bare
            && matches!(
                &s.pending_confirm,
                Some((pending_cmd, armed_at))
                    if pending_cmd == cmd && armed_at.elapsed() < CONFIRM_TIMEOUT
            );

        if explicit_arg || repeat_confirm {
            s.pending_confirm = None;
            true
        } else {
            s.pending_confirm = Some((cmd.to_string(), Instant::now()));
            false
        }
    }

    /// Discard any pending destructive-command confirmation.  Called
    /// whenever the user dispatches an unrelated command, so they can't
    /// accidentally confirm a stale `/wipe` hours later.
    fn clear_pending_confirm(&self) {
        if let Ok(mut s) = self.0.lock() {
            s.pending_confirm = None;
        }
    }

    /// Parse and execute a slash command from the input bar.
    fn execute_command(&self, input: &str, stop: &Arc<std::sync::atomic::AtomicBool>) {
        let input = input.trim();

        if !input.starts_with('/') {
            self.set_output(vec!["Commands start with /  — try /help".to_string()]);
            return;
        }

        let parts: Vec<&str> = input[1..].split_whitespace().collect();
        let cmd = parts.first().copied().unwrap_or("");
        let args = if parts.len() > 1 { &parts[1..] } else { &parts[..0] };

        // Any command other than the pending destructive one invalidates
        // the armed confirmation.  The handlers for /wipe and /reauth
        // below re-arm or consume it as appropriate.
        if !matches!(cmd, "wipe" | "reauth") {
            self.clear_pending_confirm();
        }

        // Check current view for settings-only commands
        let on_settings = self.0.lock().map(|s| s.current_view == View::Settings).unwrap_or(false);

        match cmd {
            "quit" | "exit" | "q" => {
                self.clear_output();
                self.log_warn("Shutting down…");
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            "" | "help" | "?" => {
                if on_settings {
                    self.set_output(vec![
                        "Settings commands:".to_string(),
                        "  /set <key> <value>   Change a setting".to_string(),
                        "  /export-logs         Save logs to file".to_string(),
                        "  /wipe                Unpair & erase all data".to_string(),
                        "  /reauth              Reset credentials".to_string(),
                        "  /back                Return to dashboard".to_string(),
                        "  /quit                Stop the node".to_string(),
                        String::new(),
                        "Settings keys: fps, encoder, segment, bitrate,".to_string(),
                        "  motion (on/off), sensitivity, cooldown".to_string(),
                    ]);
                } else {
                    self.set_output(vec![
                        "Available commands:".to_string(),
                        "  /settings      Open settings page".to_string(),
                        "  /status        Show node status".to_string(),
                        "  /clear         Clear the log".to_string(),
                        "  /quit          Stop the node".to_string(),
                    ]);
                }
            }
            "settings" => {
                if let Ok(mut s) = self.0.lock() {
                    s.current_view = View::Settings;
                    s.command_output.clear();
                }
            }
            "back" => {
                if let Ok(mut s) = self.0.lock() {
                    s.current_view = View::Main;
                    s.command_output.clear();
                }
            }
            "clear" | "cls" => {
                if let Ok(mut s) = self.0.lock() {
                    s.logs.clear();
                    s.command_output.clear();
                }
            }
            "status" => {
                let info = if let Ok(s) = self.0.lock() {
                    let total_bytes: u64 = s.cameras.iter().map(|c| c.bytes_uploaded).sum();
                    Some((
                        s.cameras.len(),
                        s.total_segments,
                        format_bytes(total_bytes),
                        s.uptime(),
                    ))
                } else {
                    None
                };
                if let Some((cams, segs, bytes, uptime)) = info {
                    self.set_output(vec![
                        format!("Cameras:  {}", cams),
                        format!("Segments: {}", segs),
                        format!("Uploaded: {}", bytes),
                        format!("Uptime:   {}", uptime),
                    ]);
                }
            }
            "set" if on_settings => {
                if args.len() < 2 {
                    self.set_output(vec![
                        "Usage: /set <key> <value>".to_string(),
                        String::new(),
                        "Keys:".to_string(),
                        "  fps          Frames per second (1-60)".to_string(),
                        "  encoder      Video encoder (libx264, h264_nvenc, …)".to_string(),
                        "  segment      Segment duration in seconds".to_string(),
                        "  bitrate      Encoding bitrate (e.g. 2500k)".to_string(),
                        "  motion       on / off".to_string(),
                        "  sensitivity  Motion threshold 0.0-1.0".to_string(),
                        "  cooldown     Motion cooldown seconds".to_string(),
                    ]);
                } else {
                    let key = args[0];
                    let val = args[1..].join(" ");
                    let (db_key, display_val, ok) = match key {
                        "fps" => {
                            match val.parse::<u32>() {
                                Ok(v) if (1..=60).contains(&v) => ("fps", val.clone(), true),
                                _ => ("", String::new(), false),
                            }
                        }
                        "encoder" => ("encoder", val.clone(), true),
                        "segment" => {
                            match val.parse::<u32>() {
                                Ok(v) if (1..=30).contains(&v) => ("segment_duration", val.clone(), true),
                                _ => ("", String::new(), false),
                            }
                        }
                        "bitrate" => ("bitrate", val.clone(), true),
                        "motion" => {
                            let enabled = matches!(val.as_str(), "on" | "true" | "1" | "yes");
                            let disabled = matches!(val.as_str(), "off" | "false" | "0" | "no");
                            if enabled || disabled {
                                ("motion_enabled", (if enabled { "true" } else { "false" }).to_string(), true)
                            } else {
                                ("", String::new(), false)
                            }
                        }
                        "sensitivity" => {
                            match val.parse::<f64>() {
                                Ok(v) if (0.0..=1.0).contains(&v) => ("motion_sensitivity", val.clone(), true),
                                _ => ("", String::new(), false),
                            }
                        }
                        "cooldown" => {
                            match val.parse::<u64>() {
                                Ok(_) => ("motion_cooldown", val.clone(), true),
                                _ => ("", String::new(), false),
                            }
                        }
                        _ => {
                            self.set_output(vec![
                                format!("Unknown setting: {}", key),
                                "Type /set for a list of keys.".to_string(),
                            ]);
                            return;
                        }
                    };
                    if !ok {
                        self.set_output(vec![format!("Invalid value for {}: {}", key, val)]);
                        return;
                    }
                    let saved = if let Ok(s) = self.0.lock() {
                        if let Some(ref db) = s.db {
                            db.set_config(db_key, &display_val).is_ok()
                        } else { false }
                    } else { false };
                    if saved {
                        // Update the in-memory SettingsInfo so it refreshes immediately
                        if let Ok(mut s) = self.0.lock() {
                            match key {
                                "fps" => s.settings.fps = display_val.parse().unwrap_or(s.settings.fps),
                                "encoder" => s.settings.encoder = display_val.clone(),
                                "segment" => s.settings.segment_duration = display_val.parse().unwrap_or(s.settings.segment_duration),
                                "motion" => s.settings.motion_enabled = display_val == "true",
                                "sensitivity" => s.settings.motion_sensitivity = display_val.parse().unwrap_or(s.settings.motion_sensitivity),
                                "cooldown" => s.settings.motion_cooldown = display_val.parse().unwrap_or(s.settings.motion_cooldown),
                                _ => {}
                            }
                        }
                        self.set_output(vec![
                            format!("Set {} = {} (takes effect on next segment)", key, display_val),
                        ]);
                        self.log_info(format!("Setting changed: {} = {}", key, display_val));
                    } else {
                        self.set_output(vec!["Failed to save setting.".to_string()]);
                    }
                }
            }
            "export-logs" if on_settings => {
                let timestamp = Local::now().format("%Y-%m-%d_%H%M%S");
                let filename = format!("sourcebox-sentry-logs-{}.txt", timestamp);
                let path = std::path::PathBuf::from(&filename);
                self.export_logs(&path);
                self.set_output(vec![
                    format!("Logs exported to {}", filename),
                ]);
                self.log_info(format!("Logs exported to {}", filename));
            }
            "wipe" if on_settings => {
                let explicit_arg = args.first().copied() == Some("confirm");
                let bare = args.is_empty();
                let confirm = self.check_or_arm_confirm("wipe", explicit_arg, bare);
                if !confirm {
                    self.set_output(vec![
                        "This will permanently delete ALL data and unpair from Command Center:"
                            .to_string(),
                        "  - Tell the backend to delete this node's record".to_string(),
                        "  - Local snapshots, recordings, config".to_string(),
                        "  - HLS segment cache".to_string(),
                        String::new(),
                        "The node will shut down. Run setup again with a NEW".to_string(),
                        "node ID / API key from Command Center to re-pair.".to_string(),
                        String::new(),
                        "Press /wipe again within 30s (or type /wipe confirm) to proceed."
                            .to_string(),
                    ]);
                } else {
                    // Snapshot what we need out of the lock before doing
                    // anything blocking — we can't hold the Mutex across
                    // the tokio Runtime::block_on call below.
                    let (api_client, db, hls_dir) = match self.0.lock() {
                        Ok(s) => (s.api_client.clone(), s.db.clone(), s.hls_dir.clone()),
                        Err(_) => {
                            self.set_output(vec!["Wipe failed — state lock poisoned.".to_string()]);
                            return;
                        }
                    };

                    // ── Step 1: tell the backend to delete our node record ──
                    // Done first so a successful unpair is logged *before*
                    // we erase the credentials we'd need to retry.
                    // Failure is non-fatal: the operator already confirmed
                    // the destructive action, so we proceed with the local
                    // wipe either way and surface the outcome.
                    let backend_outcome: Result<(), String> = if let Some(client) = api_client {
                        // Dashboard runs on its own std::thread, not a tokio
                        // task, so spinning up a throwaway current-thread
                        // runtime here is safe and cheap.
                        match tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                        {
                            Ok(rt) => rt
                                .block_on(client.decommission())
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(format!("runtime init failed: {}", e)),
                        }
                    } else {
                        // Test-mode / run_once path — no client to call.
                        // Treat as "skipped" rather than "failed" in the UI.
                        Err("no API client configured".to_string())
                    };

                    // ── Step 2: local wipe (always runs) ───────────────────
                    let mut local_ok = true;
                    if let Some(ref db) = db {
                        if let Err(e) = db.wipe_all() {
                            self.log_error(format!("DB wipe failed: {}", e));
                            local_ok = false;
                        }
                    }
                    if let Some(ref hls) = hls_dir {
                        if hls.exists() {
                            let _ = std::fs::remove_dir_all(hls);
                        }
                    }

                    // ── Step 3: report and shut down ──────────────────────
                    if local_ok {
                        let mut output = Vec::new();
                        match &backend_outcome {
                            Ok(()) => {
                                output.push("Backend unpaired ✓".to_string());
                                self.log_warn("Node decommissioned on backend");
                            }
                            Err(e) => {
                                output.push(format!("Backend unpair failed: {}", e));
                                output.push(
                                    "  (node record may still exist in Command Center — "
                                        .to_string(),
                                );
                                output.push("   delete it manually from the dashboard)".to_string());
                                self.log_warn(format!("Backend decommission failed: {}", e));
                            }
                        }
                        output.push("All local data erased. Shutting down…".to_string());
                        output.push("Run setup again to pair a new node.".to_string());
                        self.set_output(output);
                        self.log_warn("Data wiped — shutting down");
                        stop.store(true, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        self.set_output(vec!["Wipe failed — check logs.".to_string()]);
                    }
                }
            }
            "reauth" if on_settings => {
                let explicit_arg = args.first().copied() == Some("confirm");
                let bare = args.is_empty();
                let confirm = self.check_or_arm_confirm("reauth", explicit_arg, bare);
                if !confirm {
                    self.set_output(vec![
                        "This will clear your credentials and stop the node.".to_string(),
                        "You will need to run setup again with new credentials.".to_string(),
                        String::new(),
                        "Press /reauth again within 30s (or type /reauth confirm) to proceed."
                            .to_string(),
                    ]);
                } else {
                    if let Ok(s) = self.0.lock() {
                        if let Some(ref db) = s.db {
                            // Delete the config rows — can't use set_config("api_key", "")
                            // because api_key is stored encrypted and loading would fail
                            // trying to decrypt an empty plaintext string.
                            let _ = db.delete_config("node_id");
                            let _ = db.delete_config("api_key");
                        }
                    }
                    self.set_output(vec![
                        "Credentials cleared. Shutting down…".to_string(),
                        "Run: sourcebox-sentry-cloudnode setup".to_string(),
                    ]);
                    self.log_warn("Credentials cleared — shutting down");
                    stop.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            }
            // Settings-only commands used from main view
            "wipe" | "reauth" | "export-logs" => {
                self.set_output(vec![
                    format!("/{} is only available on the settings page.", cmd),
                    "Type /settings to open it.".to_string(),
                ]);
            }
            _ => {
                self.set_output(vec![
                    format!("Unknown command: /{} — type / for help", cmd),
                ]);
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fresh Dashboard with no DB / API / HLS dir attached —
    /// enough to exercise the pure-state confirm helpers.
    fn fresh() -> Dashboard {
        Dashboard::new("test-node", "http://test")
    }

    fn pending_cmd(dash: &Dashboard) -> Option<String> {
        dash.0
            .lock()
            .ok()
            .and_then(|s| s.pending_confirm.as_ref().map(|(c, _)| c.clone()))
    }

    #[test]
    fn first_bare_press_arms_but_does_not_confirm() {
        let d = fresh();
        let confirmed = d.check_or_arm_confirm("wipe", /*explicit*/ false, /*bare*/ true);
        assert!(!confirmed, "first press must not proceed");
        assert_eq!(pending_cmd(&d).as_deref(), Some("wipe"));
    }

    #[test]
    fn second_bare_press_within_timeout_confirms() {
        let d = fresh();
        assert!(!d.check_or_arm_confirm("wipe", false, true));
        let confirmed = d.check_or_arm_confirm("wipe", false, true);
        assert!(confirmed, "second bare press must confirm");
        assert_eq!(pending_cmd(&d), None, "pending cleared after confirm");
    }

    #[test]
    fn explicit_confirm_arg_always_proceeds() {
        let d = fresh();
        let confirmed = d.check_or_arm_confirm("wipe", /*explicit*/ true, /*bare*/ false);
        assert!(confirmed);
        assert_eq!(pending_cmd(&d), None);
    }

    #[test]
    fn second_press_with_unknown_arg_does_not_confirm() {
        // /wipe then /wipe dry-run should NOT wipe — only bare repeat counts.
        let d = fresh();
        assert!(!d.check_or_arm_confirm("wipe", false, true)); // arm
        let confirmed = d.check_or_arm_confirm("wipe", false, false); // non-bare
        assert!(!confirmed, "non-bare repeat must not confirm");
        // And it re-arms rather than leaving stale state.
        assert_eq!(pending_cmd(&d).as_deref(), Some("wipe"));
    }

    #[test]
    fn pending_for_different_command_does_not_cross_confirm() {
        // Arming /wipe must not let a bare /reauth sneak through.
        let d = fresh();
        assert!(!d.check_or_arm_confirm("wipe", false, true));
        let confirmed = d.check_or_arm_confirm("reauth", false, true);
        assert!(!confirmed, "different pending cmd must not confirm");
        assert_eq!(pending_cmd(&d).as_deref(), Some("reauth"));
    }

    #[test]
    fn clear_pending_confirm_drops_armed_state() {
        let d = fresh();
        assert!(!d.check_or_arm_confirm("wipe", false, true));
        d.clear_pending_confirm();
        assert_eq!(pending_cmd(&d), None);
        // After clear, a fresh bare press must re-arm, not confirm.
        let confirmed = d.check_or_arm_confirm("wipe", false, true);
        assert!(!confirmed);
    }

    #[test]
    fn expired_pending_requires_rearming() {
        // Simulate a stale arming older than CONFIRM_TIMEOUT by stuffing
        // an `Instant` from far in the past into the slot directly.
        let d = fresh();
        {
            let mut s = d.0.lock().unwrap();
            let old = Instant::now()
                .checked_sub(CONFIRM_TIMEOUT + Duration::from_secs(1))
                .expect("system clock must support subtraction");
            s.pending_confirm = Some(("wipe".to_string(), old));
        }
        let confirmed = d.check_or_arm_confirm("wipe", false, true);
        assert!(!confirmed, "expired pending must require re-arming");
        // And the slot should be re-armed with a fresh timestamp.
        assert_eq!(pending_cmd(&d).as_deref(), Some("wipe"));
    }
}
