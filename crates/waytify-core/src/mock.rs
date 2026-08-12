//! A fake MPRIS player, for testing and for debugging an install.
//!
//! Two uses. The integration tests drive it to check the engine against a real
//! bus without needing a real player, and `waytify mock-player` runs it as a
//! command so a contributor can exercise the window without opening Spotify or
//! making any sound.
//!
//! It is deliberately awkward in the same ways real players are: the track id is
//! a plain string rather than an object path, and nothing is emitted that the
//! spec does not require. Code that only works against a well-behaved mock is
//! code that has not been tested.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use zbus::Connection;
use zbus::zvariant::{OwnedValue, Value};

/// Bus name suffix. The full name is `org.mpris.MediaPlayer2.waytifymock`.
pub const SUFFIX: &str = "waytifymock";
pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.waytifymock";
pub const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";

/// Transport calls received, so a test can assert what actually reached the player.
#[derive(Debug, Default)]
pub struct Calls {
    pub next: AtomicUsize,
    pub previous: AtomicUsize,
    pub play_pause: AtomicUsize,
    pub seek: AtomicUsize,
}

/// One entry in the mock's playlist.
#[derive(Debug, Clone)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub length_us: i64,
    /// Left empty by default. The window renders its no-art placeholder, which is
    /// worth being able to look at.
    pub art_url: String,
    /// `xesam:url`. Empty by default, which keeps the mock a generic MPRIS player.
    /// Setting it to a Spotify link is what turns on the parts of the engine that
    /// only run for Spotify's own playback.
    pub url: String,
}

impl Track {
    pub fn new(id: &str, title: &str, artist: &str, album: &str, seconds: i64) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            length_us: seconds * 1_000_000,
            art_url: String::new(),
            url: String::new(),
        }
    }
}

/// A small playlist with the shapes worth exercising: a normal track, one with no
/// album, and one long enough to make seeking visible.
///
/// Set `WAYTIFY_MOCK_ART` to a `file://` URL or an image path to give every entry
/// cover art. Without it the tracks have none, which is the case worth looking at
/// by default since it exercises the placeholder.
///
/// Set `WAYTIFY_MOCK_SPOTIFY=1` to hand every entry a Spotify `xesam:url`. The
/// engine keys its Spotify-only behaviour off that rather than off the player's
/// name, so this is what exercises the like button and the queue. The ids are
/// well formed but invented, which is enough for every call waytify makes: the
/// queue does not depend on them, and a saved-track check on an unknown id simply
/// answers no.
pub fn sample_playlist() -> Vec<Track> {
    let art = std::env::var("WAYTIFY_MOCK_ART").ok().map(|value| {
        if value.starts_with("file://") || value.starts_with("http") {
            value
        } else {
            format!("file://{value}")
        }
    });

    let spotify = std::env::var("WAYTIFY_MOCK_SPOTIFY").is_ok_and(|v| v != "0");
    let mut ids =
        ["3hQMe0ovH7VVtLDsHqTuLL", "0kvNVh4G6mYqUOMDzcy8fW", "6TZ8CkKsYLhBw2LsUtGjMt"].into_iter();

    let mut prepare = |track: Track| Track {
        art_url: art.clone().unwrap_or_default(),
        url: match ids.next() {
            Some(id) if spotify => format!("https://open.spotify.com/track/{id}"),
            _ => String::new(),
        },
        ..track
    };
    let with_art = &mut prepare;

    vec![
        with_art(Track::new("mock:track:1", "Digital Love", "Daft Punk", "Discovery", 301)),
        with_art(Track {
            album: String::new(),
            ..Track::new("mock:track:2", "Untitled Demo", "Someone", "", 187)
        }),
        with_art(Track::new(
            "mock:track:3",
            "A Very Long One",
            "Test Artist",
            "Long Player",
            1_805,
        )),
    ]
}

