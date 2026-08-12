//! MPRIS over D-Bus: proxies, discovery, and choosing which player to follow.

use waytify_ipc::{Repeat, Status};

/// Every MPRIS player claims a bus name starting with this.
pub const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// Bus names that look like players but are not.
///
/// `playerctld` is a proxy: it re-exports whichever player was last active under
/// its own name. Left in the list it shows up as a second player mirroring the
/// real one, and every state change appears to arrive twice.
pub const IGNORED: &[&str] = &["playerctld"];

/// The `org.mpris.MediaPlayer2` interface, for identity and window raising.
#[zbus::proxy(interface = "org.mpris.MediaPlayer2", default_path = "/org/mpris/MediaPlayer2")]
pub trait MediaPlayer2 {
    fn raise(&self) -> zbus::Result<()>;
    fn quit(&self) -> zbus::Result<()>;

    /// Human readable name, for example `Spotify`.
    #[zbus(property)]
    fn identity(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn can_raise(&self) -> zbus::Result<bool>;
}

/// The `org.mpris.MediaPlayer2.Player` interface: transport, metadata, position.
#[zbus::proxy(
    interface = "org.mpris.MediaPlayer2.Player",
    default_path = "/org/mpris/MediaPlayer2"
)]
pub trait Player {
    fn next(&self) -> zbus::Result<()>;
    fn previous(&self) -> zbus::Result<()>;
    fn pause(&self) -> zbus::Result<()>;
    fn play(&self) -> zbus::Result<()>;
    fn play_pause(&self) -> zbus::Result<()>;
    fn stop(&self) -> zbus::Result<()>;

    /// Relative seek, in microseconds. Negative rewinds.
    fn seek(&self, offset: i64) -> zbus::Result<()>;

    /// Absolute seek, in microseconds.
    ///
    /// The track id guards against a seek landing on the wrong song when the
    /// track changes between the read and the write.
    fn set_position(
        &self,
        track_id: &zbus::zvariant::ObjectPath<'_>,
        position: i64,
    ) -> zbus::Result<()>;

