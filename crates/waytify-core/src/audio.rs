//! Volume and output routing, through PulseAudio's API.
//!
//! On a modern desktop this is PipeWire answering on the PulseAudio protocol.
//! That is deliberate: `libpulse` is stable, ubiquitous, and far simpler than
//! talking to PipeWire natively, and it works on an actual PulseAudio system too.
//!
//! Two things are controlled here, and they are not the same thing.
//!
//! A *sink* is an output device. A *sink input* is one application's stream
//! feeding into it. waytify adjusts the player's own stream, not the system
//! volume, because a media widget quietly moving the master slider is a bad
//! surprise. Moving that stream between sinks is what "play through the
//! headphones instead" means.
//!
//! libpulse is callback driven with its own main loop, so it lives on a dedicated
//! thread and is spoken to over channels. Nothing here touches the async runtime.

use anyhow::{Context as _, Result, anyhow};
use libpulse_binding as pulse;
use pulse::callbacks::ListResult;
use pulse::context::subscribe::{Facility, InterestMaskSet};
use pulse::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use pulse::mainloop::threaded::Mainloop;
use pulse::volume::{ChannelVolumes, Volume};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use waytify_ipc::Sink;

/// How long to wait for the sound server before giving up.
///
/// A desktop without one is a normal situation, not an error worth blocking on:
/// waytify still shows and controls the player, just without volume.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// How to recognise the player's own audio stream.
///
/// Matching on the process name alone is not enough. Chrome claims MPRIS as
/// `org.mpris.MediaPlayer2.chromium` while its stream is owned by a process
/// called `chrome`, and the stream comes from a separate audio child process
/// rather than the one holding the bus name. So the process id is carried too,
/// and a stream counts as the player's if it belongs to that process or to any
/// of its descendants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Owner {
    /// Short name from the MPRIS bus name, for example `spotify`.
    pub binary: String,
    /// The process holding the bus name, when the bus could say.
    pub pid: Option<u32>,
}

impl Owner {
    /// Whether a stream belongs to this player.
    pub fn owns(&self, stream_pid: Option<u32>, stream_binary: &str) -> bool {
        if !stream_binary.is_empty() && stream_binary.eq_ignore_ascii_case(&self.binary) {
            return true;
        }
        let (Some(stream_pid), Some(player_pid)) = (stream_pid, self.pid) else {
            return false;
        };
        stream_pid == player_pid || is_descendant(stream_pid, player_pid)
    }
}

/// What the engine wants done. Sent to the audio thread, which owns the
/// connection.
#[derive(Debug, Clone)]
pub enum Request {
    /// Absolute volume for the player's own stream, 0 to 100.
    SetVolume {
        owner: Owner,
        percent: u8,
    },
    /// Relative change, clamped to 0 to 100 by the audio thread since only it
    /// knows the current value.
    ChangeVolume {
        owner: Owner,
        delta: i8,
    },
    SetMuted {
        owner: Owner,
        muted: bool,
    },
    ToggleMuted {
        owner: Owner,
    },
    /// Move the player's stream to a different output device.
    MoveToSink {
        owner: Owner,
        sink: String,
    },
    /// Re-read everything and publish, used on connect and on any server event.
    Refresh {
        owner: Owner,
    },
}

/// Whether `pid` is a descendant of `ancestor`, by walking parents in `/proc`.
///
/// Bounded so a malformed or cyclic tree cannot spin here. Depth of 16 is far
/// more than any real process nesting.
fn is_descendant(pid: u32, ancestor: u32) -> bool {
    let mut current = pid;
    for _ in 0..16 {
        let Some(parent) = parent_pid(current) else { return false };
        if parent == ancestor {
            return true;
        }
        if parent <= 1 {
            return false;
        }
        current = parent;
    }
    false
}

fn parent_pid(pid: u32) -> Option<u32> {
    // Field 4 of /proc/<pid>/stat is the parent. The process name in field 2 can
    // contain spaces and brackets, so parsing starts after its closing bracket.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(')')?.1;
    after_name.split_whitespace().nth(1)?.parse().ok()
}

/// What the audio thread found. Mirrors the shape the state model wants.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AudioSnapshot {
    /// `None` when the player has no stream, which is normal while paused on
    /// some players and always true when playback is on a remote device.
    pub volume: Option<u8>,
    pub muted: Option<bool>,
    pub sinks: Vec<Sink>,
    /// The sink the player's stream is attached to.
    pub active_sink: Option<String>,
}

/// A handle to the audio thread.
pub struct Audio {
    requests: std::sync::mpsc::Sender<Request>,
}

