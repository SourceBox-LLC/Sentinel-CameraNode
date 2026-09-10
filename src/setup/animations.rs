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
//! Animation utilities for setup wizard
//!
//! Provides visual effects like confetti, rainbow gradients, and animations.

use std::io::{self, Write};
use std::thread;
use std::time::Duration;

use colored::Colorize;

/// Animation result type
pub type AnimationResult<T> = std::result::Result<T, io::Error>;

/// Clear terminal screen
pub fn clear_screen() -> AnimationResult<()> {
    print!("\x1B[2J\x1B[1;1H");
    io::stdout().flush()?;
    Ok(())
}

/// Clear last N lines
pub fn clear_lines(n: usize) -> AnimationResult<()> {
    for _ in 0..n {
        print!("\x1B[2K\x1B[1A"); // Clear line and move up
    }
    io::stdout().flush()?;
    Ok(())
}

/// Move cursor to position (row, col)
pub fn move_cursor(row: u16, col: u16) -> AnimationResult<()> {
    print!("\x1B[{};{}H", row, col);
    io::stdout().flush()?;
    Ok(())
}

/// Get terminal size (width, height)
pub fn get_terminal_size() -> AnimationResult<(u16, u16)> {
    let size = crossterm::terminal::size()?;
    Ok(size)
}

/// Hide cursor
pub fn hide_cursor() -> AnimationResult<()> {
    print!("\x1B[?25l");
    io::stdout().flush()?;
    Ok(())
}

/// Show cursor
pub fn show_cursor() -> AnimationResult<()> {
    print!("\x1B[?25h");
    io::stdout().flush()?;
    Ok(())
}

/// Particle for confetti animation
#[derive(Clone)]
struct Particle {
    x: f32,
    y: f32,
    velocity_x: f32,
    velocity_y: f32,
    char: char,
    color_index: usize,
}

/// Confetti particle system
pub struct ConfettiSystem {
    particles: Vec<Particle>,
    width: u16,
    height: u16,
}

impl ConfettiSystem {
    /// Create new confetti system
    pub fn new(width: u16, height: u16, num_particles: usize) -> Self {
        let mut particles = Vec::new();
        let chars = ['✦', '✧', '★', '◆', '●', '○', '♦', '♠', '♣', '♥'];
        let mut rng = fastrand::Rng::new();

        for _ in 0..num_particles {
            let char_idx = (rng.f32() * chars.len() as f32) as usize;
            particles.push(Particle {
                x: rng.f32() * width as f32,
                y: -rng.f32() * height as f32 * 0.5,
                velocity_x: (rng.f32() - 0.5) * 2.0,
                velocity_y: rng.f32() * 2.0 + 1.0,
                char: chars[char_idx.min(chars.len() - 1)],
                color_index: (rng.f32() * 8.0) as usize,
            });
        }

        Self {
            particles,
            width,
            height,
        }
    }

    /// Update particle positions
    pub fn update(&mut self, dt: f32) {
        for particle in &mut self.particles {
            particle.x += particle.velocity_x * dt * 20.0;
            particle.y += particle.velocity_y * dt * 10.0;

            // Bounce off walls
            if particle.x < 0.0 || particle.x > self.width as f32 {
                particle.velocity_x *= -0.5;
            }

            // Wrap around vertically
            if particle.y > self.height as f32 {
                particle.y = 0.0;
            }
        }
    }

    /// Render particles
    pub fn render(&self) -> AnimationResult<()> {
        let colors = [
            colored::Color::Red,
            colored::Color::Yellow,
            colored::Color::Green,
            colored::Color::Cyan,
            colored::Color::Blue,
            colored::Color::Magenta,
            colored::Color::White,
            colored::Color::BrightCyan,
        ];

        for particle in &self.particles {
            let x = particle.x.max(0.0).min(self.width as f32 - 1.0) as u16;
            let y = particle.y.max(0.0).min(self.height as f32 - 1.0) as u16;

            if y < self.height {
                move_cursor(y + 1, x + 1)?;
                let color = colors[particle.color_index % colors.len()];
                let colored_char = particle.char.to_string().color(color);
                print!("{}", colored_char);
            }
        }

        io::stdout().flush()?;
        Ok(())
    }
}