    /// Deliberately uncached.
    ///
    /// zbus caches properties that announce changes, and its cache is updated by
    /// its own signal handler. Reading through that cache while processing the
    /// very signal that updates it is a race, and this is the one property where
    /// reading a stale value is visible: the bar shows paused while music plays.
    /// A real call costs one round trip on an event that is already rare.
    #[zbus(property(emits_changed_signal = "false"))]
    fn playback_status(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn loop_status(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_loop_status(&self, value: &str) -> zbus::Result<()>;

    #[zbus(property)]
    fn shuffle(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_shuffle(&self, value: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn metadata(&self) -> zbus::Result<crate::metadata::Metadata>;

    /// Position in microseconds.
    ///
    /// Deliberately not cached by zbus: the spec marks this property as one that
    /// does not emit change notifications, so a cached read would return whatever
    /// it saw first and never update.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> zbus::Result<i64>;

    #[zbus(property)]
    fn can_seek(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_go_next(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_go_previous(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn can_control(&self) -> zbus::Result<bool>;

    /// Emitted on seek. Not every player sends it, which is why the position
    /// clock does not depend on it.
    #[zbus(signal)]
    fn seeked(&self, position: i64) -> zbus::Result<()>;
}

/// Whether a bus name is a player worth following.
pub fn is_player_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix(MPRIS_PREFIX) else {
        return false;
    };
    if suffix.is_empty() {
        return false;
    }
    // Instance names look like `vlc.instance1234`, so compare on the leading segment.
    let base = suffix.split('.').next().unwrap_or(suffix);
    !IGNORED.contains(&base)
}

/// The short name a user would type in config, for example `spotify`.
pub fn short_name(bus_name: &str) -> &str {
    let suffix = bus_name.strip_prefix(MPRIS_PREFIX).unwrap_or(bus_name);
    suffix.split('.').next().unwrap_or(suffix)
}

pub fn parse_status(s: &str) -> Status {
    match s {
        "Playing" => Status::Playing,
        "Paused" => Status::Paused,
        _ => Status::Stopped,
    }
}

pub fn parse_repeat(s: &str) -> Repeat {
    match s {
        "Track" => Repeat::Track,
        "Playlist" => Repeat::Playlist,
        _ => Repeat::Off,
    }
}

pub fn repeat_to_mpris(r: Repeat) -> &'static str {
    match r {
        Repeat::Off => "None",
        Repeat::Track => "Track",
        Repeat::Playlist => "Playlist",
    }
}

/// A player seen on the bus, reduced to what selection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub bus_name: String,
    pub status: Status,
}

/// Pick the player to follow.
///
/// The ordering tries to match what someone would point at if asked which player
/// they meant. Something actually playing wins over something idle, because that
/// is what the user is listening to. Within that, the configured preference order
/// decides, so a paused Spotify still beats a paused Firefox tab when Spotify is
/// listed first. Ties break on name so the choice is stable across restarts
/// rather than following hash order.
///
/// `only`, when it is not empty, removes everything unlisted from consideration
/// first. That is a different statement from preference: preference decides who
/// wins, this decides who is playing at all.
pub fn select<'a>(
    candidates: &'a [Candidate],
    preferred: &[String],
    only: &[String],
) -> Option<&'a Candidate> {
    candidates.iter().filter(|c| only.is_empty() || names_match(c, only)).max_by(|a, b| {
        rank(a, preferred).cmp(&rank(b, preferred)).then_with(|| b.bus_name.cmp(&a.bus_name))
    })
}

/// Whether a player is named in a list, by short name or by full bus name.
fn names_match(c: &Candidate, names: &[String]) -> bool {
    let short = short_name(&c.bus_name);
    names.iter().any(|n| n.eq_ignore_ascii_case(short) || n.eq_ignore_ascii_case(&c.bus_name))
}

fn rank(c: &Candidate, preferred: &[String]) -> (u8, usize) {
    let short = short_name(&c.bus_name);
    let pref = preferred
        .iter()
        .position(|p| p.eq_ignore_ascii_case(short) || p.eq_ignore_ascii_case(&c.bus_name))
        // Earlier in the list should score higher, so invert the index. Anything
        // unlisted scores zero and loses to everything listed.
        .map_or(0, |i| preferred.len() - i);
    (u8::from(c.status.is_playing()), pref)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, status: Status) -> Candidate {
        Candidate { bus_name: format!("{MPRIS_PREFIX}{name}"), status }
    }

    #[test]
    fn playerctld_is_not_a_player() {
        // It mirrors whichever player is active, so following it double counts.
        assert!(!is_player_name("org.mpris.MediaPlayer2.playerctld"));
    }

    #[test]
    fn real_players_are_recognised() {
        assert!(is_player_name("org.mpris.MediaPlayer2.spotify"));
        assert!(is_player_name("org.mpris.MediaPlayer2.mpv"));
        assert!(is_player_name("org.mpris.MediaPlayer2.firefox.instance_1_15"));
    }

    #[test]
    fn unrelated_bus_names_are_ignored() {
        assert!(!is_player_name("org.freedesktop.DBus"));
        assert!(!is_player_name("org.mpris.MediaPlayer2"));
        assert!(!is_player_name("org.mpris.MediaPlayer2."));
    }

    #[test]
    fn short_name_strips_prefix_and_instance() {
        assert_eq!(short_name("org.mpris.MediaPlayer2.spotify"), "spotify");
        assert_eq!(short_name("org.mpris.MediaPlayer2.vlc.instance7"), "vlc");
    }

    #[test]
    fn nothing_playing_means_no_selection() {
        assert!(select(&[], &[], &[]).is_none());
    }

    #[test]
    fn a_playing_player_beats_a_paused_one() {
        let list = [c("spotify", Status::Paused), c("mpv", Status::Playing)];
        // No preference configured, so what is audible wins.
        assert_eq!(select(&list, &[], &[]).unwrap().bus_name, "org.mpris.MediaPlayer2.mpv");
    }

    #[test]
    fn preference_decides_between_two_idle_players() {
        let list = [c("firefox", Status::Paused), c("spotify", Status::Paused)];
        let pref = vec!["spotify".to_string()];
        assert_eq!(select(&list, &pref, &[]).unwrap().bus_name, "org.mpris.MediaPlayer2.spotify");
    }

    #[test]
    fn only_hides_everything_it_does_not_name() {
        let candidates =
            [c("spotify", Status::Paused), c("firefox.instance_1_15", Status::Playing)];
        let only = ["spotify".to_string()];

        // Without it, a playing video wins over a paused Spotify, which is the
        // right answer for a media widget and the wrong one for someone who
        // asked for Spotify.
        let any = select(&candidates, &[], &[]).unwrap();
        assert!(any.bus_name.contains("firefox"));

        let picked = select(&candidates, &[], &only).unwrap();
        assert!(picked.bus_name.ends_with("spotify"), "paused Spotify beats a video it excludes");

        // Nothing listed running means nothing at all, rather than falling back
        // to whatever else happens to be there.
        assert!(select(&[c("firefox.instance_1_15", Status::Playing)], &[], &only).is_none());

        // A full bus name works as well as a short one.
        let full = ["org.mpris.MediaPlayer2.spotify".to_string()];
        assert!(select(&candidates, &[], &full).is_some());
    }

    #[test]
    fn preference_does_not_override_what_is_audible() {
        // Spotify is preferred but paused while a video is playing. Showing the
        // paused one would mean the bar disagrees with the speakers.
        let list = [c("spotify", Status::Paused), c("mpv", Status::Playing)];
        let pref = vec!["spotify".to_string()];
        assert_eq!(select(&list, &pref, &[]).unwrap().bus_name, "org.mpris.MediaPlayer2.mpv");
    }

    #[test]
    fn preference_order_is_respected_among_equals() {
        let list = [c("mpv", Status::Playing), c("spotify", Status::Playing)];
        let pref = vec!["spotify".to_string(), "mpv".to_string()];
        assert_eq!(select(&list, &pref, &[]).unwrap().bus_name, "org.mpris.MediaPlayer2.spotify");

        let pref = vec!["mpv".to_string(), "spotify".to_string()];
        assert_eq!(select(&list, &pref, &[]).unwrap().bus_name, "org.mpris.MediaPlayer2.mpv");
    }

    #[test]
    fn preference_matches_a_full_bus_name_too() {
        let list = [c("firefox", Status::Paused), c("spotify", Status::Paused)];
        let pref = vec!["org.mpris.MediaPlayer2.firefox".to_string()];
        assert_eq!(select(&list, &pref, &[]).unwrap().bus_name, "org.mpris.MediaPlayer2.firefox");
    }

    #[test]
    fn selection_is_stable_when_nothing_distinguishes_players() {
        // Hash order would make the bar flip between players across restarts.
        let list = [c("mpv", Status::Paused), c("spotify", Status::Paused)];
        let reversed = [c("spotify", Status::Paused), c("mpv", Status::Paused)];
        assert_eq!(select(&list, &[], &[]).unwrap(), select(&reversed, &[], &[]).unwrap());
    }

    #[test]
    fn status_strings_map_and_unknown_values_stop_playback() {
        assert_eq!(parse_status("Playing"), Status::Playing);
        assert_eq!(parse_status("Paused"), Status::Paused);
        assert_eq!(parse_status("Stopped"), Status::Stopped);
        assert_eq!(parse_status("nonsense"), Status::Stopped);
    }

    #[test]
    fn repeat_round_trips_through_the_mpris_spelling() {
        for r in [Repeat::Off, Repeat::Track, Repeat::Playlist] {
            assert_eq!(parse_repeat(repeat_to_mpris(r)), r);
        }
        assert_eq!(parse_repeat("None"), Repeat::Off);
    }
}
