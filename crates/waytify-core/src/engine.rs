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
use std::time::{Duration, Instant};
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

/// Something that happened outside the main loop and needs folding into state.
#[derive(Debug)]
enum PlayerEvent {
    Properties(HashMap<String, OwnedValue>),
    /// Position in microseconds. Not every player sends this.
    Seeked(i64),
    /// Album art finished downloading. Carries the cache key it was fetched for,
    /// because the track can change while a download is in flight.
    Artwork {
        key: String,
        path: std::path::PathBuf,
        colors: Option<waytify_ipc::ArtColors>,
    },
    /// Whether a track is in the library. Carries the track it is about, since
    /// the answer arrives after a round trip and the song may have moved on.
    Liked {
        track_id: String,
        liked: bool,
    },
    /// Spotify refused a library call, so likes are not usable with this token.
    LibraryUnavailable,
    /// What is coming up next, as far as Spotify will say.
    Queue(Vec<waytify_ipc::Track>),
    /// The playlist or album the current track came out of.
    Context(Option<waytify_ipc::PlayContext>),
    /// The user's own playlists.
    Playlists(Vec<waytify_ipc::Playlist>),
    /// What a search turned up.
    Search(Vec<waytify_ipc::SearchResult>),
    /// What was played recently.
    Recent(Vec<waytify_ipc::Track>),
    /// Everything in the playlist or album being played.
    ContextTracks(Vec<waytify_ipc::Track>),
    /// Lyrics finished downloading, or turned out not to exist. Carries the key
    /// they were fetched for, since the track can change while a request is out.
    Lyrics {
        key: String,
        lyrics: Option<waytify_ipc::Lyrics>,
    },
    /// The Spotify Connect device list, and whether the account can control it.
    Devices {
        devices: Vec<waytify_ipc::Device>,
        premium: Option<bool>,
    },
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

    /// `None` when there is no sound server, which is a normal situation rather
    /// than an error. Everything else still works; volume simply is not offered.
    audio: Option<crate::audio::Audio>,
    audio_rx: Option<tokio::sync::mpsc::UnboundedReceiver<crate::audio::AudioSnapshot>>,

    /// `None` until a client id is configured. Shared rather than owned because
    /// requests happen in spawned tasks: a network round trip must not block the
    /// loop that is also feeding the bar.
    spotify: Option<Arc<tokio::sync::Mutex<crate::spotify::Client>>>,

    updates: watch::Sender<Arc<State>>,
}

struct Attached {
    bus_name: String,
    player: PlayerProxy<'static>,
    /// The process holding the bus name, used to find its audio stream. Asked for
    /// once on attach rather than per request, since it cannot change.
    pid: Option<u32>,
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

        // A desktop with no sound server is unusual but not broken, and refusing
        // to start over it would take the bar down with it.
        let (audio, audio_rx) = match crate::audio::Audio::connect() {
            Ok((audio, rx)) => (Some(audio), Some(rx)),
            Err(e) => {
                tracing::info!("volume control unavailable: {e:#}");
                (None, None)
            }
        };

        // Empty client id means the Spotify layer is simply off, which is a
        // supported configuration rather than a missing one.
        let spotify = if config.spotify.client_id.trim().is_empty() {
            None
        } else {
            match crate::spotify::Client::new(config.spotify.client_id.clone()) {
                Ok(client) => Some(Arc::new(tokio::sync::Mutex::new(client))),
                Err(e) => {
                    tracing::warn!("could not start the Spotify client: {e:#}");
                    None
                }
            }
        };

