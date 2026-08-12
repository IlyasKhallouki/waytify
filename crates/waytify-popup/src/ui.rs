//! The widget tree and how state is bound onto it.
//!
//! Widget names and CSS classes here are public API. Themeability is a promise
//! not to rename things, so anything a stylesheet can select is documented in
//! `docs/THEMING.md` and changing it is a breaking change.

use crate::client::Client;
use gtk4::glib;
use gtk4::prelude::*;
use std::cell::Cell;
use std::rc::Rc;
use waytify_ipc::{Command, State, Status};

/// Album art edge length in logical pixels. Square, because every source of
/// cover art is.
const ART_SIZE: i32 = 96;

/// How long the volume slider has to stop moving before the change is sent.
const VOLUME_SETTLE: std::time::Duration = std::time::Duration::from_millis(60);

/// How long the scrubber has to stop moving before the seek is sent.
///
/// Long enough that a drag produces one seek at the end rather than one per
/// motion event, short enough that a single click still feels immediate.
const SEEK_SETTLE: std::time::Duration = std::time::Duration::from_millis(150);

/// How many upcoming tracks to list.
///
/// Spotify returns up to twenty, which would make the window taller than most
/// bars have room below them. Five is enough to answer "what is after this" and
/// still leaves the window a fixed, predictable size.
const QUEUE_ROWS: usize = 5;

pub struct Ui {
    pub root: gtk4::Box,
    art: gtk4::Image,
    art_placeholder: gtk4::Box,
    title: gtk4::Label,
    artist: gtk4::Label,
    album: gtk4::Label,
    elapsed: gtk4::Label,
    duration: gtk4::Label,
    scrubber: gtk4::Scale,
    play_pause: gtk4::Button,
    shuffle: gtk4::ToggleButton,
    repeat: gtk4::Button,
    like: gtk4::Button,
    volume_row: gtk4::Box,
    volume: gtk4::Scale,
    mute: gtk4::Button,
    output: gtk4::MenuButton,
    output_list: gtk4::Box,
    queue: gtk4::Box,
    queue_list: gtk4::Box,
    /// The queue as last rendered, so rows are rebuilt when it moves rather than
    /// on every state frame.
    listed_queue: std::cell::RefCell<Vec<waytify_ipc::Track>>,
    /// Sinks currently listed in the picker, so the list is only rebuilt when the
    /// set of outputs actually changes rather than on every state frame.
    listed_sinks: std::cell::RefCell<(Vec<waytify_ipc::Sink>, Vec<waytify_ipc::Device>)>,
    /// True while the user has hold of the scrubber. Position updates from the
    /// daemon are ignored during that time, so the thumb does not fight the
    /// pointer, and the label follows the thumb instead.
    dragging: Rc<Cell<bool>>,
    /// Length of the current track, needed to turn a scrubber fraction back into
    /// a position. Zero means unknown, and the scrubber is disabled.
    length_ms: Rc<Cell<u64>>,
    /// Suppresses the handler while state is being written into widgets, so
    /// programmatic updates are not mistaken for user input.
    binding: Rc<Cell<bool>>,
    /// Kept so the output picker can be rebuilt after construction, when the
    /// list of sinks changes.
    client: Rc<Client>,
    /// Last art colours pushed into the stylesheet. State arrives once a second
    /// while playing, and reparsing CSS on each of those for colours that only
    /// change with the track would be wasteful.
    art_colors: std::cell::RefCell<Option<waytify_ipc::ArtColors>>,
}

impl Ui {
    pub fn build(client: Rc<Client>) -> Self {
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
        root.set_widget_name("waytify-popup");

        // Header: art beside the track's identity.
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 14);
        header.add_css_class("waytify-header");

        // Image rather than Picture. A Picture reports the image's own resolution
        // as its natural size and has no maximum, so a 640px cover stretches the
        // layout to whatever it feels like. Image::set_pixel_size is the API that
        // actually means "draw it this big".
        let art = gtk4::Image::new();
        art.add_css_class("waytify-art");
        art.set_pixel_size(ART_SIZE);

        // A separate widget rather than a fallback image, so a theme can style
        // "no art yet" differently from art that happens to be dark.
        let art_placeholder = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        art_placeholder.add_css_class("waytify-art");
        art_placeholder.add_css_class("art-missing");
        art_placeholder.set_size_request(ART_SIZE, ART_SIZE);

        let meta = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        meta.add_css_class("waytify-meta");
        meta.set_valign(gtk4::Align::Center);
        meta.set_hexpand(true);

