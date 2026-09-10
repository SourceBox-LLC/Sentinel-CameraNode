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
//! Terminal UI panel system
//!
//! Provides full-width bordered panels, pill progress bars, and layout
//! primitives that adapt to the terminal width — similar to Claude Code's UI.

use colored::{Color, Colorize};
use crossterm::terminal;
use std::io::{self, Write};

/// Minimum terminal width before we fall back to fixed 72-char layout.
const MIN_WIDTH: u16 = 60;

/// Get current terminal width, clamped to a sensible range.
pub fn term_width() -> usize {
    terminal::size()
        .map(|(w, _)| w)
        .unwrap_or(80)
        .max(MIN_WIDTH) as usize
}

// ─── Double-line box characters ────────────────────────────────────────────
const TL: &str = "╔";
const TR: &str = "╗";
const BL: &str = "╚";
const BR: &str = "╝";
const H: &str = "═";
const V: &str = "║";
const ML: &str = "╠"; // mid-left T
const MR: &str = "╣"; // mid-right T

// ─── Single-line box characters for inner dividers ─────────────────────────
const SH: &str = "─";
const SML: &str = "├";
const SMR: &str = "┤";

/// Draw a full-width double-border panel with an optional title (cyan border).
///
/// ```text
/// ╔══ ▸ STEP 1 / 5 — PREREQUISITES ═══════════════════════════════════════╗
/// ║                                                                        ║
/// ...content...
/// ╚════════════════════════════════════════════════════════════════════════╝
/// ```
pub fn panel_top(title: &str) {
    panel_top_color(title, Color::Cyan);
}

/// Draw a panel top with an explicit border color.
pub fn panel_top_color(title: &str, color: Color) {
    let w = term_width();
    // The title segment: " ▸ TITLE " padded with spaces
    let label = format!(" ▸ {} ", title.to_uppercase());
    let label_len = label.chars().count();
    // Fill the rest of the top line with ═
    let fill = w.saturating_sub(2 + label_len + 2); // TL + label + TR
    let top = format!(
        "{}{}{}{}{}",
        TL.color(color).bold(),
        H.color(color).bold(),
        label.color(color).bold(),
        H.repeat(fill).color(color).bold(),
        TR.color(color).bold()
    );
    println!("{}", top);
}

/// Draw the bottom of a panel (cyan border).
pub fn panel_bottom() {
    panel_bottom_color(Color::Cyan);
}

/// Draw the bottom of a panel with an explicit border color.
pub fn panel_bottom_color(color: Color) {
    let w = term_width();
    let fill = w.saturating_sub(2);
    println!(
        "{}{}{}",
        BL.color(color).bold(),
        H.repeat(fill).color(color).bold(),
        BR.color(color).bold()
    );
}

/// Draw an empty panel row with cyan side borders.
pub fn panel_blank() {
    panel_blank_color(Color::Cyan);
}

/// Draw an empty panel row with explicit border color.
pub fn panel_blank_color(color: Color) {
    let w = term_width();
    let fill = w.saturating_sub(2);
    println!("{}{}{}", V.color(color), " ".repeat(fill), V.color(color));
}

/// Draw a panel row with content left-aligned (cyan borders).
pub fn panel_row(content: &str) {
    panel_row_color(content, Color::Cyan);
}

/// Draw a panel row with content left-aligned and explicit border color.
pub fn panel_row_color(content: &str, color: Color) {
    let w = term_width();
    // Strip ANSI codes for length calculation
    let visible_len = strip_ansi_len(content);
    let inner_w = w.saturating_sub(4); // ║ space ... space ║
    let pad = inner_w.saturating_sub(visible_len);
    println!(
        "{} {}{} {}",
        V.color(color),
        content,
        " ".repeat(pad),
        V.color(color),
    );
}

/// Draw a panel row with content centered within the panel (cyan borders).
pub fn panel_center(content: &str) {
    panel_center_color(content, Color::Cyan);
}

/// Draw a panel row with content centered and explicit border color.
pub fn panel_center_color(content: &str, color: Color) {
    let w = term_width();
    let visible_len = strip_ansi_len(content);
    let inner_w = w.saturating_sub(4); // ║ space ... space ║
    let extra = inner_w.saturating_sub(visible_len);
    let left = extra / 2;
    let right = extra - left;
    println!(
        "{} {}{}{} {}",
        V.color(color),
        " ".repeat(left),
        content,
        " ".repeat(right),
        V.color(color),
    );
}

