//! Lyrics from [lrclib](https://lrclib.net), parsed, cached, and timed.
//!
//! lrclib is the only free lyrics source with synced timings and no account, no
//! key, and no per-request signing. It asks for a User-Agent that identifies the
//! client, which is the whole of its terms.
//!
//! Most tracks have no synced lyrics, so misses are cached as well as hits. A
//! popup that opens on a track with no lyrics would otherwise ask again on every
//! open, forever, for an answer that is nearly always the same.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::time::{Duration, SystemTime};
use waytify_ipc::{LyricLine, Lyrics, Track, paths};

const API: &str = "https://lrclib.net/api";

const TIMEOUT: Duration = Duration::from_secs(10);

/// How long a "no lyrics for this" answer is trusted.
///
/// lrclib is a contributed database, so a miss today can be a hit next month.
/// Long enough that a track played daily is not asked about daily, short enough
/// that a newly added transcription turns up without clearing the cache by hand.
const MISS_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// A duration this far from the one asked for is a different recording.
///
/// Live versions, radio edits and remasters share a title and an artist, and
/// lyrics timed against one of them scroll visibly wrong against another.
const DURATION_TOLERANCE_S: i64 = 5;

#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    instrumental: bool,
    #[serde(default)]
    #[serde(rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
}

/// Lyrics for a track, from the cache or from lrclib.
///
/// `Ok(None)` means the answer is "there are none", which is a result rather
/// than a failure and is cached as one. `Err` is reserved for not having got an
/// answer at all.
pub async fn fetch(track: &Track) -> Result<Option<Lyrics>> {
    let Some(key) = key_for(track) else {
        return Ok(None);
    };

    let dir = paths::lyrics_cache_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(format!("{key}.json"));

    if let Some(cached) = read_cache(&path) {
        return Ok(cached);
    }

    let found = lookup(track).await?;
    write_cache(&path, &found);
    Ok(found)
}

/// Cache identity for a track's lyrics.
///
/// Deliberately not the track id. The same recording played from Spotify, from a
/// file, or from a browser has three different ids and one set of lyrics, and
/// the duration is part of the identity because it is what distinguishes a live
/// take from the studio one.
pub fn key_for(track: &Track) -> Option<String> {
    if track.title.trim().is_empty() {
        return None;
    }
    let seconds = duration_s(track).unwrap_or(0);
    let identity = format!(
        "{}\u{1}{}\u{1}{seconds}",
        track.artist_line().to_lowercase(),
        track.title.to_lowercase()
    );

    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(identity.as_bytes());
    Some(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

/// A track's length in whole seconds, when the player has reported one.
///
/// Zero is not a length. Players publish it while a track is still loading, and
/// taking it at face value asks lrclib for a recording of no duration, which
/// matches nothing and then caches that as the answer.
fn duration_s(track: &Track) -> Option<i64> {
    match track.length_ms {
        Some(0) | None => None,
        Some(ms) => Some((ms / 1000) as i64),
    }
}

/// A cached answer, if there is one that is still trusted.
///
/// The outer `Option` is "was there a cache entry", the inner one is the answer
/// it held.
fn read_cache(path: &Path) -> Option<Option<Lyrics>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let cached: Option<Lyrics> = serde_json::from_str(&raw).ok()?;

    if cached.is_none() && is_stale(path) {
        return None;
    }
    Some(cached)
}

fn is_stale(path: &Path) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) else {
        return true;
    };
    SystemTime::now().duration_since(modified).is_ok_and(|age| age > MISS_LIFETIME)
}

fn write_cache(path: &Path, found: &Option<Lyrics>) {
    let Ok(encoded) = serde_json::to_string(found) else { return };
    // A miss rewritten in place refreshes its own expiry, which is what should
    // happen: it was checked again just now.
    if let Err(e) = std::fs::write(path, encoded) {
        tracing::debug!("could not cache lyrics: {e}");
    }
}

