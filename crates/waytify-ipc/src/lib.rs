//! Wire protocol between the waytify daemon and its clients.
//!
//! Clients speak newline-delimited JSON over a Unix socket. Every line a client
//! sends is one [`Command`]; every line the daemon sends back is one [`Frame`].
//!
//! The daemon is the only process that talks to D-Bus, PipeWire, or the network.
//! Clients hold no state of their own beyond the last frame they received, which
//! keeps them cheap to restart. Waybar in particular respawns its module process
//! on every config reload, so the bar client has to be disposable.

pub mod paths;
pub mod state;

pub use state::{
    ArtColors, Audio, Caps, ContextKind, Device, LyricLine, Lyrics, MediaKind, PlayContext, Player,
    Repeat, Rgb, Spotify, State, Status, Track, VolumeRoute,
};

use serde::{Deserialize, Serialize};

/// Bumped whenever a change would make an older client misread a newer daemon.
///
/// The daemon reports its version in [`Frame::Hello`] and clients are expected to
/// exit loudly on a mismatch rather than guess. A stale `waytify bar` left running
/// across an upgrade is the case this exists for.
pub const PROTOCOL_VERSION: u32 = 1;

/// How much of the daemon's state a client wants to receive.
///
/// This is not a permission boundary, it is a bandwidth one. The bar renders a
/// single line of text and would throw away album art, queue entries, and lyrics,
/// so it never gets sent them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Receives [`Frame::Bar`] only: text, tooltip, and CSS classes, already rendered.
    #[default]
    Bar,
    /// Receives [`Frame::State`]: the full model, including art paths and lyrics.
    Full,
}

/// A point in compositor logical pixels, used to anchor the popup where the click landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// What the popup process is being asked to do.
///
/// Sent to [`Scope::Full`] subscribers rather than acted on by the daemon, since
/// the daemon has no window of its own and the popup is the only process that can
/// answer for whether one is currently visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PopupAction {
    Show {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<Point>,
    },
    Hide,
    Toggle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<Point>,
    },
}

/// Client to daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Must be the first command on a connection. Sending it twice re-scopes the client.
    Subscribe {
        #[serde(default)]
        scope: Scope,
    },

    /// Whether this client's window is actually on screen.
    ///
    /// Subscribing is not the same as watching. The player window stays
    /// connected while hidden, because reopening it has to be instant, so the
    /// daemon cannot tell from the connection alone whether anyone can see what
    /// it publishes. Anything polled for the window's benefit, which is the
    /// device list, the queue and lyrics, waits for this.
    Watching {
        active: bool,
    },

    // Transport. These route to MPRIS, never to the Spotify Web API: MPRIS has no
    // rate limit, needs no token, and works when the network is down.
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    /// Absolute seek within the current track.
    Seek {
        position_ms: u64,
    },
    /// Relative seek. Negative rewinds. The daemon clamps to track bounds.
    SeekBy {
        delta_ms: i64,
    },

    ToggleShuffle,
    SetShuffle {
        on: bool,
    },
    CycleRepeat,
    SetRepeat {
        mode: Repeat,
    },

    /// Absolute volume, 0 to 100. The daemon decides whether that means the local
    /// PipeWire stream or a remote Connect device. See [`VolumeRoute`].
    SetVolume {
        percent: u8,
    },
    /// Relative volume, clamped to 0 to 100 by the daemon.
    VolumeBy {
        delta: i8,
    },
    ToggleMute,

    /// Save or unsave the current track. Requires an authorized Spotify account.
    ToggleLike,
    /// Move playback to another Spotify Connect device. Requires Premium.
    TransferTo {
        device_id: String,
    },
    /// Ask Spotify for the device list now.
    ///
    /// It is polled while the window is open, but a device that has just been
    /// woken up should not need waiting for.
    RefreshDevices,

    /// Show the popup if hidden, hide it if shown. `at` anchors it to the click,
    /// which is how the popup lands on whichever monitor was clicked.
    TogglePopup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<Point>,
    },
    ShowPopup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<Point>,
    },
    HidePopup,

    /// MPRIS `Raise`, to bring the player's own window forward.
    RaisePlayer,
    /// Ask the daemon to exit. Clients see the socket close.
    Shutdown,
}

