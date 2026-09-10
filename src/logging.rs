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

//! Tracing layer that forwards log events into the TUI dashboard.
//!
//! All `tracing::info!()`, `tracing::warn!()`, etc. calls throughout the
//! codebase are routed through this layer once a [`Dashboard`] is installed.
//! Before that, events are silently discarded (the TUI isn't visible yet
//! anyway — startup messages use `println!` directly).

use std::fmt;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::Dashboard;

/// Shared slot that holds the [`Dashboard`] once it's created.
///
/// The tracing subscriber is installed at process start (before the dashboard
/// exists), so this starts as `None` and is filled in by [`set_dashboard`].
static DASHBOARD: once_cell::sync::Lazy<Arc<Mutex<Option<Dashboard>>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(None)));

/// Install a [`Dashboard`] so that future tracing events are forwarded to it.
pub fn set_dashboard(dash: Dashboard) {
    if let Ok(mut slot) = DASHBOARD.lock() {
        *slot = Some(dash);
    }
}

/// Remove the installed [`Dashboard`], if any, so subsequent tracing events
/// are silently discarded. Used when we tear down the live node to re-enter
/// the setup wizard (setup output would otherwise bleed into an orphaned TUI).
pub fn clear_dashboard() {
    if let Ok(mut slot) = DASHBOARD.lock() {
        *slot = None;
    }
}

/// A [`tracing_subscriber::Layer`] that formats each event into a single line
/// and pushes it into the dashboard's log buffer (which also persists to SQLite).
pub struct DashboardLayer;

impl<S> Layer<S> for DashboardLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let dash = match DASHBOARD.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(d) => d.clone(),
                None => return, // Dashboard not installed yet
            },
            Err(_) => return,
        };

        // Extract the message field from the event
        let mut visitor = MessageVisitor(String::new());
        event.record(&mut visitor);
        let message = visitor.0;
        if message.is_empty() {
            return;
        }

        match *event.metadata().level() {
            Level::ERROR => dash.log_error(message),
            Level::WARN  => dash.log_warn(message),
            Level::INFO  => dash.log_info(message),
            Level::DEBUG | Level::TRACE => dash.log_debug(message),
        }
    }
}

/// Visitor that concatenates all fields into a single display string.
/// The `message` field (from `info!("...")`) is placed first; any additional
/// structured fields are appended as ` key=value`.
struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.0 = format!("{:?}", value);
        } else if self.0.is_empty() {
            self.0 = format!("{}={:?}", field.name(), value);
        } else {
            self.0.push_str(&format!(" {}={:?}", field.name(), value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0 = value.to_string();
        } else if self.0.is_empty() {
            self.0 = format!("{}={}", field.name(), value);
        } else {
            self.0.push_str(&format!(" {}={}", field.name(), value));
        }
    }
}
