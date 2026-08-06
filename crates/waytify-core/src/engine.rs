//! The live engine: watches D-Bus, keeps one canonical [`State`], applies commands.
//!
//! Signals from the attached player are forwarded into a single channel by a task
//! that is cancelled and respawned whenever the player changes. Doing it that way
//! keeps the main loop selecting over a fixed set of sources instead of juggling
//! streams whose lifetime depends on which player happens to be running.

use crate::clock::{Attention, PositionClock};
use crate::config::Config;
use crate::metadata::{self, Metadata};
use crate::mpris::{self, MediaPlayer2Proxy, PlayerProxy};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{mpsc, watch};
use waytify_ipc::{Command, Player, State, Status};
use zbus::Connection;
use zbus::zvariant::{ObjectPath, OwnedValue};

/// Anything the daemon can tell the engine.
#[derive(Debug)]
pub enum EngineMsg {
    Command {
        command: Command,
        /// Where to report the outcome. A command whose result nobody is waiting
        /// for is logged instead, but anything a user typed or bound to a key
        /// gets an answer, so a failure is visible rather than silent.
        reply: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    },
    /// Who is watching, which decides how often position drift is corrected.
    Attention(Attention),
}

/// A signal from the player we are currently attached to.
#[derive(Debug)]
enum PlayerEvent {
    Properties(HashMap<String, OwnedValue>),
    /// Position in microseconds. Not every player sends this.
    Seeked(i64),
}

/// How far the interpolated position may disagree with the player before it is
/// worth republishing. Anything under this is round trip latency, not a seek.
const DRIFT_TOLERANCE_MS: u64 = 250;

pub struct Engine {
    conn: Connection,
    config: Config,
    state: State,
    clock: PositionClock,
    attention: Attention,

    attached: Option<Attached>,
    events_tx: mpsc::Sender<PlayerEvent>,
    events_rx: mpsc::Receiver<PlayerEvent>,

    updates: watch::Sender<Arc<State>>,
}

struct Attached {
    bus_name: String,
    player: PlayerProxy<'static>,
    watcher: tokio::task::JoinHandle<()>,
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.watcher.abort();
    }
}

impl Engine {
    pub async fn new(config: Config) -> Result<Self> {
        let conn = Connection::session().await.context("connecting to the session bus")?;
        let (events_tx, events_rx) = mpsc::channel(64);
        let (updates, _) = watch::channel(Arc::new(State::default()));

        let mut engine = Self {
            conn,
            config,
            state: State::default(),
            clock: PositionClock::new(Instant::now()),
            attention: Attention::Idle,
            attached: None,
            events_tx,
            events_rx,
            updates,
        };
        engine.rescan().await?;
        Ok(engine)
    }

    /// Latest state, updated in place. New subscribers see the current value
    /// immediately rather than waiting for the next change.
    pub fn subscribe(&self) -> watch::Receiver<Arc<State>> {
        self.updates.subscribe()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Run until the message channel closes.
    pub async fn run(mut self, mut msgs: mpsc::Receiver<EngineMsg>) -> Result<()> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let mut names = dbus.receive_name_owner_changed().await?;

        loop {
            // Recreated each iteration on purpose. Any event restarts the timer,
            // which gives the semantics we want: correct drift only when nothing
            // has been heard for a while.
            let drift = async {
                match self.attention.drift_interval(self.clock.is_playing()) {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                msg = msgs.recv() => match msg {
                    Some(m) => self.handle_msg(m).await,
                    None => break,
                },
                Some(sig) = names.next() => {
                    if let Ok(args) = sig.args()
                        && mpris::is_player_name(args.name())
                        && let Err(e) = self.rescan().await
                    {
                        tracing::warn!("rescan after a bus name change failed: {e:#}");
                    }
                }
                Some(ev) = self.events_rx.recv() => self.handle_player_event(ev).await,
                _ = drift => self.correct_drift().await,
            }
        }
        Ok(())
    }

    async fn handle_msg(&mut self, msg: EngineMsg) {
        match msg {
            EngineMsg::Attention(a) => self.attention = a,
            EngineMsg::Command { command, reply } => {
                let result = self.apply(command).await.map_err(|e| format!("{e:#}"));
                match reply {
                    Some(tx) => {
                        let _ = tx.send(result);
                    }
                    None => {
                        if let Err(e) = result {
                            tracing::warn!("command failed: {e}");
                        }
                    }
                }
            }
        }
    }

    /// Find every player on the bus and attach to the best one.
    async fn rescan(&mut self) -> Result<()> {
        let dbus = zbus::fdo::DBusProxy::new(&self.conn).await?;
        let names = dbus.list_names().await?;

        let mut candidates = Vec::new();
        for name in names {
            let name = name.as_str();
            if !mpris::is_player_name(name) {
                continue;
            }
            // A player that cannot answer is one that is shutting down. Skip it
            // rather than failing the whole scan.
            let Ok(proxy) = self.player_proxy(name).await else { continue };
            let status =
                proxy.playback_status().await.map(|s| mpris::parse_status(&s)).unwrap_or_default();
            candidates.push(mpris::Candidate { bus_name: name.to_string(), status });
        }

        let chosen =
            mpris::select(&candidates, &self.config.player.preferred).map(|c| c.bus_name.clone());

        match chosen {
            Some(bus) => {
                if self.attached.as_ref().map(|a| a.bus_name.as_str()) != Some(bus.as_str()) {
                    self.attach(&bus).await?;
                } else {
                    self.refresh().await?;
                }
            }
            None => self.detach(),
        }
        Ok(())
    }

    async fn player_proxy(&self, bus_name: &str) -> Result<PlayerProxy<'static>> {
        Ok(PlayerProxy::builder(&self.conn).destination(bus_name.to_string())?.build().await?)
    }