        let mut engine = Self {
            conn,
            config,
            state: State::default(),
            clock: PositionClock::new(Instant::now()),
            attention: Attention::Idle,
            attached: None,
            events_tx,
            events_rx,
            audio,
            audio_rx,
            spotify,
            updates,
        };
        engine.restore_spotify().await;
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
            // which gives the semantics we want: reconcile only when nothing has
            // been heard for a while.
            let tick = async {
                match self.attention.poll_interval(self.clock.is_playing()) {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            };

            // Separate from the reconcile tick above, and much cheaper: no D-Bus
            // call, just a clone and a channel send.
            //
            // The position advances continuously but the daemon only publishes on
            // change, and smooth interpolation is not a change. Clients would see
            // the elapsed time freeze between events. The bar cannot paper over
            // that by interpolating itself, because it receives text the daemon
            // has already rendered rather than a position to advance.
            //
            // Only while playing, and only while someone is connected, so an idle
            // machine still produces nothing at all.
            let republish = async {
                if self.attention != Attention::Idle && self.clock.is_playing() {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                } else {
                    std::future::pending().await
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
                Some(snapshot) = async {
                    match self.audio_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => self.apply_audio(snapshot),
                _ = republish => self.publish(),
                _ = tick => {
                    let now = Instant::now();
                    // Nothing announced anything for a while, so both reads are
                    // gap fills rather than second guesses.
                    if self.attention == Attention::Popup {
                        self.request_devices();
                    }
                    let moved = self.refresh_status(now).await | self.refresh_position(now).await;
                    if moved {
                        self.recompute_caps();
                        self.publish();
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_msg(&mut self, msg: EngineMsg) {
        match msg {
            EngineMsg::Attention(a) => {
                let opened = a == Attention::Popup && self.attention != Attention::Popup;
                self.attention = a;
                // Only while the window is open. There is no push channel for
                // the device list, so polling it unseen would spend rate limit
                // on an answer nobody would look at.
                if opened {
                    self.request_devices();
                    self.request_queue();
                    self.request_context();
                    self.request_lyrics();
                }
            }
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
            mpris::select(&candidates, &self.config.player.preferred, &self.config.player.only)
                .map(|c| c.bus_name.clone());

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

        // Best effort: a player that will not say which process it is still works,
        // it just has to be matched by name alone.
        let pid = match zbus::fdo::DBusProxy::new(&self.conn).await {
            Ok(dbus) => match zbus::names::BusName::try_from(bus_name.to_string()) {
                Ok(name) => dbus.get_connection_unix_process_id(name).await.ok(),
                Err(_) => None,
            },
            Err(_) => None,
        };

        self.attached = Some(Attached { bus_name: bus_name.to_string(), player, pid, watcher });
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
        self.refresh_audio();
        self.refresh().await
    }

    fn detach(&mut self) {
        if self.attached.take().is_some() {
            tracing::info!("no players left");
        }
        self.state.player = None;
        self.state.spotify.queue.clear();
        self.state.spotify.context = None;
        self.state.spotify.context_tracks.clear();
        self.state.lyrics = None;
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

        let was = self.state.track().and_then(crate::lyrics::key_for);
        if let Some(p) = &mut self.state.player {
            p.status = status;
            p.track = track;
            p.position_ms = position_ms;
            p.shuffle = shuffle;
            p.repeat = repeat;
        }
        self.forget_stale_lyrics(was);
        self.recompute_caps();
        self.request_artwork();
        self.request_liked();
        self.request_queue();
        self.request_context();
        self.request_lyrics();
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
            PlayerEvent::Liked { track_id, liked } => {
                // The song may have changed while the answer was in flight.
                let current = self.state.track().and_then(crate::metadata::spotify_uri);
                if current.as_deref() != Some(track_id.as_str()) {
                    return;
                }
                if let Some(track) = self.state.player.as_mut().and_then(|p| p.track.as_mut()) {
                    track.liked = Some(liked);
                }
                self.recompute_caps();
                self.publish();
            }
            PlayerEvent::Lyrics { key, lyrics } => {
                // The track can change while the request is out, and lyrics for
                // the previous song scrolling against this one is worse than
                // none at all.
                if self.state.track().and_then(crate::lyrics::key_for) != Some(key) {
                    return;
                }
                if self.state.lyrics != lyrics {
                    self.state.lyrics = lyrics;
                    self.publish();
                }
            }
            PlayerEvent::Context(context) => {
                if !current_is_spotify(&self.state) {
                    return;
                }
                if self.state.spotify.context != context {
                    // What was in the last one does not belong to this one.
                    self.state.spotify.context_tracks.clear();
                    self.state.spotify.context = context;
                    self.publish();
                }
            }
            PlayerEvent::ContextTracks(tracks) => {
                if self.state.spotify.context_tracks != tracks {
                    self.state.spotify.context_tracks = tracks;
                    self.publish();
                }
            }
            PlayerEvent::Recent(recent) => {
                if self.state.spotify.recent != recent {
                    self.state.spotify.recent = recent;
                    self.publish();
                }
            }
            PlayerEvent::Search(results) => {
                if self.state.spotify.search != results {
                    self.state.spotify.search = results;
                    self.publish();
                }
            }
            PlayerEvent::Playlists(playlists) => {
                if self.state.spotify.playlists != playlists {
                    self.state.spotify.playlists = playlists;
                    self.publish();
                }
            }
            PlayerEvent::Queue(queue) => {
                // The round trip outlives the track it was asked about, so the
                // player may have moved on to something that is not Spotify's.
                if !current_is_spotify(&self.state) {
                    return;
                }
                if self.state.spotify.queue != queue {
                    self.state.spotify.queue = queue;
                    self.publish();
                }
            }
            PlayerEvent::LibraryUnavailable => {
                if self.state.spotify.library_available {
                    tracing::warn!(
                        "Spotify refused access to the library, so the like button is \
                         hidden. If the application is in development mode, add this \
                         account under User Management in the dashboard."
                    );
                }
                self.state.spotify.library_available = false;
                self.recompute_caps();
                self.publish();
            }
            PlayerEvent::Devices { devices, premium } => {
                self.state.spotify.devices = devices;
                if let Some(premium) = premium {
                    self.state.spotify.premium = Some(premium);
                }
                self.state.spotify.active_device =
                    self.state.spotify.devices.iter().find(|d| d.is_active).map(|d| d.id.clone());
                self.recompute_caps();
                self.publish();
            }
            PlayerEvent::Artwork { key, path, colors } => {
                // A skip during the download means this art belongs to a track
                // that is no longer showing. Dropping it is correct; the current
                // track has its own fetch already running.
                let current = self.state.track().and_then(crate::art::key_for);
                if current.as_deref() != Some(key.as_str()) {
                    return;
                }
                if let Some(track) = self.state.player.as_mut().and_then(|p| p.track.as_mut()) {
                    track.art_path = Some(path);
                    track.colors = colors;
                }
                self.publish();
            }
        }
    }

    /// Adopt a stored refresh token, if there is one.
    ///
    /// A failure here means the account is simply not connected. It is logged
    /// once and never retried in a loop, because a revoked token would otherwise
    /// generate a request every time anything happened.
    async fn restore_spotify(&mut self) {
        let Some(client) = &self.spotify else { return };
        // The keyring talks to the secret service over blocking D-Bus, so it goes
        // on a blocking thread rather than stalling the executor that is also
        // driving the bar.
        let stored = tokio::task::spawn_blocking(crate::spotify::auth::load_refresh_token).await;
        let token = match stored {
            Ok(Ok(Some(token))) => token,
            Ok(Ok(None)) => return,
            Ok(Err(e)) => return tracing::warn!("could not read the stored Spotify token: {e:#}"),
            Err(e) => return tracing::warn!("reading the keyring panicked: {e}"),
        };

        let mut guard = client.lock().await;
        match guard.restore(&token).await {
            Ok(()) => {
                self.state.spotify.authorized = true;
                let premium = guard.check_premium().await.ok().flatten();
                self.state.spotify.premium = premium;
                tracing::info!("Spotify connected (premium: {premium:?})");
            }
            Err(e) => tracing::warn!("stored Spotify token is not usable: {e:#}"),
        }
    }

    /// Ask whether the current track is in the library.
    fn request_liked(&self) {
        if !self.state.spotify.library_available {
            return;
        }
        let (Some(client), Some(track)) = (&self.spotify, self.state.track()) else { return };
        let Some(uri) = crate::metadata::spotify_uri(track) else { return };

        let client = Arc::clone(client);
        let events = self.events_tx.clone();
        tokio::spawn(async move {
            let mut guard = client.lock().await;
            let liked = guard.is_saved(&uri).await;
            let available = guard.library_available();
            drop(guard);

            match liked {
                Ok(liked) => {
                    let _ = events.send(PlayerEvent::Liked { track_id: uri, liked }).await;
                }
                Err(e) => {
                    tracing::debug!("could not read the saved state: {e:#}");
                    if !available {
                        let _ = events.send(PlayerEvent::LibraryUnavailable).await;
                    }
                }
            }
        });
    }

    /// Fetch what is playing next.
    ///
    /// Tied to track changes and to the window opening rather than to a timer.
    /// The queue only moves when the track does, so polling it on a clock would
    /// spend rate limit re-reading an answer that has not changed.
    ///
    /// Unlike the device list, which describes the account and is true whatever
    /// is playing, a queue only means something when the thing playing is the
    /// thing the queue belongs to. Listing Spotify's next track underneath a
    /// YouTube video would be worse than listing nothing, so a non-Spotify track
    /// clears it instead.
    fn request_queue(&mut self) {
        if !current_is_spotify(&self.state) {
            if !self.state.spotify.queue.is_empty() {
                self.state.spotify.queue.clear();
                self.publish();
            }
            return;
        }
        if self.attention != Attention::Popup {
            return;
        }
        let Some(client) = &self.spotify else { return };
        let client = Arc::clone(client);
        let events = self.events_tx.clone();

        tokio::spawn(async move {
            match client.lock().await.queue().await {
                Ok(queue) => {
                    tracing::debug!(upcoming = queue.len(), "read the queue");
                    let _ = events.send(PlayerEvent::Queue(queue)).await;
                }
                Err(e) => tracing::debug!("could not read the queue: {e:#}"),
            }
        });
    }

    /// Ask Spotify what the current track is being played out of.
    ///
    /// Same rules as the queue: only with the window open, only when Spotify is
    /// what is playing. A playlist name belongs to Spotify's idea of playback,
    /// not to whatever MPRIS player happens to be attached.
    fn request_context(&mut self) {
        if !current_is_spotify(&self.state) {
            if self.state.spotify.context.take().is_some() {
                self.publish();
            }
            return;
        }
        if self.attention != Attention::Popup {
            return;
        }
        let Some(client) = &self.spotify else { return };
        let client = Arc::clone(client);
        let events = self.events_tx.clone();

        tokio::spawn(async move {
            match client.lock().await.context().await {
                Ok(context) => {
                    let _ = events.send(PlayerEvent::Context(context)).await;
                }
                Err(e) => tracing::debug!("could not read the playing context: {e:#}"),
            }
        });
    }

    /// Fetch the user's playlists.
    ///
    /// Only when something asks. A playlist list is a thing you go looking for,
    /// so it is fetched when the picker opens rather than kept current against
    /// a clock nobody is watching.
    fn request_playlists(&self) {
        let Some(client) = &self.spotify else { return };
        let client = Arc::clone(client);
        let events = self.events_tx.clone();

        tokio::spawn(async move {
            match client.lock().await.playlists().await {
                Ok(playlists) => {
                    let _ = events.send(PlayerEvent::Playlists(playlists)).await;
                }
                Err(e) => tracing::warn!("could not read your playlists: {e:#}"),
            }
        });
    }

    /// Run a search.
    ///
    /// An empty query clears the results rather than asking Spotify about
    /// nothing, which is what emptying the box means.
    fn request_search(&self, query: String) {
        let Some(client) = &self.spotify else { return };
        let client = Arc::clone(client);
        let events = self.events_tx.clone();

        tokio::spawn(async move {
            match client.lock().await.search(&query).await {
                Ok(results) => {
                    let _ = events.send(PlayerEvent::Search(results)).await;
                }
                Err(e) => tracing::debug!("search failed: {e:#}"),
            }
        });
    }

    /// Fetch what was played recently.
    ///
    /// Asked for when the list is opened, like the playlists. It is a record of
    /// the past, so it cannot go out of date in a way that matters.
    fn request_recent(&self) {
        let Some(client) = &self.spotify else { return };
        let client = Arc::clone(client);
        let events = self.events_tx.clone();

        tokio::spawn(async move {
            match client.lock().await.recently_played().await {
                Ok(recent) => {
                    let _ = events.send(PlayerEvent::Recent(recent)).await;
                }
                Err(e) => tracing::warn!("could not read recently played: {e:#}"),
            }
        });
    }

    /// Fetch everything in the playlist or album being played.
    fn request_context_tracks(&self) {
        let (Some(client), Some(context)) = (&self.spotify, self.state.spotify.context.clone())
        else {
            return;
        };
        let client = Arc::clone(client);
        let events = self.events_tx.clone();

        tokio::spawn(async move {
            match client.lock().await.context_tracks(&context).await {
                Ok(tracks) => {
                    let _ = events.send(PlayerEvent::ContextTracks(tracks)).await;
                }
                Err(e) => tracing::warn!("could not read what is in this: {e:#}"),
            }
        });
    }

    /// Refresh the Connect device list.
    ///
    /// Only called while the window is open. There is no push channel for this,
    /// so it has to be polled, and polling it with nothing watching would spend
    /// rate limit on an answer nobody would see.
    fn request_devices(&self) {
        let Some(client) = &self.spotify else { return };
        let client = Arc::clone(client);
        let events = self.events_tx.clone();

        tokio::spawn(async move {
            let mut guard = client.lock().await;
            let premium = guard.premium();
            match guard.devices().await {
                Ok(devices) => {
                    let devices = devices
                        .into_iter()
                        .filter_map(|d| {
                            // A device with no id cannot be transferred to, so
                            // listing it would be offering something that fails.
                            Some(waytify_ipc::Device {
                                id: d.id?,
                                name: d.name,
                                kind: d.kind,
                                is_active: d.is_active,
                                supports_volume: d.supports_volume,
                                volume_percent: d.volume_percent,
                            })
                        })
                        .collect();
                    let _ = events.send(PlayerEvent::Devices { devices, premium }).await;
                }
                Err(e) => tracing::debug!("could not list Connect devices: {e:#}"),
            }
        });
    }

    /// Fetch artwork for the current track, if it has any and we lack it.
    ///
    /// Spawned rather than awaited: a cover image is a network round trip, and
    /// blocking the loop on it would stall every other event behind it.
    /// Drop lyrics belonging to a track that is no longer playing.
    ///
    /// Called after the track has been replaced, with the key it had before.
    /// Keyed on the lyrics identity rather than the track id, so the same
    /// recording arriving under a new id, which is what a reconnect or a switch
    /// between players produces, keeps what was already fetched instead of
    /// blanking and asking again.
    fn forget_stale_lyrics(&mut self, previous: Option<String>) {
        if self.state.lyrics.is_some()
            && self.state.track().and_then(crate::lyrics::key_for) != previous
        {
            self.state.lyrics = None;
        }
    }

    /// Fetch lyrics for the current track.
    ///
    /// Only with the window open. Nothing else displays them, and lrclib is a
    /// volunteer-run service that should not be asked for something nobody is
    /// going to read.
    fn request_lyrics(&self) {
        if !self.config.lyrics.enabled || self.attention != Attention::Popup {
            return;
        }
        let Some(track) = self.state.track() else { return };
        // An episode has no lyrics, and asking lrclib about one by its title
        // spends somebody else's bandwidth on a guaranteed miss that then gets
        // cached for a week.
        if track.kind == waytify_ipc::MediaKind::Podcast {
            return;
        }
        let Some(key) = crate::lyrics::key_for(track) else { return };

        let track = track.clone();
        let events = self.events_tx.clone();
        tokio::spawn(async move {
            match crate::lyrics::fetch(&track).await {
                Ok(lyrics) => {
                    let _ = events.send(PlayerEvent::Lyrics { key, lyrics }).await;
                }
                // A track with no lyrics is the common case and is reported as
                // Ok(None). Reaching here means lrclib could not be asked, which
                // is not worth putting in front of anyone.
                Err(e) => tracing::debug!("could not fetch lyrics: {e:#}"),
            }
        });
    }

    fn request_artwork(&self) {
        let Some(track) = self.state.track() else { return };
        if track.art_path.is_some() {
            return;
        }
        let (Some(key), Some(url)) = (crate::art::key_for(track), track.art_url.clone()) else {
            return;
        };

        let events = self.events_tx.clone();
        tokio::spawn(async move {
            match crate::art::fetch(&url, &key, crate::art::DEFAULT_SURFACE).await {
                Ok(art) => {
                    let event = PlayerEvent::Artwork { key, path: art.path, colors: art.colors };
                    let _ = events.send(event).await;
                }
                // Missing art is a cosmetic loss, not a failure worth surfacing.
                Err(e) => tracing::debug!("could not fetch album art: {e:#}"),
            }
        });
    }

    async fn apply_properties(&mut self, changed: HashMap<String, OwnedValue>) {
        let now = Instant::now();
        let mut dirty = false;
        // Whether this signal told us the playback state outright. If it did,
        // that value stands: it is the player reporting its own transition, and
        // asking again straight afterwards can catch it mid-change and read the
        // state it is leaving rather than the one it is entering.
        let mut status_announced = false;

        tracing::debug!(
            props = ?changed.keys().collect::<Vec<_>>(),
            "properties changed"
        );

        if let Some(v) = changed.get("PlaybackStatus") {
            match as_str(v) {
                Some(s) => {
                    let status = mpris::parse_status(s);
                    tracing::debug!(%s, ?status, "playback status announced");
                    if let Some(p) = &mut self.state.player {
                        p.status = status;
                    }
                    self.clock.set_playing(status.is_playing(), now);
                    status_announced = true;
                    dirty = true;
                }
                // Announced in a shape we cannot read. Leaving the old value in
                // place is what made the bar show paused over music, so treat it
                // as if it had not been announced and go ask instead.
                None => tracing::warn!(
                    signature = ?v.value_signature(),
                    "PlaybackStatus arrived in an unreadable form"
                ),
            }
        }

        if let Some(v) = changed.get("Metadata") {
            let md: Result<Metadata, _> = v.clone().try_into();
            if let Ok(md) = md {
                let track = metadata::track_from_metadata(&md);
                let changed_track =
                    self.state.track().map(|t| &t.id) != track.as_ref().map(|t| &t.id);

                self.clock.set_length(track.as_ref().and_then(|t| t.length_ms));
                let was = self.state.track().and_then(crate::lyrics::key_for);
                if let Some(p) = &mut self.state.player {
                    p.track = track;
                }
                self.forget_stale_lyrics(was);
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
            // Only ask when this signal did not say. Spotify announces a track
            // change and the playback state that comes with it as two separate
            // signals, so a metadata change can arrive while our idea of the
            // state is already stale. Filling that gap is the point. Overwriting
            // a state the player just announced is not, and doing so reads it
            // mid-transition often enough to be worse than the original bug.
            if !status_announced {
                self.refresh_status(now).await;
            }
            self.refresh_position(now).await;
            self.recompute_caps();
            self.request_artwork();
            self.request_liked();
            self.request_queue();
            self.request_context();
            self.request_lyrics();
            self.publish();
        }
    }

    /// Read the playback state from the player.
    ///
    /// Only for when nothing announced it: a timer tick, or a signal that changed
    /// something else without mentioning the state. Returns whether it moved.
    async fn refresh_status(&mut self, now: Instant) -> bool {
        let Some(a) = &self.attached else { return false };
        let Ok(s) = a.player.playback_status().await else { return false };

        let status = mpris::parse_status(&s);
        let previous = self.state.player.as_ref().map(|p| p.status);
        self.clock.set_playing(status.is_playing(), now);

        if previous == Some(status) {
            return false;
        }
        tracing::debug!(?previous, ?status, "playback state corrected by a read");
        if let Some(p) = &mut self.state.player {
            p.status = status;
        }
        true
    }

    /// Re-anchor the position from the player if it has drifted noticeably.
    async fn refresh_position(&mut self, now: Instant) -> bool {
        let Some(a) = &self.attached else { return false };
        let Ok(us) = a.player.position().await else { return false };

        let observed = us_to_ms(us);
        if !self.clock.drifted(observed, now, DRIFT_TOLERANCE_MS) {
            return false;
        }
        self.clock.anchor(observed, self.clock.is_playing(), now);
        true
    }

    /// Fold a reading from the sound server into state.
    fn apply_audio(&mut self, snapshot: crate::audio::AudioSnapshot) {
        use waytify_ipc::VolumeRoute;

        self.state.audio = waytify_ipc::Audio {
            volume: snapshot.volume,
            muted: snapshot.muted,
            // A stream exists only while the player is producing audio locally.
            // Without one there is nothing here to attenuate, and the Spotify
            // layer will later claim this for a remote device instead.
            // A local stream wins: it always works, with no account, network or
            // Premium. Without one, a Connect device is the only thing left to
            // attenuate, and only if the account may write to it.
            route: if snapshot.volume.is_some() {
                VolumeRoute::Local
            } else if self.state.spotify.active_device.is_some()
                && self.state.spotify.can_control_remote()
            {
                VolumeRoute::Remote
            } else {
                VolumeRoute::Unavailable
            },
        };
        self.recompute_caps();
        self.publish();
    }

    /// How to recognise the current player's audio stream.
    ///
    /// Both halves matter. The MPRIS suffix matches the process name for some
    /// players and not others, and the process id catches the rest, including
    /// Chrome, whose stream comes from a child process under a different name.
    fn audio_owner(&self) -> Option<crate::audio::Owner> {
        let player = self.state.player.as_ref()?;
        Some(crate::audio::Owner {
            binary: mpris::short_name(&player.bus_name).to_string(),
            identity: player.identity.clone(),
            pid: self.attached.as_ref().and_then(|a| a.pid),
        })
    }

    fn refresh_audio(&self) {
        if let (Some(audio), Some(owner)) = (&self.audio, self.audio_owner()) {
            audio.send(crate::audio::Request::Refresh { owner });
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
            // Not merely "a track is playing": liking needs a Spotify catalogue
            // id, and a YouTube video in a browser tab has none. Showing the
            // control for one would be offering something that can only fail.
            //
            // Episodes included: /me/library takes a URI of either kind, so
            // saving one is the same call rather than a separate code path.
            can_like: self.state.spotify.authorized
                && self.state.spotify.library_available
                && self.state.track().and_then(crate::metadata::spotify_uri).is_some(),
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

            // Volume and output routing, through the sound server. A player with
            // no local stream reports this as unavailable rather than offering a
            // control that does nothing.
            Command::SetVolume { percent } => {
                // One slider, two targets. Which one is live depends on where the
                // audio is actually coming out.
                if self.state.audio.route == waytify_ipc::VolumeRoute::Remote {
                    let client = self
                        .spotify
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("no Spotify account connected"))?;
                    tokio::spawn(async move {
                        if let Err(e) = client.lock().await.set_remote_volume(percent).await {
                            tracing::warn!("could not set the remote volume: {e:#}");
                        }
                    });
                } else {
                    self.audio_request(|owner| crate::audio::Request::SetVolume {
                        owner,
                        percent: percent.min(100),
                    })?;
                }
            }
            Command::VolumeBy { delta } => {
                self.audio_request(|owner| crate::audio::Request::ChangeVolume { owner, delta })?
            }
            Command::ToggleMute => {
                self.audio_request(|owner| crate::audio::Request::ToggleMuted { owner })?
            }
            Command::PlayQueued { uri } => {
                let client = self
                    .spotify
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no Spotify account connected"))?;
                let context = self
                    .state
                    .spotify
                    .context
                    .as_ref()
                    .and_then(|c| c.uri.clone())
                    .ok_or_else(|| anyhow::anyhow!("nothing is playing from a playlist"))?;
                let uri = uri.clone();

                tokio::spawn(async move {
                    if let Err(e) = client.lock().await.play_at(&context, &uri).await {
                        tracing::warn!("could not play the queued item: {e:#}");
                    }
                });
            }
            Command::PlayContext { uri } => {
                let client = self
                    .spotify
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no Spotify account connected"))?;
                let uri = uri.clone();
                tokio::spawn(async move {
                    if let Err(e) = client.lock().await.play_context(&uri).await {
                        tracing::warn!("could not start that: {e:#}");
                    }
                });
            }
            Command::RefreshPlaylists => self.request_playlists(),
            Command::RefreshRecent => self.request_recent(),
            Command::RefreshContextTracks => self.request_context_tracks(),
            Command::Search { query } => self.request_search(query.clone()),
            Command::PlayTrack { uri } => {
                let client = self
                    .spotify
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no Spotify account connected"))?;
                let uri = uri.clone();
                tokio::spawn(async move {
                    if let Err(e) = client.lock().await.play_track(&uri).await {
                        tracing::warn!("could not play that: {e:#}");
                    }
                });
            }
            Command::RefreshDevices => self.request_devices(),
            Command::ToggleLike => {
                let client = self
                    .spotify
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no Spotify account connected"))?;
                let track = self.state.track().ok_or_else(|| anyhow::anyhow!("nothing playing"))?;
                let uri = crate::metadata::spotify_uri(track)
                    .ok_or_else(|| anyhow::anyhow!("this track is not from Spotify"))?;
                let wanted = !track.liked.unwrap_or(false);

                let events = self.events_tx.clone();
                tokio::spawn(async move {
                    let mut guard = client.lock().await;
                    match guard.set_saved(&uri, wanted).await {
                        // Report what was asked for rather than reading it back:
                        // the library is eventually consistent and an immediate
                        // re-read often still says the old value.
                        Ok(()) => {
                            let _ = events
                                .send(PlayerEvent::Liked { track_id: uri, liked: wanted })
                                .await;
                        }
                        Err(e) => tracing::warn!("could not change the saved state: {e:#}"),
                    }
                });
            }
            Command::TransferTo { device_id } => {
                let client = self
                    .spotify
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no Spotify account connected"))?;
                let events = self.events_tx.clone();
                tokio::spawn(async move {
                    let mut guard = client.lock().await;
                    if let Err(e) = guard.transfer_to(&device_id).await {
                        tracing::warn!("could not transfer playback: {e:#}");
                    }
                    let premium = guard.premium();
                    drop(guard);
                    // Whatever happened, the device list has probably changed.
                    let _ =
                        events.send(PlayerEvent::Devices { devices: Vec::new(), premium }).await;
                });
            }

            // Handled by the daemon before reaching here: window state belongs to
            // the popup process, and subscription and shutdown are connection
            // concerns rather than player ones.
            Command::TogglePopup { .. }
            | Command::ShowPopup { .. }
            | Command::HidePopup
            | Command::Subscribe { .. }
            | Command::Watching { .. }
            | Command::Shutdown => {}
        }
        Ok(())
    }

    /// Send an audio request for the current player, or explain why not.
    fn audio_request(
        &self,
        build: impl FnOnce(crate::audio::Owner) -> crate::audio::Request,
    ) -> Result<()> {
        let Some(audio) = &self.audio else {
            anyhow::bail!("no sound server is available");
        };
        let Some(owner) = self.audio_owner() else {
            anyhow::bail!("no player is running");
        };
        audio.send(build(owner));
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
//
// Both unwrap a nested variant. `PropertiesChanged` carries `a{sv}`, and whether
// the value arrives unwrapped or as a `Value::Value` depends on the sender. Not
// handling the wrapped form means silently ignoring the property, which for
// PlaybackStatus means the bar keeps showing whatever it last believed.
fn as_str<'a>(v: &'a zbus::zvariant::Value<'a>) -> Option<&'a str> {
    match v {
        zbus::zvariant::Value::Str(s) => Some(s.as_str()),
        zbus::zvariant::Value::Value(inner) => as_str(inner),
        _ => None,
    }
}

/// Whether what is playing is Spotify's own playback.
///
/// A Spotify catalogue id on the current track is the signal, rather than the
/// name of the attached player: playback on a phone over Connect has no local
/// MPRIS player at all, and the queue is still real in that case.
fn current_is_spotify(state: &State) -> bool {
    state.track().and_then(crate::metadata::spotify_catalogue_id).is_some()
}

fn as_bool(v: &zbus::zvariant::Value<'_>) -> Option<bool> {
    match v {
        zbus::zvariant::Value::Bool(b) => Some(*b),
        zbus::zvariant::Value::Value(inner) => as_bool(inner),
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

    fn state_playing(url: Option<&str>) -> State {
        let player = Player {
            bus_name: "org.mpris.MediaPlayer2.test".into(),
            identity: "Test".into(),
            status: Status::Playing,
            track: Some(waytify_ipc::Track {
                title: "Something".into(),
                url: url.map(Into::into),
                ..Default::default()
            }),
            position_ms: 0,
            shuffle: None,
            repeat: None,
        };
        State { player: Some(player), ..Default::default() }
    }

    #[test]
    fn a_spotify_track_owns_the_queue() {
        let state = state_playing(Some("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"));
        assert!(current_is_spotify(&state));
    }

    #[test]
    fn another_players_track_does_not_own_the_queue() {
        // A browser video is the case that matters: the account still has a
        // queue, but listing it under something else is describing the wrong
        // thing.
        assert!(!current_is_spotify(&state_playing(Some("https://youtube.com/watch?v=x"))));
        assert!(!current_is_spotify(&state_playing(None)));
        assert!(!current_is_spotify(&State::default()), "nothing playing owns nothing");
    }
}
