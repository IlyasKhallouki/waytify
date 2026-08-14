//! The canonical model the daemon owns and clients render.
//!
//! Two rules shape everything here.
//!
//! First, anything the Spotify Web API provides is optional. A missing token, a
//! free account, or no network at all should leave a working player that is only
//! missing likes, queue, and Connect devices. `None` therefore means "not known"
//! rather than "false", and the UI is expected to hide a control instead of
//! showing one that fails when clicked.
//!
//! Second, position is not a live value. The daemon sends the position it last
//! observed and clients advance it locally against their own clock, re-anchoring
//! on every frame. Pushing a position update at display framerate over a socket
//! would be wasteful, and polling the player that often is what the interpolated
//! clock exists to avoid.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl Status {
    pub fn is_playing(self) -> bool {
        matches!(self, Status::Playing)
    }

    /// The CSS class the bar and popup both use, so a single stylesheet rule
    /// covers them.
    pub fn css_class(self) -> &'static str {
        match self {
            Status::Playing => "playing",
            Status::Paused => "paused",
            Status::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Repeat {
    #[default]
    Off,
    /// Repeat the current track. MPRIS calls this `Track`.
    Track,
    /// Repeat the current context. MPRIS calls this `Playlist`.
    Playlist,
}

impl Repeat {
    /// Cycle order matches what Spotify's own client does, so muscle memory carries over.
    pub fn next(self) -> Self {
        match self {
            Repeat::Off => Repeat::Playlist,
            Repeat::Playlist => Repeat::Track,
            Repeat::Track => Repeat::Off,
        }
    }
}

/// Colours sampled from the album art, exposed to GTK CSS as named colours.
///
/// Themes opt in with `background: @art_vibrant;`. A theme that ignores them looks
/// exactly as it did, which is why this ships off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtColors {
    /// Most saturated colour that still clears a contrast check against the popup background.
    pub vibrant: Rgb,
    /// Low saturation companion, for large fills where `vibrant` would be too loud.
    pub muted: Rgb,
    /// Foreground guaranteed readable on top of `vibrant`.
    pub on_vibrant: Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    /// Relative luminance per WCAG 2.1, used to pick a readable foreground.
    pub fn luminance(self) -> f32 {
        fn channel(c: u8) -> f32 {
            let c = c as f32 / 255.0;
            if c <= 0.039_28 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        }
        0.2126 * channel(self.r) + 0.7152 * channel(self.g) + 0.0722 * channel(self.b)
    }

    /// Contrast ratio against another colour, from 1.0 to 21.0.
    pub fn contrast(self, other: Rgb) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

/// What sort of thing is playing.
///
/// Podcasts are not songs and the window should not pretend otherwise: they have
/// no lyrics to look up, and saving one is a different Spotify endpoint from
/// saving a track.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    #[default]
    Music,
    Podcast,
}

/// Something a search turned up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub name: String,
    /// The artist for a track, the artist for an album, the owner otherwise.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subtitle: String,
    pub uri: String,
    pub kind: SearchKind,
}

/// What a result is, which decides how it gets played.
///
/// A track is played on its own. An album or a playlist is a context, which
/// Spotify starts rather than plays, and those are different request bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchKind {
    Track,
    Album,
    Playlist,
}

/// One of the user's own playlists, enough to show it and to start it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playlist {
    pub name: String,
    pub uri: String,
    /// How many tracks, when Spotify says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracks: Option<u32>,
}

