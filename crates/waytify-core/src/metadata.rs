//! Turning an MPRIS `Metadata` dictionary into a [`Track`].
//!
//! The spec describes this map loosely and players interpret it loosely in turn.
//! `mpris:trackid` is meant to be an object path but arrives as a plain string
//! from some players. `xesam:artist` is meant to be an array of strings but
//! arrives as a bare string from others. Lengths show up as `i64` or `u64`
//! depending on who is sending.
//!
//! So nothing here trusts a declared type. Every accessor tries the shapes seen
//! in the wild and gives up quietly, because a player with one odd field should
//! degrade to a missing album name rather than no track at all.

use std::collections::HashMap;
use waytify_ipc::Track;
use zbus::zvariant::{OwnedValue, Value};

/// Metadata as it arrives over D-Bus.
pub type Metadata = HashMap<String, OwnedValue>;

/// Build a track from a metadata dictionary.
///
/// Returns `None` when there is nothing worth showing. Players send an empty or
/// near-empty map when stopped, and an entry with no title is not a track.
pub fn track_from_metadata(md: &Metadata) -> Option<Track> {
    let title = get_string(md, "xesam:title").filter(|s| !s.trim().is_empty())?;

    let artists = get_strings(md, "xesam:artist");
    let artists = if artists.is_empty() { get_strings(md, "xesam:albumArtist") } else { artists };

    Some(Track {
        id: get_string(md, "mpris:trackid"),
        title,
        artists,
        album: get_string(md, "xesam:album").filter(|s| !s.trim().is_empty()),
        // MPRIS lengths are microseconds. Zero means unknown, not instantaneous.
        length_ms: get_u64(md, "mpris:length").map(|us| us / 1_000).filter(|ms| *ms > 0),
        art_url: get_string(md, "mpris:artUrl").map(|u| normalize_art_url(&u)),
        art_path: None,
        colors: None,
        liked: None,
        url: get_string(md, "xesam:url"),
    })
}

/// Older Spotify builds hand out `open.spotify.com/image/<hash>` in `mpris:artUrl`,
/// which is not a real image endpoint. The CDN path with the same hash is.
///
/// Harmless on any URL that does not match, so it runs unconditionally rather
/// than behind a player check.
pub fn normalize_art_url(url: &str) -> String {
    match url.strip_prefix("https://open.spotify.com/image/") {
        Some(hash) => format!("https://i.scdn.co/image/{hash}"),
        None => url.to_string(),
    }
}

/// The Spotify catalogue id for a track, when this looks like a Spotify track.
///
/// Prefers `xesam:url` because it is a documented public URL. Falls back to the
/// trailing segment of `mpris:trackid`, which Spotify formats as
/// `/com/spotify/track/<id>`. Returns `None` for anything else, which is how a
/// local file or another player stays out of the Web API code paths entirely.
pub fn spotify_track_id(track: &Track) -> Option<String> {
    if let Some(url) = &track.url
        && let Some(rest) = url.strip_prefix("https://open.spotify.com/track/")
    {
        let id = rest.split(['?', '#', '/']).next().unwrap_or(rest);
        if is_base62(id) {
            return Some(id.to_string());
        }
    }

    let id = track.id.as_ref()?;
    let tail = id.rsplit('/').next()?;
    // `spotify:track:<id>` shows up too, depending on client version.
    let tail = tail.rsplit(':').next().unwrap_or(tail);
    (id.contains("spotify") && is_base62(tail)).then(|| tail.to_string())
}

/// Spotify ids are 22 characters of base62. Checking the shape avoids sending
/// obvious junk to the API and burning rate limit on a guaranteed 400.
fn is_base62(s: &str) -> bool {
    s.len() == 22 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn get_string(md: &Metadata, key: &str) -> Option<String> {
    as_string(md.get(key)?)
}

fn get_u64(md: &Metadata, key: &str) -> Option<u64> {
    as_u64(md.get(key)?)
}

fn get_strings(md: &Metadata, key: &str) -> Vec<String> {
    md.get(key).map(|v| as_strings(v)).unwrap_or_default()
}

fn as_string(v: &Value<'_>) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        // A variant wrapping the real value, which some players nest.
        Value::Value(inner) => as_string(inner),
        _ => None,
    }
}

fn as_u64(v: &Value<'_>) -> Option<u64> {
    match v {
        Value::U64(n) => Some(*n),
        Value::U32(n) => Some(u64::from(*n)),
        Value::U16(n) => Some(u64::from(*n)),
        Value::I64(n) => u64::try_from(*n).ok(),
        Value::I32(n) => u64::try_from(*n).ok(),
        Value::I16(n) => u64::try_from(*n).ok(),
        Value::F64(n) if *n >= 0.0 => Some(*n as u64),
        Value::Value(inner) => as_u64(inner),
        _ => None,
    }
}