        let title = label("track-title");
        let artist = label("track-artist");
        let album = label("track-album");
        meta.append(&title);
        meta.append(&artist);
        meta.append(&album);

        let like = gtk4::Button::from_icon_name("non-starred-symbolic");
        like.add_css_class("like");
        like.add_css_class("flat");
        like.set_valign(gtk4::Align::Center);

        header.append(&art);
        header.append(&art_placeholder);
        header.append(&meta);
        header.append(&like);

        // Scrubber row.
        let scrub_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        scrub_row.add_css_class("waytify-scrubber");

        let elapsed = label("elapsed");
        let duration = label("duration");
        elapsed.set_xalign(0.0);
        duration.set_xalign(1.0);

        let scrubber = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, 0.0, 1.0, 0.001);
        scrubber.add_css_class("scrubber");
        scrubber.set_draw_value(false);
        scrubber.set_hexpand(true);

        scrub_row.append(&elapsed);
        scrub_row.append(&scrubber);
        scrub_row.append(&duration);

        // Transport row.
        let transport = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        transport.add_css_class("waytify-transport");
        transport.set_halign(gtk4::Align::Center);

        let shuffle = toggle("media-playlist-shuffle-symbolic", "shuffle");
        let prev = button("media-skip-backward-symbolic", "prev");
        let play_pause = button("media-playback-start-symbolic", "playpause");
        let next = button("media-skip-forward-symbolic", "next");
        let repeat = button("media-playlist-repeat-symbolic", "repeat");

        transport.append(&shuffle);
        transport.append(&prev);
        transport.append(&play_pause);
        transport.append(&next);
        transport.append(&repeat);

        // Volume and output routing.
        let volume_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        volume_row.add_css_class("waytify-volume");

        let mute = gtk4::Button::from_icon_name("audio-volume-high-symbolic");
        mute.add_css_class("mute");
        mute.add_css_class("flat");

        let volume = gtk4::Scale::with_range(gtk4::Orientation::Horizontal, 0.0, 100.0, 1.0);
        volume.add_css_class("volume-slider");
        volume.set_draw_value(false);
        volume.set_hexpand(true);

        // A menu button rather than a cycle button: there can be any number of
        // outputs and picking blindly through them is not a control.
        let output = gtk4::MenuButton::new();
        output.set_icon_name("audio-headphones-symbolic");
        output.add_css_class("output");
        output.add_css_class("flat");
        let outputs = gtk4::Popover::new();
        outputs.add_css_class("waytify-outputs");
        let output_list = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        outputs.set_child(Some(&output_list));
        output.set_popover(Some(&outputs));

        volume_row.append(&mute);
        volume_row.append(&volume);
        volume_row.append(&output);

        // Read only. Spotify has no endpoint for jumping to an arbitrary queue
        // position, so these rows are labels rather than buttons: a row that
        // looked clickable and did nothing would be worse than one that does not.
        let queue = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        queue.add_css_class("waytify-queue");
        let queue_heading = label("queue-heading");
        queue_heading.set_text("Up next");
        let queue_list = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        queue.append(&queue_heading);
        queue.append(&queue_list);

        root.append(&header);
        root.append(&scrub_row);
        root.append(&volume_row);
        root.append(&transport);
        root.append(&queue);

        let ui = Self {
            root,
            art,
            art_placeholder,
            title,
            artist,
            album,
            elapsed,
            duration,
            scrubber,
            play_pause,
            shuffle,
            repeat,
            like,
            volume_row,
            volume,
            mute,
            output,
            output_list,
            queue,
            queue_list,
            listed_queue: std::cell::RefCell::new(Vec::new()),
            listed_sinks: std::cell::RefCell::new((Vec::new(), Vec::new())),
            client: Rc::clone(&client),
            dragging: Rc::new(Cell::new(false)),
            length_ms: Rc::new(Cell::new(0)),
            binding: Rc::new(Cell::new(false)),
            art_colors: std::cell::RefCell::new(None),
        };

        ui.wire(&client, &prev, &next);
        ui
    }

    fn wire(&self, client: &Rc<Client>, prev: &gtk4::Button, next: &gtk4::Button) {
        let send = |client: &Rc<Client>, command: Command| {
            let client = Rc::clone(client);
            move |_: &gtk4::Button| client.send(command.clone())
        };

        prev.connect_clicked(send(client, Command::Previous));
        next.connect_clicked(send(client, Command::Next));
        self.play_pause.connect_clicked(send(client, Command::PlayPause));
        self.repeat.connect_clicked(send(client, Command::CycleRepeat));

        {
            let client = Rc::clone(client);
            let binding = Rc::clone(&self.binding);
            self.shuffle.connect_toggled(move |b| {
                if binding.get() {
                    return;
                }
                client.send(Command::SetShuffle { on: b.is_active() });
            });
        }

        // Seeking is driven from `change-value`, which is the signal GtkScale
        // emits for user-initiated changes only, so it cannot be confused with
        // the position being written in from the daemon.
        //
        // A GestureClick on the scale does not work here: the scale claims the
        // pointer sequence with its own internal drag gesture, so an added click
        // gesture never sees the press or the release. Rather than fight it for
        // release detection, the seek fires once the value stops changing. That
        // behaves like release for a drag, and like an immediate seek for a click,
        // without needing to know which one happened.
        let pending = Rc::new(Cell::new(0u64));
        let debounce: Rc<std::cell::RefCell<Option<glib::SourceId>>> =
            Rc::new(std::cell::RefCell::new(None));

        let dragging = Rc::clone(&self.dragging);
        let length = Rc::clone(&self.length_ms);
        let elapsed = self.elapsed.clone();
        let client = Rc::clone(client);

        {
            let client = Rc::clone(&self.client);
            self.mute.connect_clicked(move |_| client.send(Command::ToggleMute));
        }

        {
            let client = Rc::clone(&self.client);
            self.like.connect_clicked(move |_| client.send(Command::ToggleLike));
        }

        // Same settle-then-send shape as the scrubber, for the same reason: a
        // drag would otherwise produce a request per motion event. Shorter,
        // because a volume change is a local call rather than a D-Bus round trip
        // and should feel immediate.
        {
            let client = Rc::clone(&self.client);
            let binding = Rc::clone(&self.binding);
            let pending = Rc::new(Cell::new(0u8));
            let debounce: Rc<std::cell::RefCell<Option<glib::SourceId>>> =
                Rc::new(std::cell::RefCell::new(None));

            self.volume.connect_change_value(move |_, _, value| {
                if binding.get() {
                    return glib::Propagation::Proceed;
                }
                pending.set(value.clamp(0.0, 100.0) as u8);

                if let Some(previous) = debounce.borrow_mut().take() {
                    previous.remove();
                }
                let client = Rc::clone(&client);
                let pending = Rc::clone(&pending);
                let debounce_inner = Rc::clone(&debounce);
                *debounce.borrow_mut() =
                    Some(glib::timeout_add_local_once(VOLUME_SETTLE, move || {
                        client.send(Command::SetVolume { percent: pending.get() });
                        debounce_inner.borrow_mut().take();
                    }));

                glib::Propagation::Proceed
            });
        }

        self.scrubber.connect_change_value(move |_, _, value| {
            let len = length.get();
            if len == 0 {
                return glib::Propagation::Proceed;
            }

            let position_ms = (value.clamp(0.0, 1.0) * len as f64) as u64;
            pending.set(position_ms);
            // Freeze incoming positions and move the label with the thumb, so the
            // drag reads as a scrub rather than a slider with a lagging number.
            dragging.set(true);
            elapsed.set_text(&format_time(position_ms));

            if let Some(previous) = debounce.borrow_mut().take() {
                previous.remove();
            }
            let client = Rc::clone(&client);
            let dragging = Rc::clone(&dragging);
            let pending = Rc::clone(&pending);
            let debounce_inner = Rc::clone(&debounce);
            *debounce.borrow_mut() = Some(glib::timeout_add_local_once(SEEK_SETTLE, move || {
                client.send(Command::Seek { position_ms: pending.get() });
                dragging.set(false);
                debounce_inner.borrow_mut().take();
            }));

            glib::Propagation::Proceed
        });
    }

    /// Write a state onto the widgets.
    pub fn render(&self, state: &State) {
        self.binding.set(true);

        let colors = state.track().and_then(|t| t.colors);
        if *self.art_colors.borrow() != colors {
            crate::style::set_art_colors(colors);
            *self.art_colors.borrow_mut() = colors;
        }

        let status = state.status();
        for class in ["playing", "paused", "stopped", "no-player", "liked", "remote", "no-premium"]
        {
            self.root.remove_css_class(class);
        }
        for class in state.css_classes() {
            self.root.add_css_class(&class);
        }

        self.play_pause.set_icon_name(match status {
            Status::Playing => "media-playback-pause-symbolic",
            _ => "media-playback-start-symbolic",
        });

        match state.track() {
            Some(track) => {
                self.title.set_text(&track.title);
                set_optional(&self.artist, Some(track.artist_line()).filter(|s| !s.is_empty()));
                set_optional(&self.album, track.album.clone());
                self.set_art(track.art_path.as_deref());

                let length = track.length_ms.unwrap_or(0);
                self.length_ms.set(length);
                self.duration.set_visible(length > 0);
                self.duration.set_text(&format_time(length));
                self.scrubber.set_sensitive(length > 0 && state.caps.can_seek);
            }
            None => {
                self.title.set_text("Nothing playing");
                self.artist.set_visible(false);
                self.album.set_visible(false);
                self.set_art(None);
                self.length_ms.set(0);
                self.duration.set_visible(false);
                self.scrubber.set_sensitive(false);
            }
        }

        if let Some(player) = &state.player {
            // A drag in progress is the user's intent, and the daemon's position
            // is a moment behind it. Leave both the thumb and the label alone.
            if !self.dragging.get() {
                let length = self.length_ms.get();
                self.elapsed.set_text(&format_time(player.position_ms));
                self.scrubber.set_value(if length > 0 {
                    (player.position_ms as f64 / length as f64).clamp(0.0, 1.0)
                } else {
                    0.0
                });
            }
            self.elapsed.set_visible(true);
            self.shuffle.set_active(player.shuffle.unwrap_or(false));
            self.shuffle.set_visible(player.shuffle.is_some());
            self.repeat.set_visible(player.repeat.is_some());
        } else {
            self.elapsed.set_visible(false);
        }

        // Hidden entirely without an account rather than shown inert: an
        // unauthorized heart that does nothing is worse than no heart.
        let liked = state.track().and_then(|t| t.liked);
        self.like.set_visible(state.caps.can_like);
        self.like.set_icon_name(if liked == Some(true) {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });

        self.render_audio(state);
        self.render_queue(state);

        self.binding.set(false);
    }

    /// Volume, mute and the output picker.
    ///
    /// The whole row hides when there is nothing to control. A slider that does
    /// nothing is worse than no slider, and that is the normal case for a player
    /// with no local stream.
    fn render_audio(&self, state: &State) {
        use waytify_ipc::VolumeRoute;

        let has_outputs = !state.audio.sinks.is_empty() || !state.spotify.devices.is_empty();
        let available = state.audio.route != VolumeRoute::Unavailable;
        self.volume_row.set_visible(available || has_outputs);
        self.volume.set_visible(available);
        self.mute.set_visible(available);
        if !available && !has_outputs {
            return;
        }

        if let Some(percent) = state.audio.volume {
            self.volume.set_value(f64::from(percent));
            let muted = state.audio.muted.unwrap_or(false);
            self.mute.set_icon_name(volume_icon(percent, muted));
            if muted {
                self.mute.add_css_class("muted");
            } else {
                self.mute.remove_css_class("muted");
            }
        }

        // Only rebuild when the set of outputs changed. State arrives once a
        // second while playing and rebuilding a list that often would close the
        // popover under the pointer every time.
        let outputs_now = (state.audio.sinks.clone(), state.spotify.devices.clone());
        if *self.listed_sinks.borrow() != outputs_now {
            self.rebuild_outputs(state);
            *self.listed_sinks.borrow_mut() = outputs_now;
        }

        // Nothing to choose between with a single local output and no remote
        // devices to move to.
        let choices = state.audio.sinks.len() + state.spotify.devices.len();
        self.output.set_visible(choices > 1);
    }

    fn render_queue(&self, state: &State) {
        let upcoming = &state.spotify.queue[..state.spotify.queue.len().min(QUEUE_ROWS)];
        self.queue.set_visible(!upcoming.is_empty());

        if self.listed_queue.borrow().as_slice() == upcoming {
            return;
        }

        while let Some(child) = self.queue_list.first_child() {
            self.queue_list.remove(&child);
        }

        for track in upcoming {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            row.add_css_class("queue-track");

            let title = label("queue-title");
            title.set_text(&track.title);
            title.set_hexpand(true);
            row.append(&title);

            let artists = track.artist_line();
            if !artists.is_empty() {
                let artist = label("queue-artist");
                artist.set_text(&artists);
                row.append(&artist);
            }

            self.queue_list.append(&row);
        }
        *self.listed_queue.borrow_mut() = upcoming.to_vec();
    }

    fn rebuild_outputs(&self, state: &State) {
        while let Some(child) = self.output_list.first_child() {
            self.output_list.remove(&child);
        }

        for device in &state.spotify.devices {
            let row = gtk4::Button::with_label(&format!("{} ({})", device.name, device.kind));
            row.add_css_class("device");
            row.add_css_class("remote");
            row.add_css_class("flat");
            if Some(&device.id) == state.spotify.active_device.as_ref() {
                row.add_css_class("active");
            }
            // Transferring is a write to /me/player, so it needs Premium. A row
            // that cannot work is shown disabled rather than hidden, because its
            // absence would look like the device is not there at all.
            row.set_sensitive(state.caps.can_transfer);

            let client = Rc::clone(&self.client);
            let id = device.id.clone();
            let popover = self.output.popover();
            row.connect_clicked(move |_| {
                client.send(Command::TransferTo { device_id: id.clone() });
                if let Some(popover) = &popover {
                    popover.popdown();
                }
            });
            self.output_list.append(&row);
        }

        for sink in &state.audio.sinks {
            let row = gtk4::Button::with_label(&sink.description);
            row.add_css_class("device");
            row.add_css_class("flat");
            if Some(&sink.name) == state.audio.active_sink.as_ref() {
                row.add_css_class("active");
            }

            let client = Rc::clone(&self.client);
            let name = sink.name.clone();
            let popover = self.output.popover();
            row.connect_clicked(move |_| {
                client.send(Command::SetSink { sink_name: name.clone() });
                if let Some(popover) = &popover {
                    popover.popdown();
                }
            });
            self.output_list.append(&row);
        }
    }

    fn set_art(&self, path: Option<&std::path::Path>) {
        match path {
            Some(p) if p.exists() => {
                self.art.set_from_file(Some(p));
                self.art.set_visible(true);
                self.art_placeholder.set_visible(false);
            }
            _ => {
                self.art.set_visible(false);
                self.art_placeholder.set_visible(true);
            }
        }
    }
}