/// Where playback is coming from, when Spotify says.
///
/// MPRIS has no idea about this. A player knows what it is playing, not what it
/// was picked out of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayContext {
    pub kind: ContextKind,
    pub name: String,
    /// What to hand back to Spotify to play from it again.
    ///
    /// Absent for a context Spotify names but will not identify, which is rare
    /// and means the upcoming list can be read but not played from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Opens it in the real client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl PlayContext {
    /// Whether an item inside this context can be started directly.
    ///
    /// Spotify's offset only works for an album or a playlist. An artist page
    /// or a show has an order, but not one it will start you at.
    pub fn is_addressable(&self) -> bool {
        self.uri.is_some() && matches!(self.kind, ContextKind::Playlist | ContextKind::Album)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextKind {
    Playlist,
    Album,
    Artist,
    Show,
    /// Your saved songs, which Spotify calls a collection.
    Collection,
    /// Something added after this was written. Named rather than dropped, since
    /// "playing from something" is still worth showing.
    #[serde(other)]
    Other,
}

impl ContextKind {
    /// What to call it in a heading, lower case.
    pub fn noun(self) -> &'static str {
        match self {
            ContextKind::Playlist => "playlist",
            ContextKind::Album => "album",
            ContextKind::Artist => "artist",
            ContextKind::Show => "podcast",
            ContextKind::Collection => "collection",
            ContextKind::Other => "context",
        }
    }

    /// How to introduce it, as the real client does.
    pub fn label(self) -> &'static str {
        match self {
            ContextKind::Playlist => "Playing from playlist",
            ContextKind::Album => "Playing from album",
            ContextKind::Artist => "Playing from artist",
            ContextKind::Show => "Playing from podcast",
            ContextKind::Collection => "Playing from liked songs",
            ContextKind::Other => "Playing from",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// MPRIS `mpris:trackid`. Used as the cache key for art, lyrics, and like state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artists: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_ms: Option<u64>,
    /// Remote URL straight from `mpris:artUrl`. For Spotify this is an `i.scdn.co` link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_url: Option<String>,
    /// Local path once the daemon has fetched it. Full scope only, since the bar
    /// cannot render an image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_path: Option<PathBuf>,
    /// Full scope only, and only when art-derived theming is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub colors: Option<ArtColors>,
    /// `None` when no Spotify account is authorized, which is different from "not saved".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liked: Option<bool>,
    /// `xesam:url`, so the popup can offer to open the track in the real client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Music unless something says otherwise, which is the safe way round: a
    /// song mislabelled as a podcast loses its lyrics and its like button.
    #[serde(default, skip_serializing_if = "is_music")]
    pub kind: MediaKind,
}

fn is_music(kind: &MediaKind) -> bool {
    *kind == MediaKind::Music
}

impl Track {
    /// Artists joined the way both Spotify and Apple Music display them.
    pub fn artist_line(&self) -> String {
        self.artists.join(", ")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Player {
    /// Full D-Bus name, for example `org.mpris.MediaPlayer2.spotify`.
    pub bus_name: String,
    /// The player's own `Identity`, for example `Spotify`.
    pub identity: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<Track>,
    /// Position at the moment this frame was sent. Clients interpolate from here.
    pub position_ms: u64,
    /// `None` when the player does not expose it. Spotify's MPRIS reporting of
    /// these two is unreliable, so the Web API is preferred when authorized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<Repeat>,
}

/// Where a volume change should be sent.
///
/// One slider, two possible targets. Which one is live depends on whether audio is
/// coming out of this machine or a remote Connect device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeRoute {
    /// The player's own PipeWire stream. Always works: no account, no network.
    Local,
    /// A Spotify Connect device, over the Web API. Requires Premium.
    Remote,
    /// Nothing is writable. The popup shows a disabled control rather than one
    /// that silently does nothing.
    #[default]
    Unavailable,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Audio {
    /// 0 to 100, of whichever target `route` names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub muted: Option<bool>,
    pub route: VolumeRoute,
}

impl Audio {
    pub fn is_empty(&self) -> bool {
        self.volume.is_none() && self.route == VolumeRoute::Unavailable
    }
}

/// A Spotify Connect endpoint: this machine, a phone, a speaker, a TV.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    /// Spotify's own type string, for example `Computer`, `Smartphone`, `Speaker`.
    pub kind: String,
    pub is_active: bool,
    /// Spotify reports some devices as unable to accept volume changes.
    pub supports_volume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_percent: Option<u8>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spotify {
    /// False until the OAuth flow has completed at least once.
    pub authorized: bool,
    /// `None` until the account has been checked. Free accounts keep reads but
    /// lose every write to `/me/player/*`, so the UI needs to know which it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub premium: Option<bool>,
    /// Full scope only, and only populated while the popup is open. Polling this
    /// with nothing watching would burn rate limit for no reason.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<Device>,
    /// Full scope only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queue: Vec<Track>,
    /// False once Spotify has refused a library call, which happens when an
    /// application in development mode has not allowlisted this account. The
    /// like button hides rather than failing on every press.
    #[serde(default = "yes")]
    pub library_available: bool,
    /// Id of the device playback is currently on, when Spotify reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_device: Option<String>,
    /// Full scope only. What the current track was played out of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<PlayContext>,
    /// Full scope only, and only once something has asked for them. Fetched
    /// when the picker opens rather than on a timer: a playlist list is a thing
    /// you go looking for, not something that needs to be current.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub playlists: Vec<Playlist>,
    /// Full scope only. Whatever the last search turned up, cleared when the
    /// box is emptied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<SearchResult>,
    /// Full scope only, and only once the list has been opened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<Track>,
    /// Everything in the playlist or album being played, once asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub context_tracks: Vec<Track>,
    /// Set when Spotify refused to say what is in the current context.
    ///
    /// Its own algorithmic playlists answer 404 to any request for their
    /// details, so the list cannot be had. Saying so beats an empty panel that
    /// looks broken.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub context_closed: bool,
}

impl Default for Spotify {
    fn default() -> Self {
        Self {
            authorized: false,
            premium: None,
            devices: Vec::new(),
            queue: Vec::new(),
            library_available: true,
            active_device: None,
            context: None,
            playlists: Vec::new(),
            search: Vec::new(),
            recent: Vec::new(),
            context_tracks: Vec::new(),
            context_closed: false,
        }
    }
}

impl Spotify {
    pub fn is_empty(&self) -> bool {
        !self.authorized
            && self.devices.is_empty()
            && self.queue.is_empty()
            && self.context.is_none()
            && self.playlists.is_empty()
            && self.search.is_empty()
            && self.recent.is_empty()
            && self.context_tracks.is_empty()
    }

    /// True when writes to `/me/player/*` are expected to succeed.
    ///
    /// Unknown counts as allowed. Spotify only reports the subscription level to
    /// a token holding `user-read-private`, and refusing to offer a control that
    /// probably works is worse than one refused attempt: the first 403 records
    /// the answer and the control disappears from then on.
    pub fn can_control_remote(&self) -> bool {
        self.authorized && self.premium != Some(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LyricLine {
    pub at_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lyrics {
    /// Always timed. The window shows the line being sung and its neighbours,
    /// which is not something lyrics without timing can be put into, so lyrics
    /// that arrive without any are discarded rather than carried unusable.
    pub lines: Vec<LyricLine>,
}

impl Lyrics {
    /// Index of the line that should be highlighted at `position_ms`.
    ///
    /// Returns `None` before the first timed line, which is the instrumental
    /// intro on most tracks.
    pub fn line_at(&self, position_ms: u64) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        match self.lines.binary_search_by_key(&position_ms, |l| l.at_ms) {
            Ok(i) => Some(i),
            Err(0) => None,
            Err(i) => Some(i - 1),
        }
    }
}

/// What the UI is allowed to offer right now.
///
/// Derived by the daemon so that each client does not re-implement the same
/// "is this authorized, is this Premium, does this player support seeking"
/// reasoning and drift apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caps {
    pub can_seek: bool,
    pub can_like: bool,
    pub can_transfer: bool,
    pub can_set_volume: bool,
    /// True when a Spotify account is connected but is not Premium. The popup
    /// uses this to show the one-time notice explaining what is unavailable.
    pub show_free_account_notice: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// `None` when no MPRIS player is running at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player: Option<Player>,
    #[serde(default, skip_serializing_if = "Audio::is_empty")]
    pub audio: Audio,
    #[serde(default, skip_serializing_if = "Spotify::is_empty")]
    pub spotify: Spotify,
    /// Full scope only. Fetched from lrclib on track change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lyrics: Option<Lyrics>,
    pub caps: Caps,
}

impl State {
    pub fn status(&self) -> Status {
        self.player.as_ref().map_or(Status::Stopped, |p| p.status)
    }

    pub fn track(&self) -> Option<&Track> {
        self.player.as_ref()?.track.as_ref()
    }

    /// CSS classes applied to both the Waybar widget and the popup root, so one
    /// vocabulary covers both stylesheets.
    pub fn css_classes(&self) -> Vec<String> {
        let mut out = vec![self.status().css_class().to_string()];
        if self.player.is_none() {
            out.push("no-player".into());
        }
        if self.audio.route == VolumeRoute::Remote {
            out.push("remote".into());
        }
        if self.spotify.authorized && self.spotify.premium == Some(false) {
            out.push("no-premium".into());
        }
        if self.track().and_then(|t| t.liked) == Some(true) {
            out.push("liked".into());
        }
        // Reaches the bar as well as the window, so a Waybar stylesheet can mark
        // an episode differently from a song without waytify choosing an icon
        // on its behalf.
        if self.track().map(|t| t.kind) == Some(MediaKind::Podcast) {
            out.push("podcast".into());
        }
        out
    }

    /// Track progress as a percentage, for Waybar's progress rendering.
    pub fn percentage(&self) -> u8 {
        let Some(p) = self.player.as_ref() else { return 0 };
        let Some(len) = p.track.as_ref().and_then(|t| t.length_ms).filter(|l| *l > 0) else {
            return 0;
        };
        ((p.position_ms.min(len) as f64 / len as f64) * 100.0).round() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lyrics() -> Lyrics {
        Lyrics {
            lines: vec![
                LyricLine { at_ms: 1_000, text: "first".into() },
                LyricLine { at_ms: 5_000, text: "second".into() },
                LyricLine { at_ms: 9_000, text: "third".into() },
            ],
        }
    }

    #[test]
    fn only_a_playlist_or_album_can_be_started_at_an_item() {
        let context = |kind, uri: Option<&str>| PlayContext {
            kind,
            name: "Something".into(),
            uri: uri.map(Into::into),
            url: None,
        };

        assert!(context(ContextKind::Playlist, Some("spotify:playlist:x")).is_addressable());
        assert!(context(ContextKind::Album, Some("spotify:album:x")).is_addressable());

        // An artist page and a show have an order but not one Spotify will
        // start you at, so their rows stay unclickable rather than failing.
        assert!(!context(ContextKind::Artist, Some("spotify:artist:x")).is_addressable());
        assert!(!context(ContextKind::Show, Some("spotify:show:x")).is_addressable());
        assert!(!context(ContextKind::Collection, Some("spotify:collection:x")).is_addressable());
        assert!(!context(ContextKind::Other, Some("spotify:whatever:x")).is_addressable());

        // Named but not identified: readable, not playable.
        assert!(!context(ContextKind::Playlist, None).is_addressable());
    }

    #[test]
    fn lyrics_before_first_line_highlight_nothing() {
        // The intro is usually instrumental, so highlighting line zero there is wrong.
        assert_eq!(lyrics().line_at(0), None);
        assert_eq!(lyrics().line_at(999), None);
    }

    #[test]
    fn lyrics_pick_the_line_that_has_started() {
        let l = lyrics();
        assert_eq!(l.line_at(1_000), Some(0), "exact timestamp starts that line");
        assert_eq!(l.line_at(4_999), Some(0));
        assert_eq!(l.line_at(5_000), Some(1));
        assert_eq!(l.line_at(60_000), Some(2), "past the end holds the last line");
    }

    #[test]
    fn percentage_is_zero_without_a_known_length() {
        let s = State {
            player: Some(Player {
                bus_name: "org.mpris.MediaPlayer2.spotify".into(),
                identity: "Spotify".into(),
                status: Status::Playing,
                track: Some(Track { title: "x".into(), length_ms: None, ..Default::default() }),
                position_ms: 30_000,
                shuffle: None,
                repeat: None,
            }),
            ..Default::default()
        };
        assert_eq!(s.percentage(), 0, "a live stream has no meaningful progress");
    }

    #[test]
    fn percentage_clamps_past_the_end() {
        let s = State {
            player: Some(Player {
                bus_name: "b".into(),
                identity: "i".into(),
                status: Status::Playing,
                track: Some(Track {
                    title: "x".into(),
                    length_ms: Some(10_000),
                    ..Default::default()
                }),
                // Interpolation can overshoot the real length between drift corrections.
                position_ms: 12_000,
                shuffle: None,
                repeat: None,
            }),
            ..Default::default()
        };
        assert_eq!(s.percentage(), 100);
    }

    #[test]
    fn repeat_cycles_the_way_spotify_does() {
        assert_eq!(Repeat::Off.next(), Repeat::Playlist);
        assert_eq!(Repeat::Playlist.next(), Repeat::Track);
        assert_eq!(Repeat::Track.next(), Repeat::Off);
    }

    #[test]
    fn free_account_is_not_the_same_as_no_account() {
        let mut s = State::default();
        assert!(!s.css_classes().contains(&"no-premium".to_string()));
        s.spotify.authorized = true;
        s.spotify.premium = Some(false);
        assert!(s.css_classes().contains(&"no-premium".to_string()));
        assert!(!s.spotify.can_control_remote());
    }

    #[test]
    fn remote_control_needs_both_an_account_and_premium() {
        // The volume route depends on this, and getting it wrong means either a
        // slider that silently does nothing or one hidden when it would work.
        let mut spotify = Spotify::default();
        assert!(!spotify.can_control_remote(), "no account means no remote control");

        spotify.authorized = true;
        assert!(
            spotify.can_control_remote(),
            "unknown must not hide a control that probably works; a refusal records it"
        );

        spotify.premium = Some(false);
        assert!(!spotify.can_control_remote(), "a free account cannot write to /me/player");

        spotify.premium = Some(true);
        assert!(spotify.can_control_remote());
    }

    #[test]
    fn contrast_ratio_matches_wcag_extremes() {
        let black = Rgb { r: 0, g: 0, b: 0 };
        let white = Rgb { r: 255, g: 255, b: 255 };
        assert!((black.contrast(white) - 21.0).abs() < 0.01);
        assert!((white.contrast(white) - 1.0).abs() < 0.01);
    }
}
