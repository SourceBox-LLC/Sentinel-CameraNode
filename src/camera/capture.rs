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
//! Video Capture from USB Cameras
//!
//! Captures frames from V4L2 devices (Linux USB cameras).

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use bytes::Bytes;

use crate::error::Result;
use crate::config::StreamingConfig;

/// A captured video frame
#[derive(Debug, Clone)]
pub struct Frame {
    /// Raw frame data
    pub data: Bytes,

    /// Capture timestamp (Unix epoch milliseconds)
    pub timestamp: i64,

    /// Frame sequence number
    pub sequence: u64,

    /// Frame width
    pub width: u32,

    /// Frame height  
    pub height: u32,

    /// Pixel format (e.g., "YUYV", "MJPG")
    pub format: String,
}

/// Camera capture handle
pub struct CameraCapture {
    device_path: String,
    width: u32,
    height: u32,
    fps: u32,
    #[allow(dead_code)]
    jpeg_quality: u8,
    
    /// Frame counter for sequence numbers
    frame_count: Arc<std::sync::atomic::AtomicU64>,
}

impl CameraCapture {
    /// Create a new capture handle
    pub fn new(
        device_path: String,
        width: u32,
        height: u32,
        streaming_config: &StreamingConfig,
    ) -> Result<Self> {
        Ok(Self {
            device_path,
            width,
            height,
            fps: streaming_config.fps,
            jpeg_quality: streaming_config.jpeg_quality,
            frame_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    /// Start capturing frames
    ///
    /// Returns a receiver channel for frames. Capture runs in a background thread.
    pub fn start(&self) -> Result<mpsc::Receiver<Frame>> {
        let (tx, rx) = mpsc::channel(60); // 2 seconds of buffer at 30fps

        let device_path = self.device_path.clone();
        let width = self.width;
        let height = self.height;
        let fps = self.fps;
        let frame_count = self.frame_count.clone();

        tracing::info!(
            "Starting capture on {} at {}x{} @ {}fps",
            device_path,
            width,
            height,
            fps
        );

        // Spawn capture thread
        // Note: In a real implementation, this would use v4l2-rs or similar
        // For now, this is a placeholder that generates test frames
        std::thread::spawn(move || {
            capture_loop(device_path, width, height, fps, tx, frame_count);
        });

        Ok(rx)
    }

    /// Stop capturing
    pub fn stop(&self) -> Result<()> {
        tracing::info!("Stopping capture on {}", self.device_path);
        // Signal the capture loop to stop
        // In a real implementation, we'd have a proper stop mechanism
        Ok(())
    }

    /// Get device path
    pub fn device_path(&self) -> &str {
        &self.device_path
    }
}

/// Capture loop - runs in a separate thread
///
/// In a full implementation, this would:
/// 1. Open the V4L2 device
/// 2. Set format and resolution
/// 3. Request buffers
/// 4. Stream frames
///
/// For now, it generates test frames.
fn capture_loop(
    device_path: String,
    width: u32,
    height: u32,
    fps: u32,
    tx: mpsc::Sender<Frame>,
    frame_count: Arc<std::sync::atomic::AtomicU64>,
) {
    let frame_duration = Duration::from_micros(1_000_000 / fps as u64);
    let mut last_frame = Instant::now();

    tracing::info!("Capture loop started for {}", device_path);

    loop {
        // Maintain frame rate
        let elapsed = last_frame.elapsed();
        if elapsed < frame_duration {
            std::thread::sleep(frame_duration - elapsed);
        }
        last_frame = Instant::now();

        // Get sequence number
        let sequence = frame_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        // Generate a test frame
        // In production, this would read from v4l2 device
        let frame = generate_test_frame(
            sequence,
            width,
            height,
            chrono::Utc::now().timestamp_millis(),
        );

        // Send frame
        match tx.blocking_send(frame) {
            Ok(_) => {
                // Frame sent successfully
            }
            Err(e) => {
                // Channel closed, stop capturing
                tracing::debug!("Capture channel closed for {}: {}", device_path, e);
                break;
            }
        }

        // Check if we should stop (channel receiver dropped)
        if tx.is_closed() {
            tracing::info!("Capture loop stopping for {}", device_path);
            break;
        }
    }

    tracing::info!("Capture loop ended for {}", device_path);
}

/// Generate a test frame (placeholder for actual camera capture)
fn generate_test_frame(sequence: u64, width: u32, height: u32, timestamp: i64) -> Frame {
    // In a real implementation, we'd capture from the camera
    // For now, generate a simple JPEG test pattern

    // Create a minimal JPEG-like test pattern
    // This is just a placeholder - real implementation would use v4l2
    let frame_data = generate_test_pattern(width, height, sequence);

    Frame {
        data: Bytes::from(frame_data),
        timestamp,
        sequence,
        width,
        height,
        format: "TEST".to_string(),
    }
}

/// Generate a test pattern (placeholder)
fn generate_test_pattern(width: u32, height: u32, sequence: u64) -> Vec<u8> {
    // Generate a gradient test pattern
    // In production, this would be actual camera data

    let mut data = Vec::with_capacity((width * height * 3) as usize);

    // Simple RGB test pattern with moving element based on sequence
    let offset = (sequence % 100) as u8;

    for y in 0..height {
        for x in 0..width {
            // Create a gradient with some movement
            let r = ((x + offset as u32) % 256) as u8;
            let g = ((y + offset as u32) % 256) as u8;
            let b = ((x + y + offset as u32) % 256) as u8;

            data.push(r);
            data.push(g);
            data.push(b);
        }
    }

    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_create() {
        let config = StreamingConfig::default();
        let capture = CameraCapture::new(
            "/dev/video0".to_string(),
            1280,
            720,
            &config,
        );
        assert!(capture.is_ok());
    }

    #[tokio::test]
    async fn test_capture_start() {
        let config = StreamingConfig::default();
        let capture = CameraCapture::new(
            "/dev/video0".to_string(),
            320,  // Small for test
            240,
            &config,
        ).unwrap();

        let mut rx = capture.start().unwrap();

        // Receive a few frames
        for _ in 0..3 {
            let frame = rx.recv().await;
            assert!(frame.is_some());
        }
    }
}