fn label(class: &str) -> gtk4::Label {
    let l = gtk4::Label::new(None);
    l.add_css_class(class);
    l.set_xalign(0.0);
    // Long titles are common. Ellipsise rather than letting the window grow to
    // whatever width a track name happens to need.
    l.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    l.set_max_width_chars(24);
    l
}

fn button(icon: &str, class: &str) -> gtk4::Button {
    let b = gtk4::Button::from_icon_name(icon);
    b.add_css_class(class);
    b.add_css_class("flat");
    b
}

fn toggle(icon: &str, class: &str) -> gtk4::ToggleButton {
    let b = gtk4::ToggleButton::new();
    b.set_icon_name(icon);
    b.add_css_class(class);
    b.add_css_class("flat");
    b
}

/// Speaker icon matching the level, the way every other volume control does.
fn volume_icon(percent: u8, muted: bool) -> &'static str {
    match percent {
        _ if muted || percent == 0 => "audio-volume-muted-symbolic",
        1..=33 => "audio-volume-low-symbolic",
        34..=66 => "audio-volume-medium-symbolic",
        _ => "audio-volume-high-symbolic",
    }
}

fn set_optional(label: &gtk4::Label, text: Option<String>) {
    match text {
        Some(t) if !t.is_empty() => {
            label.set_text(&t);
            label.set_visible(true);
        }
        _ => label.set_visible(false),
    }
}

/// Same shape the bar uses, so the two never disagree about how a time looks.
fn format_time(ms: u64) -> String {
    let total = ms / 1_000;
    let (h, m, s) = (total / 3_600, (total % 3_600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Drain updates from the daemon into the widgets, on the GTK thread.
pub fn drive(ui: Rc<Ui>, popup: Rc<crate::window::Popup>, client: Rc<Client>) {
    glib::spawn_future_local(async move {
        while let Ok(update) = client.updates.recv().await {
            match update {
                crate::client::Update::State(state) => {
                    popup.set_offline(false);
                    ui.render(&state);
                }
                crate::client::Update::Popup(action) => popup.apply(action),
                crate::client::Update::Disconnected => {
                    // Keep the last state on screen. A window that empties itself
                    // because a background service restarted is worse than one
                    // showing something a second out of date.
                    popup.set_offline(true);
                }
                crate::client::Update::Incompatible(message) => {
                    tracing::error!("{message}");
                    ui.title.set_text("Version mismatch");
                    ui.artist.set_text("Restart the waytify daemon");
                    ui.artist.set_visible(true);
                }
            }
        }
    });
}