    async fn attach(&mut self, bus_name: &str) -> Result<()> {
        let player = self.player_proxy(bus_name).await?;

        // A player that does not answer for its own name still plays music, so
        // fall back to the bus name rather than refusing to attach.
        let identity =
            match MediaPlayer2Proxy::builder(&self.conn).destination(bus_name.to_string()) {
                Ok(b) => match b.build().await {
                    Ok(p) => p.identity().await.ok(),
                    Err(_) => None,
                },
                Err(_) => None,
            }
            .unwrap_or_else(|| mpris::short_name(bus_name).to_string());

        let watcher = tokio::spawn(watch_player(
            self.conn.clone(),
            bus_name.to_string(),
            self.events_tx.clone(),
        ));

        self.attached = Some(Attached { bus_name: bus_name.to_string(), player, watcher });
        self.state.player = Some(Player {
            bus_name: bus_name.to_string(),
            identity,
            status: Status::Stopped,
            track: None,
            position_ms: 0,
            shuffle: None,
            repeat: None,
        });

        tracing::info!("following {bus_name}");
        self.refresh().await
    }

    fn detach(&mut self) {
        if self.attached.take().is_some() {
            tracing::info!("no players left");
        }
        self.state.player = None;
        self.clock.set_playing(false, Instant::now());
        self.publish();
    }

    /// Read everything from the attached player. Used on attach and after any
    /// event that could have invalidated more than it reported.
    async fn refresh(&mut self) -> Result<()> {
        let Some(a) = &self.attached else { return Ok(()) };
        let player = a.player.clone();

        let status =
            player.playback_status().await.map(|s| mpris::parse_status(&s)).unwrap_or_default();
        let track = player.metadata().await.ok().and_then(|m| metadata::track_from_metadata(&m));
        let position_ms = player.position().await.map(us_to_ms).unwrap_or(0);
        // Spotify reports these unreliably, so a failure means "unknown" rather
        // than a value. The popup hides a control it cannot trust.
        let shuffle = player.shuffle().await.ok();
        let repeat = player.loop_status().await.ok().map(|s| mpris::parse_repeat(&s));

        let now = Instant::now();
        self.clock.set_length(track.as_ref().and_then(|t| t.length_ms));
        self.clock.anchor(position_ms, status.is_playing(), now);

        if let Some(p) = &mut self.state.player {
            p.status = status;
            p.track = track;
            p.position_ms = position_ms;
            p.shuffle = shuffle;
            p.repeat = repeat;
        }
        self.recompute_caps();
        self.publish();
        Ok(())
    }

    async fn handle_player_event(&mut self, ev: PlayerEvent) {
        match ev {
            PlayerEvent::Seeked(us) => {
                self.clock.anchor(us_to_ms(us), self.clock.is_playing(), Instant::now());
                self.publish();
            }
            PlayerEvent::Properties(changed) => self.apply_properties(changed).await,
        }
    }

