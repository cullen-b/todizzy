/// User-facing settings, persisted to disk as JSON.
///
/// All fields have sensible defaults so a missing settings file
/// launches the app immediately without user interaction.
use std::{fs, path::PathBuf};

use serde::{Deserialize, Serialize};

// ── Types ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MotionMode {
    /// Full Vim modal editing (Normal / Insert / Visual).
    Vim,
    /// Simplified Helix-style selection-first navigation.
    Helix,
    /// Plain editing — no modal layer.
    None,
}

impl Default for MotionMode {
    fn default() -> Self {
        Self::Vim
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Which key-motion dialect the editor uses.
    pub motion_mode: MotionMode,

    /// Hide the window when it loses keyboard focus.
    pub close_on_focus_loss: bool,

    /// Editor font size in points.
    pub font_size: f64,

    /// Initial window width in points.
    pub window_width: f64,

    /// Initial window height in points.
    pub window_height: f64,

    /// Show the ‹ › note-navigation arrow buttons.
    pub show_nav_arrows: bool,

    /// Show the page-indicator dots at the top of the window.
    pub show_page_dots: bool,

    /// Show the N / I / V mode indicator.
    pub show_mode_indicator: bool,

    /// Automatically `git pull` on open and `git push` on close.
    /// Requires the user to have initialised a git repo in the notes directory.
    pub git_sync: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            motion_mode: MotionMode::Vim,
            close_on_focus_loss: true,
            font_size: 14.0,
            window_width: 360.0,
            window_height: 420.0,
            show_nav_arrows: true,
            show_page_dots: true,
            show_mode_indicator: true,
            git_sync: false,
        }
    }
}

// ── Persistence ───────────────────────────────────────────────────────────────

impl Settings {
    /// Load from `path`, or return defaults if the file is absent / corrupt.
    pub fn load(path: &PathBuf) -> Self {
        match fs::read_to_string(path) {
            Ok(json) => serde_json::from_str(&json).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist to `path`, creating parent directories as needed.
    pub fn save(&self, path: &PathBuf) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).expect("Settings serialise");
        fs::write(path, json)
    }
}