fn as_strings(v: &Value<'_>) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(as_string).collect(),
        Value::Value(inner) => as_strings(inner),
        // A player that sends a bare string where an array belongs still meant
        // one artist, so read it as one rather than dropping it.
        other => as_string(other).into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(pairs: Vec<(&str, Value<'static>)>) -> Metadata {
        pairs.into_iter().map(|(k, v)| (k.to_string(), OwnedValue::try_from(v).unwrap())).collect()
    }

    fn spotify_like() -> Metadata {
        md(vec![
            ("mpris:trackid", Value::from("/com/spotify/track/4uLU6hMCjMI75M1A2tKUQC")),
            ("mpris:length", Value::from(215_000_000u64)),
            ("mpris:artUrl", Value::from("https://i.scdn.co/image/ab67616d0000b273abc")),
            ("xesam:title", Value::from("Digital Love")),
            ("xesam:artist", Value::from(vec!["Daft Punk"])),
            ("xesam:album", Value::from("Discovery")),
            ("xesam:url", Value::from("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC")),
        ])
    }

    #[test]
    fn parses_a_normal_spotify_track() {
        let t = track_from_metadata(&spotify_like()).unwrap();
        assert_eq!(t.title, "Digital Love");
        assert_eq!(t.artists, vec!["Daft Punk"]);
        assert_eq!(t.album.as_deref(), Some("Discovery"));
        assert_eq!(t.length_ms, Some(215_000), "microseconds must become milliseconds");
    }

    #[test]
    fn empty_metadata_is_not_a_track() {
        // Players send this when stopped. Inventing a blank track here would put
        // an empty label in the bar.
        assert!(track_from_metadata(&md(vec![])).is_none());
    }

    #[test]
    fn a_blank_title_is_not_a_track() {
        let m = md(vec![("xesam:title", Value::from("   "))]);
        assert!(track_from_metadata(&m).is_none());
    }

    #[test]
    fn zero_length_reads_as_unknown() {
        // Live streams and some podcast clients report zero rather than omitting
        // the key. A zero-length track would make the scrubber divide by zero.
        let m = md(vec![("xesam:title", Value::from("x")), ("mpris:length", Value::from(0u64))]);
        assert_eq!(track_from_metadata(&m).unwrap().length_ms, None);
    }

    #[test]
    fn a_bare_string_artist_is_still_an_artist() {
        // The spec says array of strings. Not every player agrees.
        let m = md(vec![
            ("xesam:title", Value::from("x")),
            ("xesam:artist", Value::from("Solo Artist")),
        ]);
        assert_eq!(track_from_metadata(&m).unwrap().artists, vec!["Solo Artist"]);
    }

    #[test]
    fn album_artist_covers_a_missing_artist() {
        let m = md(vec![
            ("xesam:title", Value::from("x")),
            ("xesam:albumArtist", Value::from(vec!["Various"])),
        ]);
        assert_eq!(track_from_metadata(&m).unwrap().artists, vec!["Various"]);
    }

    #[test]
    fn signed_lengths_are_accepted() {
        // Spotify sends i64 here, other players send u64.
        let m = md(vec![
            ("xesam:title", Value::from("x")),
            ("mpris:length", Value::from(215_000_000i64)),
        ]);
        assert_eq!(track_from_metadata(&m).unwrap().length_ms, Some(215_000));
    }

    #[test]
    fn negative_lengths_are_discarded_rather_than_wrapping() {
        let m = md(vec![("xesam:title", Value::from("x")), ("mpris:length", Value::from(-1i64))]);
        assert_eq!(track_from_metadata(&m).unwrap().length_ms, None);
    }

    #[test]
    fn legacy_spotify_art_urls_are_rewritten_to_the_cdn() {
        assert_eq!(
            normalize_art_url("https://open.spotify.com/image/ab67616d0000b273abc"),
            "https://i.scdn.co/image/ab67616d0000b273abc"
        );
    }

    #[test]
    fn other_art_urls_pass_through_untouched() {
        let file = "file:///home/u/.cache/art/cover.png";
        assert_eq!(normalize_art_url(file), file);
    }

    #[test]
    fn spotify_id_comes_from_the_public_url_first() {
        let t = track_from_metadata(&spotify_like()).unwrap();
        assert_eq!(spotify_track_id(&t).as_deref(), Some("4uLU6hMCjMI75M1A2tKUQC"));
    }

    #[test]
    fn spotify_id_falls_back_to_the_trackid_path() {
        let mut t = track_from_metadata(&spotify_like()).unwrap();
        t.url = None;
        assert_eq!(spotify_track_id(&t).as_deref(), Some("4uLU6hMCjMI75M1A2tKUQC"));
    }

    #[test]
    fn local_files_have_no_spotify_id() {
        // This is what keeps mpv and local playback out of the Web API paths.
        let m = md(vec![
            ("mpris:trackid", Value::from("/org/mpris/MediaPlayer2/Track/7")),
            ("xesam:title", Value::from("A local file")),
        ]);
        let t = track_from_metadata(&m).unwrap();
        assert_eq!(spotify_track_id(&t), None);
    }

    #[test]
    fn a_malformed_spotify_id_is_rejected() {
        let mut t = track_from_metadata(&spotify_like()).unwrap();
        t.url = Some("https://open.spotify.com/track/short".into());
        t.id = None;
        assert_eq!(spotify_track_id(&t), None, "wrong length should not reach the API");
    }

    #[test]
    fn object_path_trackids_are_read_as_strings() {
        let m = md(vec![
            ("xesam:title", Value::from("x")),
            (
                "mpris:trackid",
                Value::ObjectPath(
                    zbus::zvariant::ObjectPath::try_from("/com/spotify/track/abc").unwrap(),
                ),
            ),
        ]);
        assert_eq!(track_from_metadata(&m).unwrap().id.as_deref(), Some("/com/spotify/track/abc"));
    }
}