/// Simple fastrand replacement (since we don't want to add dependency)
mod fastrand {
    pub struct Rng {
        state: u64,
    }

    impl Rng {
        pub fn new() -> Self {
            Self {
                state: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_else(|_| std::time::Duration::from_secs(0))
                    .as_nanos() as u64,
            }
        }

        pub fn f32(&mut self) -> f32 {
            self.state = self
                .state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.state >> 33) as f32 / (1u64 << 31) as f32
        }
    }
}

/// Show confetti animation
pub fn show_confetti(duration: Duration) -> AnimationResult<()> {
    let (width, height) = get_terminal_size()?;
    let mut confetti = ConfettiSystem::new(width, height, 80);

    let frames = (duration.as_millis() / 50) as usize;
    let dt = 0.05;

    hide_cursor()?;
    clear_screen()?;

    for _ in 0..frames {
        clear_screen()?;
        confetti.update(dt);
        confetti.render()?;
        thread::sleep(Duration::from_millis(50));
    }

    show_cursor()?;
    clear_screen()?;

    Ok(())
}

/// Apply rainbow gradient effect to text
pub fn rainbow_text(text: &str) -> String {
    let colors = [
        colored::Color::Red,
        colored::Color::Yellow,
        colored::Color::Green,
        colored::Color::Cyan,
        colored::Color::Blue,
        colored::Color::Magenta,
    ];

    let mut result = String::new();
    let chars: Vec<char> = text.chars().collect();

    for (i, char) in chars.iter().enumerate() {
        let color_index = i % colors.len();
        result.push_str(&char.to_string().color(colors[color_index]).to_string());
    }

    result
}

/// Apply gradient with offset (for animation)
pub fn rainbow_text_offset(text: &str, offset: usize) -> String {
    let colors = [
        colored::Color::Red,
        colored::Color::Yellow,
        colored::Color::Green,
        colored::Color::Cyan,
        colored::Color::Blue,
        colored::Color::Magenta,
    ];

    let mut result = String::new();
    let chars: Vec<char> = text.chars().collect();

    for (i, char) in chars.iter().enumerate() {
        let color_index = (i + offset) % colors.len();
        result.push_str(&char.to_string().color(colors[color_index]).to_string());
    }

    result
}

/// Animate text with rainbow gradient cycling
pub fn animate_rainbow_text(text: &str, duration: Duration) -> AnimationResult<String> {
    let frames = (duration.as_millis() / 100) as usize;

    for frame in 0..frames {
        clear_lines(1)?;
        println!("{}", rainbow_text_offset(text, frame));
        thread::sleep(Duration::from_millis(100));
    }

    Ok(rainbow_text(text).to_string())
}

/// Fade in text gradually
pub fn fade_in(text: &str, duration: Duration) -> AnimationResult<()> {
    let frames = (duration.as_millis() / 30) as usize;

    for frame in 0..frames {
        let intensity = frame as f32 / frames as f32;

        // Use dimmed brightness for fade effect
        if intensity < 0.33 {
            print!("\r{}", text.dimmed());
        } else if intensity < 0.66 {
            print!("\r{}", text.normal());
        } else {
            print!("\r{}", text.bold());
        }

        io::stdout().flush()?;
        thread::sleep(Duration::from_millis(30));
    }

    println!();
    Ok(())
}

/// Fade in multiple lines
pub fn fade_in_lines(lines: &[String], frame_delay: Duration) -> AnimationResult<()> {
    for line in lines {
        fade_in(line, frame_delay)?;
        thread::sleep(frame_delay);
    }
    Ok(())
}

/// Print centered text
pub fn print_centered(text: &str) -> AnimationResult<()> {
    let (width, _) = get_terminal_size()?;
    let padding = (width as usize).saturating_sub(text.len()) / 2;
    println!("{:padding$}{}", "", text, padding = padding);
    Ok(())
}

/// Draw expanding border animation
pub fn draw_expanding_border(duration: Duration) -> AnimationResult<()> {
    let frames = (duration.as_millis() / 50) as usize;
    let (width, _) = get_terminal_size()?;

    for frame in 1..=frames {
        let progress = frame as f32 / frames as f32;
        let border_width = ((width as f32 * progress) as usize).min(60);

        clear_lines(1)?;
        println!("{:=^border_width$}", "", border_width = border_width);
        thread::sleep(Duration::from_millis(50));
    }

    Ok(())
}