impl Audio {
    /// Connect and start watching. Returns the handle and a receiver of
    /// snapshots, which is written to on every server event.
    ///
    /// Failing to connect is not fatal. The error is returned so the caller can
    /// log it once and carry on without volume control rather than refusing to
    /// start.
    pub fn connect() -> Result<(Self, tokio::sync::mpsc::UnboundedReceiver<AudioSnapshot>)> {
        let (request_tx, request_rx) = std::sync::mpsc::channel::<Request>();
        let (snapshot_tx, snapshot_rx) = tokio::sync::mpsc::unbounded_channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

        std::thread::Builder::new()
            .name("waytify-audio".into())
            .spawn(move || match Worker::start() {
                Ok(worker) => {
                    let _ = ready_tx.send(Ok(()));
                    worker.run(request_rx, snapshot_tx);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .context("spawning the audio thread")?;

        ready_rx
            .recv_timeout(CONNECT_TIMEOUT)
            .map_err(|_| anyhow!("the sound server did not respond within {CONNECT_TIMEOUT:?}"))?
            .context("connecting to the sound server")?;

        Ok((Self { requests: request_tx }, snapshot_rx))
    }

    /// Queue a request. Never blocks, so it is safe from the engine's loop.
    pub fn send(&self, request: Request) {
        if self.requests.send(request).is_err() {
            tracing::debug!("the audio thread has gone away");
        }
    }
}

struct Worker {
    // Declaration order is load bearing. Rust drops fields in order, and libpulse
    // aborts the process with an assertion if a context outlives the main loop it
    // was created against, because freeing its IO events touches a dead loop.
    // Context first, main loop second.
    context: Context,
    mainloop: Mainloop,
}

impl Worker {
    fn start() -> Result<Self> {
        let mut mainloop =
            Mainloop::new().ok_or_else(|| anyhow!("could not create a main loop"))?;
        let mut context = Context::new(&mainloop, "waytify")
            .ok_or_else(|| anyhow!("could not create a context"))?;

        context.connect(None, ContextFlagSet::NOFLAGS, None).context("connecting")?;
        mainloop.start().context("starting the main loop")?;

        // Wait for the connection to settle. The alternative is discovering it
        // failed later, inside a callback, where there is nothing to return it to.
        let deadline = std::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            match context.get_state() {
                ContextState::Ready => break,
                ContextState::Failed | ContextState::Terminated => {
                    return Err(anyhow!("the sound server refused the connection"));
                }
                _ if std::time::Instant::now() > deadline => {
                    return Err(anyhow!("timed out waiting for the sound server"));
                }
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }

        Ok(Self { context, mainloop })
    }

    fn run(
        mut self,
        requests: std::sync::mpsc::Receiver<Request>,
        snapshots: tokio::sync::mpsc::UnboundedSender<AudioSnapshot>,
    ) {
        // Anything the server changes should reach the window without it having
        // to ask, so that changing volume elsewhere is reflected here.
        let dirty = Arc::new(Mutex::new(false));
        {
            let dirty = Arc::clone(&dirty);
            self.context.set_subscribe_callback(Some(Box::new(move |facility, _, _| {
                if matches!(facility, Some(Facility::Sink) | Some(Facility::SinkInput)) {
                    *dirty.lock().unwrap() = true;
                }
            })));
            self.context.subscribe(InterestMaskSet::SINK | InterestMaskSet::SINK_INPUT, |_| {});
        }

        let mut owner = Owner::default();
        let mut last = AudioSnapshot::default();

        loop {
            // A short block rather than a busy loop: requests are rare and server
            // events are collapsed into a flag, so waking ten times a second is
            // enough to feel immediate and cheap enough not to matter.
            match requests.recv_timeout(Duration::from_millis(100)) {
                Ok(request) => {
                    owner = request_owner(&request).clone();
                    self.apply(&request);
                    *dirty.lock().unwrap() = true;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }

            let changed = std::mem::replace(&mut *dirty.lock().unwrap(), false);
            if !changed || owner == Owner::default() {
                continue;
            }

            if let Some(snapshot) = self.snapshot(&owner)
                && snapshot != last
            {
                last = snapshot.clone();
                if snapshots.send(snapshot).is_err() {
                    break;
                }
            }
        }

        // Disconnect before stopping, so the context has no live IO registered
        // against a loop that is about to go away.
        self.context.disconnect();
        self.mainloop.stop();
    }

    fn apply(&mut self, request: &Request) {
        match request {
            Request::Refresh { .. } => {}
            Request::SetVolume { owner, percent } => {
                if let Some(input) = self.find_input(owner) {
                    let mut volumes = input.volume;
                    volumes.set(volumes.len(), percent_to_volume(*percent));
                    self.context.introspect().set_sink_input_volume(input.index, &volumes, None);
                }
            }
            Request::ChangeVolume { owner, delta } => {
                if let Some(input) = self.find_input(owner) {
                    let current = volume_to_percent(input.volume.avg()) as i16;
                    let target = (current + i16::from(*delta)).clamp(0, 100) as u8;
                    let mut volumes = input.volume;
                    volumes.set(volumes.len(), percent_to_volume(target));
                    self.context.introspect().set_sink_input_volume(input.index, &volumes, None);
                }
            }
            Request::SetMuted { owner, muted } => {
                if let Some(input) = self.find_input(owner) {
                    self.context.introspect().set_sink_input_mute(input.index, *muted, None);
                }
            }
            Request::ToggleMuted { owner } => {
                if let Some(input) = self.find_input(owner) {
                    self.context.introspect().set_sink_input_mute(input.index, !input.muted, None);
                }
            }
            Request::MoveToSink { owner, sink } => {
                if let Some(input) = self.find_input(owner) {
                    self.context.introspect().move_sink_input_by_name(input.index, sink, None);
                }
            }
        }
    }

    /// Read the current sinks and the player's stream.
    fn snapshot(&mut self, owner: &Owner) -> Option<AudioSnapshot> {
        let sinks = self.list_sinks();
        let input = self.find_input(owner);

        Some(AudioSnapshot {
            volume: input.as_ref().map(|i| volume_to_percent(i.volume.avg())),
            muted: input.as_ref().map(|i| i.muted),
            active_sink: input
                .as_ref()
                .and_then(|i| sinks.iter().find(|s| s.index == i.sink).map(|s| s.name.clone())),
            sinks: sinks
                .into_iter()
                .map(|s| Sink {
                    name: s.name,
                    description: s.description,
                    is_default: s.is_default,
                    battery_percent: None,
                })
                .collect(),
        })
    }

    fn list_sinks(&mut self) -> Vec<SinkInfo> {
        let collected = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&collected);
        let op = self.context.introspect().get_sink_info_list(move |result| {
            if let ListResult::Item(info) = result {
                sink.lock().unwrap().push(SinkInfo {
                    index: info.index,
                    name: info.name.as_deref().unwrap_or_default().to_string(),
                    description: info.description.as_deref().unwrap_or_default().to_string(),
                    is_default: false,
                });
            }
        });
        self.wait(op);

        let mut sinks = Arc::try_unwrap(collected)
            .map(|m| m.into_inner().unwrap())
            .unwrap_or_else(|arc| arc.lock().unwrap().clone());

        // Which sink is the default is a server property rather than something on
        // the sinks themselves.
        if let Some(default) = self.default_sink_name() {
            for sink in &mut sinks {
                sink.is_default = sink.name == default;
            }
        }
        sinks
    }

    fn default_sink_name(&mut self) -> Option<String> {
        let name = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&name);
        let op = self.context.introspect().get_server_info(move |info| {
            *sink.lock().unwrap() = info.default_sink_name.as_deref().map(str::to_string);
        });
        self.wait(op);
        // Cloned out before the guard drops at the end of the expression.
        let name = name.lock().unwrap();
        name.clone()
    }

    /// The player's own stream, matched on the process that owns it.
    ///
    /// `application.process.binary` rather than `application.name`, which is
    /// localised and cosmetic and therefore not something to match on.
    fn find_input(&mut self, owner: &Owner) -> Option<InputInfo> {
        let collected = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&collected);
        let wanted = owner.clone();

        let op = self.context.introspect().get_sink_input_info_list(move |result| {
            let ListResult::Item(info) = result else { return };
            let binary = info.proplist.get_str("application.process.binary").unwrap_or_default();
            let pid =
                info.proplist.get_str("application.process.id").and_then(|v| v.parse::<u32>().ok());
            if wanted.owns(pid, &binary) {
                let mut slot = sink.lock().unwrap();
                // Some players hold several streams. The first is as good a
                // choice as any and at least it is a stable one.
                if slot.is_none() {
                    *slot = Some(InputInfo {
                        index: info.index,
                        sink: info.sink,
                        volume: info.volume,
                        muted: info.mute,
                    });
                }
            }
        });
        self.wait(op);

        let found = collected.lock().unwrap();
        found.clone()
    }

    /// Block until an introspection operation finishes.
    ///
    /// The main loop runs on its own thread, so this only parks the audio thread,
    /// never the caller's.
    fn wait<C: ?Sized>(&mut self, op: pulse::operation::Operation<C>) {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while op.get_state() == pulse::operation::State::Running {
            if std::time::Instant::now() > deadline {
                tracing::debug!("an audio query did not finish in time");
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

#[derive(Clone)]
struct SinkInfo {
    index: u32,
    name: String,
    description: String,
    is_default: bool,
}

#[derive(Clone)]
struct InputInfo {
    index: u32,
    sink: u32,
    volume: ChannelVolumes,
    muted: bool,
}

fn request_owner(request: &Request) -> &Owner {
    match request {
        Request::SetVolume { owner, .. }
        | Request::ChangeVolume { owner, .. }
        | Request::SetMuted { owner, .. }
        | Request::ToggleMuted { owner }
        | Request::MoveToSink { owner, .. }
        | Request::Refresh { owner } => owner,
    }
}

/// PulseAudio volumes are not percentages. `NORMAL` is 100%, and values above it
/// are amplification, which this deliberately does not offer.
pub fn volume_to_percent(volume: Volume) -> u8 {
    let normal = Volume::NORMAL.0 as f64;
    ((volume.0 as f64 / normal) * 100.0).round().clamp(0.0, 100.0) as u8
}

pub fn percent_to_volume(percent: u8) -> Volume {
    let normal = Volume::NORMAL.0 as f64;
    Volume(((percent.min(100) as f64 / 100.0) * normal).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_round_trip_through_pulse_volumes() {
        for percent in [0, 1, 25, 50, 75, 99, 100] {
            let back = volume_to_percent(percent_to_volume(percent));
            assert!(back.abs_diff(percent) <= 1, "{percent}% became {back}% after a round trip");
        }
    }

    #[test]
    fn normal_volume_is_a_hundred_percent() {
        assert_eq!(volume_to_percent(Volume::NORMAL), 100);
        assert_eq!(percent_to_volume(100), Volume::NORMAL);
    }

    #[test]
    fn silence_is_zero() {
        assert_eq!(volume_to_percent(Volume::MUTED), 0);
    }

    #[test]
    fn amplification_is_not_offered() {
        // Pulse allows well above NORMAL. Reporting 150% in a slider that only
        // goes to 100 would make the thumb sit at the end and lie about it.
        let loud = Volume(Volume::NORMAL.0 * 3 / 2);
        assert_eq!(volume_to_percent(loud), 100);
    }

    #[test]
    fn a_stream_matches_its_player_by_name() {
        let owner = Owner { binary: "spotify".into(), pid: Some(1234) };
        assert!(owner.owns(Some(9999), "spotify"), "the name alone should be enough");
        assert!(owner.owns(None, "Spotify"), "matching is case insensitive");
    }

    #[test]
    fn a_stream_matches_its_player_by_process() {
        // Chrome is the reason this exists: MPRIS says chromium, the stream says
        // chrome, so only the process identity connects the two.
        let owner = Owner { binary: "chromium".into(), pid: Some(1234) };
        assert!(owner.owns(Some(1234), "chrome"), "same process should match");
    }

    #[test]
    fn an_unrelated_stream_is_not_claimed() {
        let owner = Owner { binary: "spotify".into(), pid: Some(1234) };
        assert!(!owner.owns(Some(5678), "firefox"));
        assert!(!owner.owns(None, "firefox"));
        // An empty binary must not match an empty configured name by accident.
        assert!(!Owner::default().owns(None, ""));
    }

    #[test]
    fn this_process_is_its_own_descendant_check() {
        // Exercises the /proc walk against something guaranteed to exist: the
        // test process is a descendant of its own parent.
        let me = std::process::id();
        let parent = parent_pid(me).expect("this process should have a parent");
        assert!(is_descendant(me, parent), "{me} should descend from {parent}");
        assert!(!is_descendant(parent, me), "ancestry should not run backwards");
    }

    #[test]
    fn requests_all_carry_the_player_they_are_about() {
        // The audio thread remembers the last player it was told about, so a
        // request that did not name one would act on whatever came before it.
        let owner = Owner { binary: "spotify".into(), pid: Some(42) };
        let requests = [
            Request::SetVolume { owner: owner.clone(), percent: 50 },
            Request::ChangeVolume { owner: owner.clone(), delta: -5 },
            Request::SetMuted { owner: owner.clone(), muted: true },
            Request::ToggleMuted { owner: owner.clone() },
            Request::MoveToSink { owner: owner.clone(), sink: "x".into() },
            Request::Refresh { owner: owner.clone() },
        ];
        for request in requests {
            assert_eq!(request_owner(&request), &owner, "{request:?}");
        }
    }
}
