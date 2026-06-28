// --- User configuration ---
//
// A small TOML config file, read once at startup from the platform config
// directory (e.g. `~/.config/lightningview/config.toml` on Linux,
// `%APPDATA%\lightningview\config.toml` on Windows). When no file exists yet a
// commented default is written so users have something to edit.

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// A pair of physical keys an action can be bound to. The first key always
/// means "backward / previous", the second "forward / next".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyPair {
    /// Left / Right arrow keys.
    Arrows,
    /// Page Up / Page Down keys.
    PageUpDown,
}

impl KeyPair {
    /// The (backward, forward) egui keys this pair maps to.
    pub fn keys(self) -> (egui::Key, egui::Key) {
        match self {
            KeyPair::Arrows => (egui::Key::ArrowLeft, egui::Key::ArrowRight),
            KeyPair::PageUpDown => (egui::Key::PageUp, egui::Key::PageDown),
        }
    }
}

/// Which key pair drives each of the two navigation actions.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyBindings {
    /// Seek backward / forward within a playing video.
    pub video_seek: KeyPair,
    /// Move to the previous / next file in the directory.
    pub file_browse: KeyPair,
}

impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            video_seek: KeyPair::Arrows,
            file_browse: KeyPair::PageUpDown,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub keybindings: KeyBindings,
}

/// Comment block prepended to a freshly written default config so the file is
/// self-documenting.
const CONFIG_HEADER: &str = "\
# LightningView configuration
#
# [keybindings]
#   video_seek  = key pair that seeks within a playing video
#   file_browse = key pair that moves to the previous / next file
#
# Valid values for each: \"arrows\" (Left / Right) or \"pageupdown\" (Page Up / Page Down)

";

impl Config {
    /// Load the config from disk, falling back to defaults on any error. If no
    /// config exists yet, a commented default is written for the user.
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            log::warn!("Could not determine config directory; using default keybindings.");
            return Config::default();
        };
        match fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(cfg) => cfg,
                Err(e) => {
                    log::warn!(
                        "Failed to parse config {}: {e}. Using defaults.",
                        path.display()
                    );
                    Config::default()
                }
            },
            Err(_) => {
                // Most likely the file simply doesn't exist yet: create one.
                let cfg = Config::default();
                cfg.write_default(&path);
                cfg
            }
        }
    }

    fn write_default(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                log::warn!("Failed to create config dir {}: {e}", parent.display());
                return;
            }
        }
        match toml::to_string_pretty(self) {
            Ok(body) => {
                let contents = format!("{CONFIG_HEADER}{body}");
                if let Err(e) = fs::write(path, contents) {
                    log::warn!("Failed to write default config {}: {e}", path.display());
                } else {
                    log::info!("Wrote default config to {}", path.display());
                }
            }
            Err(e) => log::warn!("Failed to serialize default config: {e}"),
        }
    }
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("lightningview").join("config.toml"))
}
