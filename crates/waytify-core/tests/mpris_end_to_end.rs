//! End to end tests against a mock MPRIS player served on the real session bus.
//!
//! The unit tests cover parsing and arithmetic. What they cannot cover is whether
//! the engine actually attaches to a player, reacts to a `PropertiesChanged`, and
//! sends transport calls to the right place. That needs a bus and something
//! listening on it, so this file provides one.
//!
//! The mock is deliberately as awkward as the real thing: it reports its track id
//! as a plain string rather than an object path, and it never emits `Seeked`.
//! Both are behaviours seen from real players, and both are what the engine is
//! built not to depend on.
//!
//! Skipped, not failed, when there is no session bus. That keeps the suite honest
//! on a build machine without one instead of pretending to have verified this.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use waytify_core::config::{Config, PlayerConfig};
use waytify_core::engine::{Engine, EngineMsg};
use waytify_ipc::{Command, Status};
use zbus::zvariant::{OwnedValue, Value};

/// Unique enough that a developer running the suite while listening to music does
/// not end up testing against their own player.
const MOCK_NAME: &str = "org.mpris.MediaPlayer2.waytifymock";
const MOCK_PATH: &str = "/org/mpris/MediaPlayer2";

#[derive(Default)]
struct Calls {
    next: AtomicUsize,
    previous: AtomicUsize,
    play_pause: AtomicUsize,
}

struct MockPlayer {
    status: String,
    title: String,
    artist: String,
    track_id: String,
    length_us: i64,
    position_us: i64,
    shuffle: bool,
    calls: Arc<Calls>,
}

impl MockPlayer {
    fn metadata_map(&self) -> HashMap<String, OwnedValue> {
        let mut m = HashMap::new();
        // A plain string, not an object path. Real players do this and it is what
        // forces the absolute-seek fallback.
        m.insert(
            "mpris:trackid".into(),
            OwnedValue::try_from(Value::from(self.track_id.clone())).unwrap(),
        );
        m.insert("mpris:length".into(), OwnedValue::try_from(Value::from(self.length_us)).unwrap());
        m.insert(
            "xesam:title".into(),
            OwnedValue::try_from(Value::from(self.title.clone())).unwrap(),
        );
        m.insert(
            "xesam:artist".into(),
            OwnedValue::try_from(Value::from(vec![self.artist.clone()])).unwrap(),
        );
        m
    }
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl MockPlayer {
    async fn next(&mut self) {
        self.calls.next.fetch_add(1, Ordering::SeqCst);
    }

    async fn previous(&mut self) {
        self.calls.previous.fetch_add(1, Ordering::SeqCst);
    }

    async fn play_pause(&mut self) {
        self.calls.play_pause.fetch_add(1, Ordering::SeqCst);
    }

    async fn play(&mut self) {}
    async fn pause(&mut self) {}
    async fn stop(&mut self) {}
    async fn seek(&mut self, offset: i64) {
        self.position_us = (self.position_us + offset).max(0);
    }

    #[zbus(property)]
    async fn playback_status(&self) -> String {
        self.status.clone()
    }

    #[zbus(property)]
    async fn metadata(&self) -> HashMap<String, OwnedValue> {
        self.metadata_map()
    }

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
    async fn can_control(&self) -> bool {
        true
    }
}

struct MockRoot;

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl MockRoot {
    async fn raise(&self) {}
    async fn quit(&self) {}

    #[zbus(property)]
    async fn identity(&self) -> String {
        "Waytify Mock".into()
    }

    #[zbus(property)]
    async fn can_raise(&self) -> bool {
        true
    }
}

/// Wait for a predicate to hold, polling the state channel.
///
/// D-Bus round trips have no fixed duration, so the alternative would be a sleep
/// long enough to be slow and short enough to be flaky.
async fn wait_for<F>(states: &mut tokio::sync::watch::Receiver<Arc<waytify_ipc::State>>, mut ok: F)
where
    F: FnMut(&waytify_ipc::State) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if ok(&states.borrow_and_update()) {
            return;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "state never satisfied the condition");
        let _ = tokio::time::timeout(remaining, states.changed()).await;
    }
}