    async fn apply_properties(&mut self, changed: HashMap<String, OwnedValue>) {
        let now = Instant::now();
        let mut dirty = false;

        if let Some(v) = changed.get("PlaybackStatus")
            && let Some(s) = as_str(v)
        {
            let status = mpris::parse_status(s);
            if let Some(p) = &mut self.state.player {
                p.status = status;
            }
            self.clock.set_playing(status.is_playing(), now);
            dirty = true;
        }

        if let Some(v) = changed.get("Metadata") {
            let md: Result<Metadata, _> = v.clone().try_into();
            if let Ok(md) = md {
                let track = metadata::track_from_metadata(&md);
                let changed_track =
                    self.state.track().map(|t| &t.id) != track.as_ref().map(|t| &t.id);

                self.clock.set_length(track.as_ref().and_then(|t| t.length_ms));
                if let Some(p) = &mut self.state.player {
                    p.track = track;
                }
                // A new track starts at zero. Waiting for the player to say so
                // leaves the old position on screen for a visible moment.
                if changed_track {
                    self.clock.anchor(0, self.clock.is_playing(), now);
                }
                dirty = true;
            }
        }

        if let Some(v) = changed.get("Shuffle")
            && let Some(b) = as_bool(v)
            && let Some(p) = &mut self.state.player
        {
            p.shuffle = Some(b);
            dirty = true;
        }

        if let Some(v) = changed.get("LoopStatus")
            && let Some(s) = as_str(v)
            && let Some(p) = &mut self.state.player
        {
            p.repeat = Some(mpris::parse_repeat(s));
            dirty = true;
        }

        if dirty {
            // Every property change is also a chance to re-anchor the position,
            // which is how this stays correct on players that never emit Seeked.
            if let Some(a) = &self.attached
                && let Ok(us) = a.player.position().await
            {
                let observed = us_to_ms(us);
                if self.clock.drifted(observed, now, DRIFT_TOLERANCE_MS) {
                    self.clock.anchor(observed, self.clock.is_playing(), now);
                }
            }
            self.recompute_caps();
            self.publish();
        }
    }

    /// Re-read the real position and republish only if it moved noticeably.
    async fn correct_drift(&mut self) {
        let Some(a) = &self.attached else { return };
        let Ok(us) = a.player.position().await else { return };

        let now = Instant::now();
        let observed = us_to_ms(us);
        if self.clock.drifted(observed, now, DRIFT_TOLERANCE_MS) {
            self.clock.anchor(observed, self.clock.is_playing(), now);
            self.publish();
        }
    }

    fn recompute_caps(&mut self) {
        let has_track = self.state.track().is_some();
        let has_length = self.state.track().and_then(|t| t.length_ms).is_some();
        self.state.caps = waytify_ipc::Caps {
            // Deliberately not gated on the player's CanSeek flag, which several
            // players report incorrectly. A track with a known length is seekable
            // until proven otherwise.
            can_seek: has_track && has_length,
            can_like: self.state.spotify.authorized && has_track,
            can_transfer: self.state.spotify.can_control_remote(),
            can_set_volume: self.state.audio.route != waytify_ipc::VolumeRoute::Unavailable,
            show_free_account_notice: self.state.spotify.authorized
                && self.state.spotify.premium == Some(false),
        };
    }

    fn publish(&mut self) {
        if let Some(p) = &mut self.state.player {
            p.position_ms = self.clock.position();
        }
        self.updates.send_replace(Arc::new(self.state.clone()));
    }

    async fn apply(&mut self, cmd: Command) -> Result<()> {
        let Some(a) = &self.attached else {
            anyhow::bail!("no player is running");
        };
        let player = a.player.clone();

        match cmd {
            Command::PlayPause => player.play_pause().await?,
            Command::Play => player.play().await?,
            Command::Pause => player.pause().await?,
            Command::Next => player.next().await?,
            Command::Previous => player.previous().await?,

            Command::Seek { position_ms } => self.seek_absolute(position_ms).await?,
            Command::SeekBy { delta_ms } => player.seek(delta_ms * 1_000).await?,

            Command::ToggleShuffle => {
                let current = self.state.player.as_ref().and_then(|p| p.shuffle).unwrap_or(false);
                player.set_shuffle(!current).await?;
            }
            Command::SetShuffle { on } => player.set_shuffle(on).await?,
            Command::CycleRepeat => {
                let current = self.state.player.as_ref().and_then(|p| p.repeat).unwrap_or_default();
                player.set_loop_status(mpris::repeat_to_mpris(current.next())).await?;
            }
            Command::SetRepeat { mode } => {
                player.set_loop_status(mpris::repeat_to_mpris(mode)).await?;
            }

            Command::RaisePlayer => {
                let bus = a.bus_name.clone();
                let root = MediaPlayer2Proxy::builder(&self.conn).destination(bus)?.build().await?;
                root.raise().await?;
            }

            // Landing in later phases. Rejected explicitly so a client gets an
            // error it can show rather than silence it has to guess about.
            Command::SetVolume { .. }
            | Command::VolumeBy { .. }
            | Command::ToggleMute
            | Command::SetSink { .. } => anyhow::bail!("volume control arrives in the next phase"),
            Command::ToggleLike | Command::TransferTo { .. } => {
                anyhow::bail!("this needs a connected Spotify account")
            }
            Command::TogglePopup { .. } | Command::ShowPopup { .. } | Command::HidePopup => {
                anyhow::bail!("the popup arrives in the next phase")
            }

            Command::Subscribe { .. } | Command::Shutdown => {}
        }
        Ok(())
    }