pub struct Player {
    pub playlist: Vec<Track>,
    pub index: usize,
    pub status: String,
    pub position_us: i64,
    pub shuffle: bool,
    pub loop_status: String,
    pub calls: Arc<Calls>,
}

impl Player {
    pub fn new(playlist: Vec<Track>, calls: Arc<Calls>) -> Self {
        Self {
            playlist,
            index: 0,
            status: "Playing".into(),
            position_us: 0,
            shuffle: false,
            loop_status: "None".into(),
            calls,
        }
    }

    fn track(&self) -> Option<&Track> {
        self.playlist.get(self.index)
    }

    fn metadata_map(&self) -> HashMap<String, OwnedValue> {
        let mut m = HashMap::new();
        let Some(track) = self.track() else { return m };

        let mut put = |key: &str, value: Value<'static>| {
            if let Ok(v) = OwnedValue::try_from(value) {
                m.insert(key.to_string(), v);
            }
        };
        // A plain string, not an object path, which is what Spotify sends and what
        // forces the absolute-seek fallback in the engine.
        put("mpris:trackid", Value::from(track.id.clone()));
        put("mpris:length", Value::from(track.length_us));
        put("xesam:title", Value::from(track.title.clone()));
        put("xesam:artist", Value::from(vec![track.artist.clone()]));
        if !track.album.is_empty() {
            put("xesam:album", Value::from(track.album.clone()));
        }
        if !track.art_url.is_empty() {
            put("mpris:artUrl", Value::from(track.art_url.clone()));
        }
        if !track.url.is_empty() {
            put("xesam:url", Value::from(track.url.clone()));
        }
        m
    }

    fn advance(&mut self, delta: isize) {
        if self.playlist.is_empty() {
            return;
        }
        let len = self.playlist.len() as isize;
        self.index = (((self.index as isize + delta) % len + len) % len) as usize;
        self.position_us = 0;
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    // Every method that changes a property announces it, because a real player
    // does. Mutating state without emitting would make the mock quieter than
    // anything it stands in for, and quiet is exactly the condition the engine
    // most needs to be tested against.
    async fn next(
        &mut self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        self.calls.next.fetch_add(1, Ordering::SeqCst);
        self.advance(1);
        let _ = self.metadata_changed(&emitter).await;
    }

    async fn previous(
        &mut self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        self.calls.previous.fetch_add(1, Ordering::SeqCst);
        self.advance(-1);
        let _ = self.metadata_changed(&emitter).await;
    }

    async fn play_pause(
        &mut self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        self.calls.play_pause.fetch_add(1, Ordering::SeqCst);
        self.status = if self.status == "Playing" { "Paused".into() } else { "Playing".into() };
        let _ = self.playback_status_changed(&emitter).await;
    }

    async fn play(
        &mut self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        self.status = "Playing".into();
        let _ = self.playback_status_changed(&emitter).await;
    }

    async fn pause(
        &mut self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        self.status = "Paused".into();
        let _ = self.playback_status_changed(&emitter).await;
    }

    async fn stop(
        &mut self,
        #[zbus(signal_emitter)] emitter: zbus::object_server::SignalEmitter<'_>,
    ) {
        self.status = "Stopped".into();
        self.position_us = 0;
        let _ = self.playback_status_changed(&emitter).await;
    }

    async fn seek(&mut self, offset: i64) {
        self.calls.seek.fetch_add(1, Ordering::SeqCst);
        let length = self.track().map_or(i64::MAX, |t| t.length_us);
        self.position_us = (self.position_us + offset).clamp(0, length);
    }

    /// Takes an object path, per the spec. The engine falls back to a relative
    /// seek when a track id cannot be turned into one, which is the path this
    /// mock exercises by using string ids.
    async fn set_position(&mut self, _track: zbus::zvariant::ObjectPath<'_>, position: i64) {
        self.calls.seek.fetch_add(1, Ordering::SeqCst);
        let length = self.track().map_or(i64::MAX, |t| t.length_us);
        self.position_us = position.clamp(0, length);
    }

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        self.status.clone()
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        self.metadata_map()
    }

    // Not cached, matching how the engine declares it: position changes without
    // any notification, so a cached value would be wrong immediately.
    #[zbus(property(emits_changed_signal = "false"))]
    async fn position(&self) -> i64 {
        self.position_us
    }

    #[zbus(property)]
    async fn shuffle(&self) -> bool {
        self.shuffle
    }

    #[zbus(property)]
    async fn set_shuffle(&mut self, value: bool) {
        self.shuffle = value;
    }

    #[zbus(property)]
    async fn loop_status(&self) -> String {
        self.loop_status.clone()
    }

    #[zbus(property)]
    async fn set_loop_status(&mut self, value: String) {
        self.loop_status = value;
    }

    #[zbus(property)]
    async fn can_control(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn can_go_previous(&self) -> bool {
        true
    }
}

