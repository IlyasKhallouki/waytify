//! User configuration, read from `~/.config/waytify/config.toml`.
//!
//! Every field has a default, so the file is entirely optional and a fresh
//! install works with nothing in it. Unknown keys are rejected rather than
//! ignored: a silently dropped typo in a config file is a bad afternoon.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub player: PlayerConfig,
    pub bar: BarConfig,
    pub spotify: SpotifyConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpotifyConfig {
    /// Your own Spotify application's client id.
    ///
    /// Not a secret, which is why it lives here rather than in the keyring, but
    /// still per user: Spotify counts rate limits per application, so everyone
    /// sharing one id would share one budget. Register your own at
    /// developer.spotify.com and add
    /// `http://127.0.0.1:PORT/callback` as a redirect URI.
    ///
    /// Empty means the Spotify layer is off, and everything else works as usual.
    pub client_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlayerConfig {
    /// Players to favour when more than one is running, most preferred first.
    ///
    /// Accepts a short name (`spotify`) or a full bus name. Preference only
    /// decides between players in the same playback state: something actually
    /// playing always wins, so the bar never disagrees with the speakers.
    pub preferred: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BarConfig {
    /// Template used while playing. See [`crate::format`] for the syntax.
    pub format: String,
    /// Template used while paused. Falls back to `format` when unset.
    pub format_paused: Option<String>,
    /// Template used when nothing is playing. Empty output hides the module.
    pub format_stopped: Option<String>,
    pub tooltip: String,
    pub icons: Icons,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            // The bracketed group disappears when there is no artist, which is
            // common for podcasts and local files.
            format: "{icon}  {title}[ · {artist}]".into(),
            format_paused: None,
            format_stopped: Some(String::new()),
            tooltip: "{title}[\n{artist}][\n{album}][\n\n{position} / {duration}]".into(),
            icons: Icons::default(),
        }
    }
}

impl BarConfig {
    /// The template for a given playback state, after fallbacks.
    pub fn template(&self, status: waytify_ipc::Status) -> &str {
        use waytify_ipc::Status;
        match status {
            Status::Playing => &self.format,
            Status::Paused => self.format_paused.as_deref().unwrap_or(&self.format),
            Status::Stopped => self.format_stopped.as_deref().unwrap_or(&self.format),
        }
    }
}

/// Glyphs for each playback state.
///
/// The defaults are plain Unicode so that a fresh install renders on any font.
/// Nerd Font users will want to swap these, and the README shows how.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Icons {
    pub playing: String,
    pub paused: String,
    pub stopped: String,
}

impl Default for Icons {
    fn default() -> Self {
        Self { playing: "▶".into(), paused: "⏸".into(), stopped: "⏹".into() }
    }
}

impl Icons {
    pub fn for_status(&self, status: waytify_ipc::Status) -> &str {
        use waytify_ipc::Status;
        match status {
            Status::Playing => &self.playing,
            Status::Paused => &self.paused,
            Status::Stopped => &self.stopped,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid TOML: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
}

impl Config {
    /// Load from a path, or return defaults when the file does not exist.
    ///
    /// A missing config is normal. A malformed one is not, and is reported rather
    /// than silently replaced with defaults, so a typo does not look like the
    /// setting simply having no effect.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Read { path: path.display().to_string(), source });
            }
        };
        toml::from_str(&text)
            .map_err(|source| ConfigError::Parse { path: path.display().to_string(), source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waytify_ipc::Status;

    #[test]
    fn an_empty_file_is_the_default_config() {
        assert_eq!(toml::from_str::<Config>("").unwrap(), Config::default());
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let cfg = Config::load(Path::new("/nonexistent/waytify/config.toml")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn a_typo_is_reported_rather_than_ignored() {
        // `formats` is not a key. Accepting it silently would look exactly like
        // the format option having no effect.
        let err = toml::from_str::<Config>("[bar]\nformats = \"x\"").unwrap_err();
        assert!(err.to_string().contains("formats"), "error should name the bad key: {err}");
    }

    #[test]
    fn paused_falls_back_to_the_playing_format() {
        let cfg =
            BarConfig { format: "P {title}".into(), format_paused: None, ..Default::default() };
        assert_eq!(cfg.template(Status::Paused), "P {title}");
    }

    #[test]
    fn stopped_defaults_to_empty_so_the_module_collapses() {
        // Waybar hides a custom module whose text is empty, which is what most
        // people want when no music is running.
        assert_eq!(BarConfig::default().template(Status::Stopped), "");
    }

    #[test]
    fn partial_config_keeps_defaults_for_everything_else() {
        let cfg: Config = toml::from_str("[player]\npreferred = [\"spotify\"]").unwrap();
        assert_eq!(cfg.player.preferred, vec!["spotify"]);
        assert_eq!(cfg.bar.format, BarConfig::default().format);
    }

    #[test]
    fn config_round_trips_through_toml() {
        // Guards the `waytify config --print-default` output staying loadable.
        let text = toml::to_string(&Config::default()).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), Config::default());
    }
}