    /// Absolute seek, with a fallback for players whose track ids are not object paths.
    async fn seek_absolute(&mut self, position_ms: u64) -> Result<()> {
        let Some(a) = &self.attached else { anyhow::bail!("no player is running") };
        let player = a.player.clone();
        let track_id = self.state.track().and_then(|t| t.id.clone());

        let sought = match track_id.as_deref().and_then(|id| ObjectPath::try_from(id).ok()) {
            Some(path) => player.set_position(&path, (position_ms * 1_000) as i64).await.is_ok(),
            None => false,
        };

        if !sought {
            // The spec requires an object path here and some players send a plain
            // string, which makes SetPosition unusable. A relative seek from where
            // we believe we are gets to the same place.
            let current = self.clock.position() as i64;
            let delta_ms = position_ms as i64 - current;
            player.seek(delta_ms * 1_000).await?;
        }

        self.clock.release(position_ms, Instant::now());
        self.publish();
        Ok(())
    }
}

/// Forward one player's signals into the engine's channel.
///
/// Lives for exactly as long as that player is the attached one. The engine
/// aborts it on detach, which is why it can loop forever without a shutdown path.
async fn watch_player(conn: Connection, bus_name: String, tx: mpsc::Sender<PlayerEvent>) {
    let props = match zbus::fdo::PropertiesProxy::builder(&conn)
        .destination(bus_name.clone())
        .and_then(|b| b.path("/org/mpris/MediaPlayer2"))
    {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => return tracing::warn!("properties proxy for {bus_name}: {e}"),
        },
        Err(e) => return tracing::warn!("properties proxy for {bus_name}: {e}"),
    };

    let mut prop_changes = match props.receive_properties_changed().await {
        Ok(s) => s,
        Err(e) => return tracing::warn!("property signals for {bus_name}: {e}"),
    };

    let player = match PlayerProxy::builder(&conn).destination(bus_name.clone()) {
        Ok(b) => match b.build().await {
            Ok(p) => p,
            Err(e) => return tracing::warn!("player proxy for {bus_name}: {e}"),
        },
        Err(e) => return tracing::warn!("player proxy for {bus_name}: {e}"),
    };
    let mut seeks = match player.receive_seeked().await {
        Ok(s) => s,
        Err(e) => return tracing::warn!("seek signals for {bus_name}: {e}"),
    };

    loop {
        tokio::select! {
            Some(sig) = prop_changes.next() => {
                let Ok(args) = sig.args() else { continue };
                if args.interface_name != "org.mpris.MediaPlayer2.Player" {
                    continue;
                }
                let owned: HashMap<String, OwnedValue> = args
                    .changed_properties
                    .iter()
                    .filter_map(|(k, v)| {
                        OwnedValue::try_from(v.clone()).ok().map(|v| (k.to_string(), v))
                    })
                    .collect();
                if tx.send(PlayerEvent::Properties(owned)).await.is_err() {
                    return;
                }
            }
            Some(sig) = seeks.next() => {
                let Ok(args) = sig.args() else { continue };
                if tx.send(PlayerEvent::Seeked(args.position)).await.is_err() {
                    return;
                }
            }
            else => return,
        }
    }
}

/// MPRIS speaks microseconds. Negative values are nonsense and read as zero.
fn us_to_ms(us: i64) -> u64 {
    u64::try_from(us).unwrap_or(0) / 1_000
}

// Matching the variant directly rather than going through TryFrom, for the same
// reason as in `metadata`: a property arriving with an unexpected type should
// read as absent instead of failing anything.
fn as_str(v: &OwnedValue) -> Option<&str> {
    match &**v {
        zbus::zvariant::Value::Str(s) => Some(s.as_str()),
        _ => None,
    }
}

fn as_bool(v: &OwnedValue) -> Option<bool> {
    match &**v {
        zbus::zvariant::Value::Bool(b) => Some(*b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn microseconds_become_milliseconds() {
        assert_eq!(us_to_ms(215_000_000), 215_000);
        assert_eq!(us_to_ms(0), 0);
    }

    #[test]
    fn a_negative_position_reads_as_zero() {
        // Seen from players that briefly report -1 while loading a track.
        assert_eq!(us_to_ms(-1), 0);
    }
}
