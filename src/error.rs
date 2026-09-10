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
//! Error types for Sentinel CameraNode

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Camera error: {0}")]
    Camera(String),

    #[error("Capture error: {0}")]
    Capture(String),

    #[error("API error: {0}")]
    Api(String),

    /// HTTP error from the backend where we know the exact status code.
    ///
    /// Callers that want to make retry decisions (is this a 429 we should
    /// back off on, or a 403 we should give up on?) should prefer this
    /// variant over the stringly-typed ``Api``.  Parsing status out of a
    /// human-readable message is fragile — see the commit that introduced
    /// this variant for the bug that motivated it (push-segment retries
    /// silently never firing because the matcher looked for "failed: 429"
    /// and the producer emitted "failed (429):").
    #[error("API error ({status}): {message}")]
    ApiStatus { status: u16, message: String },

    #[error("HTTP server error: {0}")]
    Server(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Streaming error: {0}")]
    Streaming(String),

    #[error("Io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("HTTP client error: {0}")]
    HttpClient(#[from] reqwest::Error),

    #[error("Unknown error: {0}")]
    Unknown(String),

    /// The underlying failure has already been shown to the user through a
    /// formatted TUI message (e.g. via `setup::recovery::show_registration_error`).
    /// Main should translate this into a non-zero exit code without re-printing.
    #[error("(already reported)")]
    AlreadyReported,

    /// The user responded to a registration failure by confirming the
    /// credential-reset prompt. The credential rows have been wiped; the
    /// caller should drop the current node state, re-run the setup wizard,
    /// and re-enter the run loop with fresh credentials.
    #[error("(reset requested)")]
    ResetRequested,
}

impl From<serde_yaml::Error> for Error {
    fn from(e: serde_yaml::Error) -> Self {
        Error::Config(e.to_string())
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Unknown(e.to_string())
    }
}