/// Ask lrclib, exact match first.
async fn lookup(track: &Track) -> Result<Option<Lyrics>> {
    let http = reqwest::Client::builder()
        .timeout(TIMEOUT)
        // lrclib asks clients to identify themselves and where to complain.
        .user_agent(concat!(
            "waytify/",
            env!("CARGO_PKG_VERSION"),
            " (https://github.com/IlyasKhallouki/waytify)"
        ))
        .build()?;

    let artist = track.artist_line();
    let seconds = duration_s(track);

    // The exact endpoint matches on all four fields at once and is the only one
    // that can be trusted without checking the answer, so it is worth asking
    // first even though it misses more often.
    if let Some(seconds) = seconds {
        let query = [
            ("artist_name", artist.clone()),
            ("track_name", track.title.clone()),
            ("album_name", track.album.clone().unwrap_or_default()),
            ("duration", seconds.to_string()),
        ];
        let response = http.get(format!("{API}/get")).query(&query).send().await?;
        if response.status().is_success() {
            let record: Record = response.json().await.context("reading the lrclib record")?;
            return Ok(convert(record));
        }
    }

    // The search endpoint ignores album and duration, so several recordings of
    // the same song come back together and the right one has to be picked out.
    let query = [("artist_name", artist), ("track_name", track.title.clone())];
    let response = http.get(format!("{API}/search")).query(&query).send().await?;
    if !response.status().is_success() {
        return Ok(None);
    }
    let records: Vec<Record> = response.json().await.context("reading the lrclib results")?;
    Ok(best_match(records, seconds).and_then(convert))
}

/// The result that is the same recording as the track being played.
///
/// With no duration to compare against there is nothing to choose by, and the
/// first result is as good a guess as any. With one, anything outside the
/// tolerance is a different recording and no answer beats a wrong one.
fn best_match(records: Vec<Record>, seconds: Option<i64>) -> Option<Record> {
    let Some(seconds) = seconds else {
        return records.into_iter().next();
    };
    records
        .into_iter()
        .filter_map(|r| {
            let off = (r.duration? as i64 - seconds).abs();
            (off <= DURATION_TOLERANCE_S).then_some((off, r))
        })
        .min_by_key(|(off, _)| *off)
        .map(|(_, r)| r)
}

/// An lrclib record as something the window can show, or nothing worth showing.
fn convert(record: Record) -> Option<Lyrics> {
    // An instrumental is an answer: there is nothing to display and asking again
    // will not change that.
    if record.instrumental {
        return None;
    }

    // Only timed lyrics are usable. The window shows the line being sung between
    // the one before and the one after, and a wall of untimed text has no line
    // being sung to put in the middle of it.
    let lines = record.synced_lyrics.as_deref().map(parse_lrc).unwrap_or_default();
    (!lines.is_empty()).then_some(Lyrics { lines })
}

/// Parse LRC into timed lines, in time order.
///
/// One line may carry several timestamps, which is how a repeated chorus is
/// written without repeating its text. Metadata tags such as `[ar:...]` are not
/// timestamps and are skipped.
///
/// Empty text is kept rather than dropped. lrclib marks instrumental breaks with
/// timed blank lines, and they are what stops the display from holding the last
/// line of a verse through a thirty second solo.
pub fn parse_lrc(raw: &str) -> Vec<LyricLine> {
    let mut lines = Vec::new();

    for line in raw.lines() {
        let mut rest = line.trim_start();
        let mut stamps = Vec::new();

        while let Some(close) = rest.strip_prefix('[').and_then(|r| r.find(']')) {
            let tag = &rest[1..=close];
            match parse_timestamp(tag) {
                Some(ms) => stamps.push(ms),
                // A metadata tag before any timestamp means the whole line is
                // metadata. One after a timestamp is part of the lyric text.
                None => break,
            }
            rest = &rest[close + 2..];
        }

        let text = rest.trim().to_string();
        for at_ms in stamps {
            lines.push(LyricLine { at_ms, text: text.clone() });
        }
    }

    lines.sort_by_key(|l| l.at_ms);
    lines
}