/// Daemon to client.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// Sent once, immediately on connect, before any state.
    Hello {
        protocol: u32,
        /// Daemon crate version, for diagnostics only.
        version: String,
    },
    /// Sent to [`Scope::Full`] subscribers whenever the model changes.
    State { state: Box<State> },
    /// Sent to [`Scope::Bar`] subscribers. Already rendered, so the bar client
    /// carries no formatting logic and no user config of its own.
    Bar { bar: BarOutput },
    /// Sent to [`Scope::Full`] subscribers to show, hide, or toggle the window.
    Popup { action: PopupAction },
    /// A command was carried out. Sent so a one-shot client can exit as soon as
    /// the work is done instead of waiting out a timeout to learn nothing broke.
    Ack,
    /// A command failed. Non-fatal: the connection stays open.
    Error { message: String },
}

/// Exactly the JSON object Waybar expects on a custom module's stdout.
///
/// Rendered daemon-side so that format strings live in one config file rather
/// than being duplicated into every client.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BarOutput {
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alt: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tooltip: String,
    /// Waybar appends each entry to the widget's CSS classes, which is what makes
    /// `#custom-waytify.playing` work in a user stylesheet.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class: Vec<String>,
    /// Track progress as a percentage, for Waybar's built-in progress rendering.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub percentage: u8,
}

fn is_zero(n: &u8) -> bool {
    *n == 0
}

impl Frame {
    /// Serialize as a single line, newline included. Every frame is one line, so a
    /// client can read with `lines()` and never needs a length prefix.
    pub fn to_line(&self) -> serde_json::Result<String> {
        let mut s = serde_json::to_string(self)?;
        s.push('\n');
        Ok(s)
    }
}

impl Command {
    pub fn to_line(&self) -> serde_json::Result<String> {
        let mut s = serde_json::to_string(self)?;
        s.push('\n');
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_round_trip() {
        let cases = [
            Command::PlayPause,
            Command::Subscribe { scope: Scope::Full },
            Command::Seek { position_ms: 42_000 },
            Command::SeekBy { delta_ms: -5_000 },
            Command::SetVolume { percent: 70 },
            Command::TransferTo { device_id: "abc123".into() },
            Command::TogglePopup { at: Some(Point { x: 1920, y: 12 }) },
            Command::TogglePopup { at: None },
        ];
        for c in cases {
            let line = c.to_line().unwrap();
            assert!(line.ends_with('\n'), "frames must be exactly one line");
            let back: Command = serde_json::from_str(&line).unwrap();
            assert_eq!(c, back);
        }
    }

    #[test]
    fn subscribe_defaults_to_bar_scope() {
        // A client that sends a bare subscribe should not accidentally opt into
        // album art and lyrics it will never render.
        let c: Command = serde_json::from_str(r#"{"cmd":"subscribe"}"#).unwrap();
        assert_eq!(c, Command::Subscribe { scope: Scope::Bar });
    }

    #[test]
    fn bar_output_omits_empty_fields() {
        // Waybar treats an empty tooltip string as a tooltip, so absent and empty
        // are not the same thing on the wire.
        let out = BarOutput { text: "Paused".into(), ..Default::default() };
        let json = serde_json::to_string(&out).unwrap();
        assert_eq!(json, r#"{"text":"Paused"}"#);
    }

    #[test]
    fn frames_are_externally_distinguishable() {
        let hello = Frame::Hello { protocol: PROTOCOL_VERSION, version: "0.1.0".into() };
        let json = hello.to_line().unwrap();
        assert!(json.contains(r#""type":"hello""#));
    }
}