/// Draw a panel divider (thin single-line rule inside a double-border panel).
pub fn panel_divider() {
    panel_divider_color(Color::Cyan);
}

/// Draw a panel divider with explicit border color.
pub fn panel_divider_color(color: Color) {
    let w = term_width();
    let fill = w.saturating_sub(2);
    println!(
        "{}{}{}",
        SML.color(color),
        SH.repeat(fill).color(color),
        SMR.color(color),
    );
}

/// Draw a mid-section header inside an open panel (double-line T-junction).
pub fn panel_mid(label: &str) {
    let w = term_width();
    let label_str = format!(" {} ", label);
    let label_len = label_str.chars().count();
    let fill = w.saturating_sub(2 + label_len);
    println!(
        "{}{}{}{}",
        ML.cyan().bold(),
        label_str.cyan().bold(),
        H.repeat(fill).cyan().bold(),
        MR.cyan().bold()
    );
}

// ─── Pill progress bar ──────────────────────────────────────────────────────

/// Step definition for the progress bar.
pub struct Step {
    pub label: &'static str,
}

/// State of a step.
#[derive(Clone, Copy, PartialEq)]
pub enum StepState {
    Done,
    Active,
    Pending,
}

/// Render the pill-style progress bar.
///
/// ```text
/// [ ✓ PREREQS ]──[ ● CONFIGURE ]──[   INSTALL   ]──[  VERIFY  ]──[ LAUNCH ]
/// ```
pub fn progress_bar(steps: &[(&'static str, StepState)]) {
    let w = term_width();
    let mut parts: Vec<String> = Vec::new();

    for (label, state) in steps {
        let pill = match state {
            StepState::Done => format!("{}", format!("[ ✓ {} ]", label).bright_green().bold()),
            StepState::Active => format!("{}", format!("[ ● {} ]", label).cyan().bold()),
            StepState::Pending => format!("{}", format!("[   {}   ]", label).dimmed()),
        };
        parts.push(pill);
    }

    let connector = "──".dimmed().to_string();
    let bar = parts.join(&connector);
    let bar_visible = strip_ansi_len(&bar);
    let pad = w.saturating_sub(bar_visible) / 2;
    println!("{}{}", " ".repeat(pad), bar);
}

// ─── Key/value rows ─────────────────────────────────────────────────────────

/// Draw a key/value row inside a panel with proper alignment.
pub fn panel_kv(key: &str, value: &str) {
    let content = format!("{}  {}", key.white().bold(), value.cyan());
    panel_row(&content);
}

/// Draw a success check item inside a panel.
pub fn panel_check(msg: &str) {
    let content = format!("{}  {}", "✓".bright_green().bold(), msg.white());
    panel_row(&content);
}

/// Draw a warning item inside a panel.
pub fn panel_warn(msg: &str) {
    let content = format!("{}  {}", "⚠".yellow().bold(), msg.white());
    panel_row(&content);
}

/// Draw an error item inside a panel.
pub fn panel_error(msg: &str) {
    let content = format!("{}  {}", "✗".bright_red().bold(), msg.white());
    panel_row(&content);
}

/// Draw a spinner + message row inside a panel (overwrites current line).
pub fn panel_spinner_row(spinner_char: &str, msg: &str) {
    let w = term_width();
    let content = format!("{}  {}", spinner_char.cyan(), msg);
    let visible = strip_ansi_len(&content);
    let inner_w = w.saturating_sub(4);
    let pad = inner_w.saturating_sub(visible);
    print!("\r{} {}{} {}", V.cyan(), content, " ".repeat(pad), V.cyan());
    io::stdout().flush().ok();
}

/// Print a plain indented row inside a panel (sub-item / tree branch).
pub fn panel_sub(msg: &str) {
    let content = format!("{}  {}", "  └─".dimmed(), msg.dimmed());
    panel_row(&content);
}

// ─── Utility ────────────────────────────────────────────────────────────────

/// Estimate visible character count by stripping basic ANSI escape codes.
/// Not a full ANSI parser — good enough for our controlled strings.
fn strip_ansi_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if c == '\x1B' {
            in_escape = true;
        } else if in_escape && c == 'm' {
            in_escape = false;
        } else if !in_escape {
            len += 1;
        }
    }
    len
}

/// Flush stdout.
pub fn flush() {
    io::stdout().flush().ok();
}