/// Pulse effect on text (increase/decrease brightness)
pub fn pulse_text(text: &str, cycles: usize, cycle_duration: Duration) -> AnimationResult<()> {
    let frame_delay = Duration::from_millis(cycle_duration.as_millis() as u64 / 10);

    for _ in 0..cycles {
        // Fade in
        for _ in 0..5 {
            print!("\r{}", text.normal());
            io::stdout().flush()?;
            thread::sleep(frame_delay);
        }

        // Fade out
        for _ in 0..5 {
            print!("\r{}", text.bold());
            io::stdout().flush()?;
            thread::sleep(frame_delay);
        }
    }

    println!();
    Ok(())
}

/// Draw progress sequence with animated checkmarks
pub fn draw_progress_sequence(steps: &[&str], current: usize) -> AnimationResult<()> {
    let mut line = String::new();

    for (i, step) in steps.iter().enumerate() {
        let icon = if i < current {
            "✓".green()
        } else if i == current {
            "●".cyan()
        } else {
            "○".dimmed()
        };

        if i > 0 {
            line.push_str(" ── ");
        }

        line.push_str(&format!("{}", icon));
        line.push_str(&format!(" {}", step));
    }

    println!("{}", line);
    Ok(())
}

/// Animate text typing effect
pub fn type_text(text: &str, delay_per_char: Duration) -> AnimationResult<()> {
    for char in text.chars() {
        print!("{}", char);
        io::stdout().flush()?;
        thread::sleep(delay_per_char);
    }
    println!();
    Ok(())
}

/// Draw box with title and content
pub fn draw_box(title: &str, width: usize) -> AnimationResult<()> {
    let horizontal = "━".repeat(width.saturating_sub(2));
    let half_width = width.saturating_sub(title.len() + 2) / 2;

    println!("{}{}{}", "┏".cyan(), horizontal, "┓".cyan());
    println!(
        "{}{:half_width$}{} {:half_width$}{}",
        "┃".cyan(),
        "",
        title.bold(),
        "",
        "┃".cyan(),
        half_width = half_width
    );
    println!("{}{}{}", "┗".cyan(), horizontal, "┛".cyan());

    Ok(())
}

/// Mini celebration (small confetti burst)
pub fn show_mini_celebration() -> AnimationResult<()> {
    let emojis = ["🎉", "✨", "🎊", "⭐", "🚀"];

    for emoji in emojis {
        print!("{} ", emoji);
        io::stdout().flush()?;
        thread::sleep(Duration::from_millis(100));
    }

    println!();
    Ok(())
}

/// Animated loading spinner
pub struct Spinner {
    frames: Vec<String>,
    current: usize,
}

impl Spinner {
    pub fn new() -> Self {
        Self {
            frames: vec![
                "⠋".to_string(),
                "⠙".to_string(),
                "⠹".to_string(),
                "⠸".to_string(),
                "⠼".to_string(),
                "⠴".to_string(),
                "⠦".to_string(),
                "⠧".to_string(),
                "⠇".to_string(),
                "⠏".to_string(),
            ],
            current: 0,
        }
    }

    pub fn advance(&mut self) -> String {
        let frame = self.frames[self.current].clone();
        self.current = (self.current + 1) % self.frames.len();
        frame.cyan().to_string()
    }
}

impl Default for Spinner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rainbow_text() {
        // Force color output for testing (colored detects non-TTY and disables colors by default)
        colored::control::set_override(true);
        let text = rainbow_text("TEST");
        assert!(
            text.contains("\x1B["),
            "Expected ANSI escape codes in output: {}",
            text
        ); // Contains ANSI codes
    }

    #[test]
    fn test_rainbow_offset() {
        // Force color output for testing
        colored::control::set_override(true);
        let text1 = rainbow_text_offset("TEST", 0);
        let text2 = rainbow_text_offset("TEST", 1);
        assert_ne!(
            text1, text2,
            "Expected different colors with different offsets"
        ); // Different colors with different offsets
    }

    #[test]
    fn test_spinner() {
        let mut spinner = Spinner::new();
        let frame1 = spinner.advance();
        let frame2 = spinner.advance();
        assert_ne!(frame1, frame2); // Different frames
    }
}