/// `mm:ss.xx` from inside the brackets, in milliseconds.
///
/// Some files separate the fraction with a colon rather than a dot, and it may
/// be two digits or three, so hundredths and thousandths both appear.
fn parse_timestamp(tag: &str) -> Option<u64> {
    let (minutes, rest) = tag.split_once(':')?;
    let minutes: u64 = minutes.trim().parse().ok()?;

    let (seconds, fraction) = match rest.split_once(['.', ':']) {
        Some((s, f)) => (s, f),
        None => (rest, ""),
    };
    let seconds: u64 = seconds.parse().ok()?;

    let millis = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u64>().ok()? * 100,
        2 => fraction.parse::<u64>().ok()? * 10,
        _ => fraction.get(..3)?.parse::<u64>().ok()?,
    };

    Some((minutes * 60 + seconds) * 1000 + millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artist: &str, seconds: u64) -> Track {
        Track {
            title: title.into(),
            artists: vec![artist.into()],
            length_ms: Some(seconds * 1000),
            ..Default::default()
        }
    }

    #[test]
    fn timestamps_parse_in_every_shape_seen_in_the_wild() {
        assert_eq!(parse_timestamp("00:12.34"), Some(12_340));
        assert_eq!(parse_timestamp("01:00.00"), Some(60_000));
        // Thousandths rather than hundredths.
        assert_eq!(parse_timestamp("00:12.345"), Some(12_345));
        // A colon for the fraction, which some editors write.
        assert_eq!(parse_timestamp("00:12:34"), Some(12_340));
        // No fraction at all.
        assert_eq!(parse_timestamp("02:07"), Some(127_000));
        // Past an hour, written as minutes rather than rolling over.
        assert_eq!(parse_timestamp("75:00.00"), Some(4_500_000));

        assert_eq!(parse_timestamp("ar:Someone"), None, "metadata is not a timestamp");
        assert_eq!(parse_timestamp("length"), None);
    }

    #[test]
    fn a_normal_lrc_file_parses_in_order() {
        let raw = "[ar:Someone]\n[ti:Something]\n\n[00:01.00]First\n[00:05.50]Second\n";
        let lines = parse_lrc(raw);
        assert_eq!(lines.len(), 2, "metadata tags are not lines");
        assert_eq!(lines[0].at_ms, 1_000);
        assert_eq!(lines[0].text, "First");
        assert_eq!(lines[1].at_ms, 5_500);
    }

    #[test]
    fn a_repeated_chorus_becomes_one_line_per_timestamp() {
        let lines = parse_lrc("[00:10.00][01:20.00][02:30.00]Chorus\n");
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().all(|l| l.text == "Chorus"));
        assert_eq!(lines.iter().map(|l| l.at_ms).collect::<Vec<_>>(), [10_000, 80_000, 150_000]);
    }

    #[test]
    fn out_of_order_files_are_sorted() {
        // Written by hand, or produced by merging two files.
        let lines = parse_lrc("[00:30.00]Later\n[00:10.00]Earlier\n");
        assert_eq!(lines[0].text, "Earlier");
        assert_eq!(lines[1].text, "Later");
    }

    #[test]
    fn timed_blank_lines_survive_parsing() {
        // What lrclib puts at an instrumental break. Dropping them leaves the
        // last line of the verse on screen through the whole solo.
        let lines = parse_lrc("[00:10.00]Verse\n[00:14.00]\n[00:40.00]Next\n");
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].text, "");
    }

    #[test]
    fn brackets_inside_a_lyric_are_not_timestamps() {
        let lines = parse_lrc("[00:10.00][laughs] something\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "[laughs] something");
    }

    #[test]
    fn the_highlighted_line_follows_the_position() {
        let lyrics = Lyrics { lines: parse_lrc("[00:10.00]A\n[00:20.00]B\n") };
        assert_eq!(lyrics.line_at(0), None, "nothing is highlighted during the intro");
        assert_eq!(lyrics.line_at(9_999), None);
        assert_eq!(lyrics.line_at(10_000), Some(0), "exactly on the line counts as reached");
        assert_eq!(lyrics.line_at(19_999), Some(0));
        assert_eq!(lyrics.line_at(20_000), Some(1));
        assert_eq!(lyrics.line_at(u64::MAX), Some(1), "the last line holds to the end");
    }

    #[test]
    fn the_closest_recording_wins_and_a_distant_one_is_refused() {
        let record = |duration: f64| Record {
            duration: Some(duration),
            instrumental: false,
            synced_lyrics: Some(format!("[00:01.00]{duration}")),
        };

        let picked = best_match(vec![record(200.0), record(182.0), record(300.0)], Some(180));
        assert_eq!(picked.and_then(|r| r.duration), Some(182.0));

        // A live version twice the length is not the recording being played, and
        // its timings would scroll visibly wrong.
        assert!(best_match(vec![record(360.0)], Some(180)).is_none());

        // Nothing to compare against, so the first answer is as good as any.
        assert_eq!(best_match(vec![record(360.0)], None).and_then(|r| r.duration), Some(360.0));
    }

    #[test]
    fn an_instrumental_is_an_answer_rather_than_lyrics() {
        let record = Record {
            duration: Some(180.0),
            instrumental: true,
            synced_lyrics: Some("[00:01.00]should be ignored".into()),
        };
        assert!(convert(record).is_none());
    }

    #[test]
    fn lyrics_with_no_timing_are_not_kept() {
        // Better nothing than text the window has no way to show. Carrying it
        // would put a field in the state that nothing ever renders.
        let record = Record { duration: Some(180.0), instrumental: false, synced_lyrics: None };
        assert!(convert(record).is_none());
    }

    /// Hits the real service, so it is not part of the normal run.
    ///
    /// Everything above tests the parsing and the choosing against fixed input.
    /// This is the one that would notice lrclib renaming a field or changing an
    /// endpoint, which no amount of local testing can.
    ///
    /// ```sh
    /// cargo test -p waytify-core -- --ignored lrclib
    /// ```
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn lrclib_still_answers_the_way_this_expects() {
        let found = lookup(&track("Digital Love", "Daft Punk", 301)).await.unwrap();
        let lyrics = found.expect("a track this well known has timed lyrics");
        assert!(lyrics.lines.len() > 10);
        assert!(lyrics.lines.windows(2).all(|w| w[0].at_ms <= w[1].at_ms), "in order");

        // The search fallback has to work on its own, since the exact endpoint
        // misses whenever a player reports an album or a length differently.
        let vague = Track { album: None, length_ms: None, ..track("Digital Love", "Daft Punk", 0) };
        let fallback = lookup(&vague).await.unwrap();
        assert!(fallback.is_some(), "the search fallback finds it without a duration");
    }

    #[test]
    fn the_cache_key_separates_recordings_and_joins_sources() {
        let spotify = Track { id: Some("spotify:track:abc".into()), ..track("Song", "Band", 180) };
        let local = Track { id: Some("/org/mpris/track/7".into()), ..track("Song", "Band", 180) };
        assert_eq!(
            key_for(&spotify),
            key_for(&local),
            "the same recording from two players shares one set of lyrics"
        );

        // Different length means a different recording, and lyrics timed for one
        // do not fit the other.
        assert_ne!(key_for(&track("Song", "Band", 180)), key_for(&track("Song", "Band", 240)));
        assert_ne!(key_for(&track("Song", "Band", 180)), key_for(&track("Song", "Other", 180)));

        // Case is not identity. Players disagree about capitalisation of the
        // same metadata often enough to matter.
        assert_eq!(key_for(&track("song", "band", 180)), key_for(&track("Song", "Band", 180)));

        assert_eq!(key_for(&track("", "Band", 180)), None, "nothing to look up");

        // A player that has not reported a length yet must not be cached as a
        // recording of zero seconds, which would then be the answer for the
        // whole track once the real length arrived.
        let loading = Track { length_ms: Some(0), ..track("Song", "Band", 0) };
        let unknown = Track { length_ms: None, ..track("Song", "Band", 0) };
        assert_eq!(key_for(&loading), key_for(&unknown));
        assert_eq!(duration_s(&loading), None);
        assert_eq!(duration_s(&track("Song", "Band", 180)), Some(180));
    }
}
