//! End to end tests against a mock MPRIS player served on the real session bus.
//!
//! The unit tests cover parsing and arithmetic. What they cannot cover is whether
//! the engine attaches to a player, reacts to a `PropertiesChanged`, and sends
//! transport calls to the right place. That needs a bus and something listening
//! on it.
//!
//! The player is [`waytify_core::mock`], the same one `waytify mock-player` runs,
//! so what is tested here is what a contributor can reproduce by hand. It is
//! deliberately awkward in the ways real players are: string track ids rather than
//! object paths, and nothing emitted that the spec does not require.
//!
//! Skipped, not failed, when there is no session bus. That keeps the suite honest
//! on a build machine without one instead of pretending to have verified this.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;
use waytify_core::clock::Attention;
use waytify_core::config::{Config, PlayerConfig};
use waytify_core::engine::{Engine, EngineMsg};
use waytify_core::mock;
use waytify_ipc::{Command, State, Status};

/// Wait for a predicate to hold, polling the state channel.
///
/// D-Bus round trips have no fixed duration, so the alternative would be a sleep
/// long enough to be slow and short enough to be flaky.
async fn wait_for<F>(states: &mut tokio::sync::watch::Receiver<Arc<State>>, what: &str, mut ok: F)
where
    F: FnMut(&State) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if ok(&states.borrow_and_update()) {
            return;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for {what}");
        let _ = tokio::time::timeout(remaining, states.changed()).await;
    }
}

fn following_the_mock() -> Config {
    Config { player: PlayerConfig { preferred: vec![mock::SUFFIX.into()] }, ..Default::default() }
}

#[tokio::test]
async fn engine_follows_a_live_player() {
    let Ok(conn) = zbus::Connection::session().await else {
        eprintln!("skipping: no session bus available");
        return;
    };

    let calls = Arc::new(mock::Calls::default());
    let player = mock::Player::new(mock::sample_playlist(), Arc::clone(&calls));
    mock::serve(&conn, player, "Waytify Mock").await.unwrap();
    if conn.request_name(mock::BUS_NAME).await.is_err() {
        eprintln!("skipping: could not claim {}", mock::BUS_NAME);
        return;
    }

    let engine = Engine::new(following_the_mock()).await.expect("engine should start");
    let mut states = engine.subscribe();
    let (tx, rx) = mpsc::channel(16);
    let engine_task = tokio::spawn(engine.run(rx));

    // The engine scans on startup, so the mock should already be attached.
    wait_for(&mut states, "the first track", |s| {
        s.track().map(|t| t.title.as_str()) == Some("Digital Love")
    })
    .await;

    {
        let s = states.borrow_and_update();
        let p = s.player.as_ref().expect("a player should be attached");
        assert_eq!(p.identity, "Waytify Mock", "identity should come from the root interface");
        assert_eq!(p.status, Status::Playing);
        assert_eq!(s.track().unwrap().artists, vec!["Daft Punk"]);
        assert_eq!(s.track().unwrap().length_ms, Some(301_000));
        assert!(s.caps.can_seek, "a track with a known length is seekable");
    }

    // Transport reaches the player.
    tx.send(EngineMsg::Command { command: Command::Next, reply: None }).await.unwrap();
    wait_for(&mut states, "the next track", |s| {
        s.track().map(|t| t.title.as_str()) == Some("Untitled Demo")
    })
    .await;
    assert_eq!(calls.next.load(Ordering::SeqCst), 1);
    assert_eq!(calls.previous.load(Ordering::SeqCst), 0, "only the sent command should fire");

    // A track with no album must not invent one.
    assert_eq!(states.borrow_and_update().track().unwrap().album, None);

    // Absolute seeks fall back to a relative Seek, because the mock reports its
    // track id as a plain string and SetPosition needs an object path.
    tx.send(EngineMsg::Command { command: Command::Seek { position_ms: 60_000 }, reply: None })
        .await
        .unwrap();
    wait_for(&mut states, "the seek to land", |s| {
        s.player.as_ref().is_some_and(|p| p.position_ms >= 55_000)
    })
    .await;
    assert!(calls.seek.load(Ordering::SeqCst) >= 1, "a seek should have reached the player");

    let iface = conn.object_server().interface::<_, mock::Player>(mock::OBJECT_PATH).await.unwrap();

    // Selecting a new song while paused starts playback, and the track change and
    // the new playback state are announced as two separate signals. If the status
    // one is missed, or arrives in a shape the parser does not recognise, the bar
    // sits on "paused" while music plays.
    //
    // This withholds the status signal entirely, so the engine can only get it
    // right by asking the player rather than trusting what the signal carried.
    {
        let mut guard = iface.get_mut().await;
        guard.status = "Paused".into();
        guard.playback_status_changed(iface.signal_emitter()).await.unwrap();
    }
    wait_for(&mut states, "the pause", |s| s.status() == Status::Paused).await;

    {
        let mut guard = iface.get_mut().await;
        guard.index = 2;
        guard.position_us = 0;
        guard.status = "Playing".into();
        guard.metadata_changed(iface.signal_emitter()).await.unwrap();
        // No playback_status_changed. That is the whole point of the test.
    }
    wait_for(&mut states, "the third track", |s| {
        s.track().map(|t| t.title.as_str()) == Some("A Very Long One")
    })
    .await;
    assert_eq!(
        states.borrow_and_update().status(),
        Status::Playing,
        "playback state must be read from the player, not inferred from which \
         properties a signal happened to carry"
    );

    // Elapsed time has to keep moving between events.
    //
    // The daemon publishes on change, and a position advancing smoothly is not a
    // change, so without a heartbeat the time in the bar freezes until something
    // else happens. The bar cannot cover for that by interpolating on its own,
    // because it is sent text the daemon already rendered.
    //
    // The mock's own position stays where it is: this tests that the interpolated
    // clock reaches clients, not that the player was polled harder.
    tx.send(EngineMsg::Attention(Attention::Bar)).await.unwrap();
    let started_at = { states.borrow_and_update().player.as_ref().unwrap().position_ms };
    let advanced = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if states.changed().await.is_err() {
                return false;
            }
            let now = { states.borrow_and_update().player.as_ref().unwrap().position_ms };
            if now >= started_at + 900 {
                return true;
            }
        }
    })
    .await;
    assert_eq!(
        advanced,
        Ok(true),
        "position never advanced while playing, so elapsed time would sit frozen in the bar"
    );

    engine_task.abort();
    let _ = conn.release_name(mock::BUS_NAME).await;
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
    let state = engine.subscribe().borrow().clone();

    // Whatever is or is not playing, caps must never claim an ability the daemon
    // cannot deliver.
    assert!(!state.caps.can_like, "liking needs an authorized account");
    assert!(!state.caps.can_transfer, "transfer needs Premium");
}