pub struct Root {
    pub identity: String,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    async fn raise(&self) {}
    async fn quit(&self) {}

    #[zbus(property)]
    async fn identity(&self) -> String {
        self.identity.clone()
    }

    #[zbus(property)]
    async fn can_raise(&self) -> bool {
        true
    }

    #[zbus(property)]
    async fn has_track_list(&self) -> bool {
        // Matching Spotify, which exposes no track list. The queue has to come
        // from somewhere other than MPRIS.
        false
    }
}

/// Serve the mock on an existing connection without claiming the bus name.
///
/// Split out so tests can decide for themselves whether to request the name,
/// which they may not get if a previous run has not finished releasing it.
pub async fn serve(conn: &Connection, player: Player, identity: &str) -> Result<()> {
    conn.object_server()
        .at(OBJECT_PATH, player)
        .await
        .context("serving the mock Player interface")?;
    conn.object_server()
        .at(OBJECT_PATH, Root { identity: identity.into() })
        .await
        .context("serving the mock root interface")?;
    Ok(())
}

/// Run the mock until interrupted. This is what `waytify mock-player` calls.
///
/// Advances position once a second while playing and announces changes, so the
/// window has something moving to render without any audio being produced.
pub async fn run_standalone() -> Result<()> {
    let conn = Connection::session().await.context("connecting to the session bus")?;
    let calls = Arc::new(Calls::default());
    serve(&conn, Player::new(sample_playlist(), Arc::clone(&calls)), "Waytify Mock").await?;

    conn.request_name(BUS_NAME)
        .await
        .with_context(|| format!("claiming {BUS_NAME}; is another mock already running?"))?;

    tracing::info!("mock player on {BUS_NAME}, press ctrl-c to stop");

    let iface = conn
        .object_server()
        .interface::<_, Player>(OBJECT_PATH)
        .await
        .context("looking up the served interface")?;

    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    let mut last = Snapshot::default();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("stopping");
                return Ok(());
            }
            _ = ticker.tick() => {
                let mut guard = iface.get_mut().await;
                if guard.status == "Playing" {
                    let length = guard.track().map_or(i64::MAX, |t| t.length_us);
                    guard.position_us = (guard.position_us + 1_000_000).min(length);
                    // Roll over at the end, the way a real player would.
                    if guard.position_us >= length {
                        guard.advance(1);
                    }
                }

                // Announce only what actually changed. A player that re-emits
                // everything every second would hide ordering bugs in the engine
                // rather than expose them.
                let now = Snapshot::of(&guard);
                if now.track != last.track {
                    guard.metadata_changed(iface.signal_emitter()).await?;
                }
                if now.status != last.status {
                    guard.playback_status_changed(iface.signal_emitter()).await?;
                }
                last = now;
            }
        }
    }
}

#[derive(Default, PartialEq)]
struct Snapshot {
    track: String,
    status: String,
}

impl Snapshot {
    fn of(player: &Player) -> Self {
        Self {
            track: player.track().map(|t| t.id.clone()).unwrap_or_default(),
            status: player.status.clone(),
        }
    }
}