#[tokio::test]
async fn engine_follows_a_live_player() {
    let Ok(conn) = zbus::Connection::session().await else {
        eprintln!("skipping: no session bus available");
        return;
    };

    let calls = Arc::new(Calls::default());
    let player = MockPlayer {
        status: "Playing".into(),
        title: "Digital Love".into(),
        artist: "Daft Punk".into(),
        track_id: "spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(),
        length_us: 301_000_000,
        position_us: 60_000_000,
        shuffle: false,
        calls: Arc::clone(&calls),
    };

    conn.object_server().at(MOCK_PATH, player).await.unwrap();
    conn.object_server().at(MOCK_PATH, MockRoot).await.unwrap();
    if conn.request_name(MOCK_NAME).await.is_err() {
        eprintln!("skipping: could not claim {MOCK_NAME}");
        return;
    }

    let config = Config {
        player: PlayerConfig { preferred: vec!["waytifymock".into()] },
        ..Default::default()
    };
    let engine = Engine::new(config).await.expect("engine should start");
    let mut states = engine.subscribe();
    let (tx, rx) = mpsc::channel(16);
    let engine_task = tokio::spawn(engine.run(rx));

    // The engine scans on startup, so the mock should already be attached.
    wait_for(&mut states, |s| s.track().map(|t| t.title.as_str()) == Some("Digital Love")).await;

    {
        let s = states.borrow_and_update();
        let p = s.player.as_ref().expect("a player should be attached");
        assert_eq!(p.identity, "Waytify Mock", "identity should come from the root interface");
        assert_eq!(p.status, Status::Playing);
        assert_eq!(s.track().unwrap().artists, vec!["Daft Punk"]);
        assert_eq!(s.track().unwrap().length_ms, Some(301_000));
        assert!(s.caps.can_seek, "a track with a known length is seekable");
        // Position is read from the player, not assumed to start at zero.
        assert!(p.position_ms >= 60_000, "position was {}", p.position_ms);
    }

    // Transport reaches the player.
    tx.send(EngineMsg::Command { command: Command::Next, reply: None }).await.unwrap();
    tx.send(EngineMsg::Command { command: Command::PlayPause, reply: None }).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while calls.next.load(Ordering::SeqCst) == 0 || calls.play_pause.load(Ordering::SeqCst) == 0 {
        assert!(tokio::time::Instant::now() < deadline, "transport calls never arrived");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(calls.previous.load(Ordering::SeqCst), 0, "only the sent commands should fire");

    // A property change should reach the engine without any polling.
    let iface = conn.object_server().interface::<_, MockPlayer>(MOCK_PATH).await.unwrap();
    {
        let mut guard = iface.get_mut().await;
        guard.title = "Aerodynamic".into();
        guard.track_id = "spotify:track:2VEZx7NWsZ1D0eJ4uv5Fym".into();
        guard.status = "Paused".into();
        // A real player rewinds when the track changes. Leaving this at the old
        // value would be testing a situation that cannot happen, and the engine
        // would rightly believe the player over its own guess.
        guard.position_us = 0;
        guard.metadata_changed(iface.signal_emitter()).await.unwrap();
        guard.playback_status_changed(iface.signal_emitter()).await.unwrap();
    }

    // Metadata and playback status arrive as two separate signals, so waiting on
    // only one of them races the other.
    wait_for(&mut states, |s| {
        s.track().map(|t| t.title.as_str()) == Some("Aerodynamic") && s.status() == Status::Paused
    })
    .await;

    {
        let s = states.borrow_and_update();
        assert_eq!(s.status(), Status::Paused);
        // The engine re-reads position on every property change, so a track
        // change lands on the new track's position rather than carrying the old
        // one over until something else happens to correct it.
        assert!(
            s.player.as_ref().unwrap().position_ms < 5_000,
            "position should follow the new track, was {}",
            s.player.as_ref().unwrap().position_ms
        );
    }

    // Selecting a new song while paused starts playback, and the track change and
    // the new playback state are announced as two separate signals. If the status
    // one is missed, or arrives wrapped in a shape the parser does not recognise,
    // the bar sits on "paused" while music plays.
    //
    // This emits only the metadata change and withholds the status signal
    // entirely, so the engine can only get this right by asking the player what
    // is actually true rather than trusting what the signal carried.
    {
        let mut guard = iface.get_mut().await;
        guard.title = "Veridis Quo".into();
        guard.track_id = "spotify:track:2LD2gT7gwAurzdQDRAJgTs".into();
        guard.status = "Playing".into();
        guard.position_us = 0;
        guard.metadata_changed(iface.signal_emitter()).await.unwrap();
        // No playback_status_changed. That is the whole point of the test.
    }

    wait_for(&mut states, |s| s.track().map(|t| t.title.as_str()) == Some("Veridis Quo")).await;

    {
        let s = states.borrow_and_update();
        assert_eq!(
            s.status(),
            Status::Playing,
            "playback state must be read from the player, not inferred from which \
             properties a signal happened to carry"
        );
    }

    engine_task.abort();
    let _ = conn.release_name(MOCK_NAME).await;
}

#[tokio::test]
async fn engine_reports_no_player_when_none_is_running() {
    let Ok(_conn) = zbus::Connection::session().await else {
        eprintln!("skipping: no session bus available");
        return;
    };

    // Preferring a name nothing will ever claim is the only reliable way to test
    // the empty case on a developer machine that may have music playing.
    let config = Config {
        player: PlayerConfig { preferred: vec!["definitely-not-a-real-player".into()] },
        ..Default::default()
    };
    let engine = Engine::new(config).await.expect("engine should start with no player");
    let states = engine.subscribe();
    let state = states.borrow().clone();

    // Whatever is or is not playing, caps must never claim an ability the daemon
    // cannot deliver.
    assert!(!state.caps.can_like, "liking needs an authorized account");
    assert!(!state.caps.can_transfer, "transfer needs Premium");
    assert!(!state.caps.can_set_volume, "volume routing is not wired up yet");
}
