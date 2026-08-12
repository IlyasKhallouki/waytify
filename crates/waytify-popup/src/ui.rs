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

/// How often the window carries the position forward on its own.
///
/// The daemon publishes once a second while playing. That is often enough for a
/// scrubber and visibly late for a lyric line, which is the one place a second
/// of lag reads as broken rather than smooth.
const TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// Height of the lyrics pane, in pixels.
///
/// Fixed rather than sized to its contents, so the window is the same height on
/// a track with four hundred lines as on one with none.
const LYRICS_HEIGHT: i32 = 132;

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
    lyrics: gtk4::ScrolledWindow,
    lyrics_list: gtk4::Box,
    /// One label per line, so the current one can be highlighted and scrolled to
    /// without walking the widget tree on every frame.
    lyric_lines: std::cell::RefCell<Vec<gtk4::Label>>,
    /// The lyrics as last built, so the rows are rebuilt when the track changes
    /// rather than once a second.
    listed_lyrics: std::cell::RefCell<Option<waytify_ipc::Lyrics>>,
    /// Which line is highlighted, so a frame that does not move to a new line
    /// costs nothing.
    current_line: Cell<Option<usize>>,
    /// Where playback was when the last frame arrived, and when that was.
    ///
    /// The bar cannot do this because it receives text the daemon has already
    /// rendered. The window receives a position and a status, so between frames
    /// it can work out where playback has got to rather than waiting to be told.
    anchor: Cell<(u64, std::time::Instant)>,
    playing: Cell<bool>,
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

        // One pane for both kinds. Timed lyrics scroll and highlight; lyrics
        // with no timing are the same list without either, rather than a second
        // widget that exists to show the same text.
        let lyrics = gtk4::ScrolledWindow::new();
        lyrics.add_css_class("waytify-lyrics");
        lyrics.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        lyrics.set_min_content_height(LYRICS_HEIGHT);
        lyrics.set_max_content_height(LYRICS_HEIGHT);
        let lyrics_list = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        lyrics.set_child(Some(&lyrics_list));

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
        root.append(&lyrics);
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
            lyrics,
            lyrics_list,
            lyric_lines: std::cell::RefCell::new(Vec::new()),
            listed_lyrics: std::cell::RefCell::new(None),
            current_line: Cell::new(None),
            anchor: Cell::new((0, std::time::Instant::now())),
            playing: Cell::new(false),
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
            self.anchor.set((player.position_ms, std::time::Instant::now()));
            self.playing.set(status == Status::Playing);
            // A drag in progress is the user's intent, and the daemon's position
            // is a moment behind it. Leave both the thumb and the label alone.
            if !self.dragging.get() {
                self.show_position(player.position_ms);
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
        self.render_lyrics(state, self.position());
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

    /// Where playback has got to, counting from the last frame.
    ///
    /// Only while playing: a paused track stays exactly where the daemon left
    /// it, and advancing it would be inventing playback that is not happening.
    fn position(&self) -> u64 {
        let (anchor, at) = self.anchor.get();
        if !self.playing.get() {
            return anchor;
        }
        anchor.saturating_add(at.elapsed().as_millis() as u64)
    }

    /// Move the elapsed time and the thumb to a position.
    fn show_position(&self, position_ms: u64) {
        let length = self.length_ms.get();
        self.elapsed.set_text(&format_time(position_ms));
        self.scrubber.set_value(if length > 0 {
            (position_ms as f64 / length as f64).clamp(0.0, 1.0)
        } else {
            0.0
        });
    }

    /// Carry the window forward between frames.
    ///
    /// Does nothing unless it is on screen and playing, so a window sitting
    /// hidden in the background costs one comparison every tick.
    pub fn tick(&self) {
        if !self.playing.get() || !self.root.is_mapped() || self.dragging.get() {
            return;
        }
        let position = self.position();

        self.binding.set(true);
        self.show_position(position);
        self.binding.set(false);

        self.highlight_line(position);
    }

    fn render_lyrics(&self, state: &State, position_ms: u64) {
        let lyrics = state.lyrics.as_ref();
        self.lyrics.set_visible(lyrics.is_some());
        let Some(lyrics) = lyrics else {
            return;
        };

        if self.listed_lyrics.borrow().as_ref() != Some(lyrics) {
            self.rebuild_lyrics(lyrics);
            *self.listed_lyrics.borrow_mut() = Some(lyrics.clone());
            self.current_line.set(None);
        }

        self.highlight_line(position_ms);
    }

    /// Move the highlight to whichever line is being sung.
    fn highlight_line(&self, position_ms: u64) {
        let listed = self.listed_lyrics.borrow();
        let Some(lyrics) = listed.as_ref().filter(|l| l.is_synced()) else { return };

        let current = lyrics.line_at(position_ms);
        if current == self.current_line.get() {
            return;
        }

        let rows = self.lyric_lines.borrow();
        if let Some(previous) = self.current_line.get().and_then(|i| rows.get(i)) {
            previous.remove_css_class("current");
        }
        if let Some(row) = current.and_then(|i| rows.get(i)) {
            row.add_css_class("current");
            scroll_into_view(&self.lyrics, row);
        }
        self.current_line.set(current);
    }

    fn rebuild_lyrics(&self, lyrics: &waytify_ipc::Lyrics) {
        while let Some(child) = self.lyrics_list.first_child() {
            self.lyrics_list.remove(&child);
        }

        let texts: Vec<&str> = if lyrics.is_synced() {
            lyrics.lines.iter().map(|l| l.text.as_str()).collect()
        } else {
            lyrics.plain.as_deref().unwrap_or_default().lines().collect()
        };

        let mut rows = Vec::with_capacity(texts.len());
        for text in texts {
            let row = label("lyric-line");
            row.set_text(text);
            // Lyrics are prose, not metadata: wrapping keeps a long line
            // readable where the ellipsis every other label uses would eat it.
            row.set_ellipsize(gtk4::pango::EllipsizeMode::None);
            row.set_wrap(true);
            row.set_max_width_chars(34);
            self.lyrics_list.append(&row);
            rows.push(row);
        }
        *self.lyric_lines.borrow_mut() = rows;
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

/// Bring a line to the middle of the pane.
///
/// Deferred to an idle callback because a row built this frame has no size yet,
/// and scrolling to a widget whose height is still zero puts the view at the top
/// every time.
fn scroll_into_view(pane: &gtk4::ScrolledWindow, row: &gtk4::Label) {
    let pane = pane.clone();
    let row = row.clone();
    glib::idle_add_local_once(move || {
        let adjustment = pane.vadjustment();
        let height = f64::from(row.height());
        let origin = gtk4::graphene::Point::new(0.0, 0.0);
        let Some(point) = row.compute_point(&pane, &origin) else { return };

        // The point is relative to the visible area, so the scroll position the
        // pane is already at has to be added back to get the offset within the
        // list.
        let top = f64::from(point.y());
        let centre = adjustment.value() + top + height / 2.0 - adjustment.page_size() / 2.0;
        adjustment
            .set_value(centre.clamp(0.0, (adjustment.upper() - adjustment.page_size()).max(0.0)));
    });
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
    // Between frames the window advances the position itself, so the elapsed
    // time and the lyric line do not wait a second to be told.
    let ticking = Rc::clone(&ui);
    glib::timeout_add_local(TICK, move || {
        ticking.tick();
        glib::ControlFlow::Continue
    });

    glib::spawn_future_local(async move {
        while let Ok(update) = client.updates.recv().await {
            match update {
                crate::client::Update::State(state) => {
                    popup.set_offline(false);
                    popup.resync();
                    ui.render(&state);
                }
                crate::client::Update::Popup(action) => popup.apply(action),
                crate::client::Update::Disconnected => {
                    // Keep the last state on screen. A window that empties itself
                    // because a background service restarted is worse than one
                    // showing something a second out of date.
                    popup.set_offline(true);
                    popup.forget();
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

#[cfg(test)]
mod tests {
    use super::*;
    use waytify_ipc::{LyricLine, Lyrics, Player, State, Status, Track};

    /// One test rather than several, on purpose.
    ///
    /// GTK may only be used from the thread that initialised it, and the test
    /// harness gives every test its own thread. A second test calling `init`
    /// would be initialising a second display connection from a second thread,
    /// which aborts the whole binary rather than failing one case.
    ///
    /// Nothing here is ever presented, so no window appears: this exercises the
    /// widget tree, not the compositor. It skips where there is no display, in
    /// the same way the D-Bus tests skip without a session bus.
    #[test]
    fn the_window_renders_state_into_its_widgets() {
        if gtk4::init().is_err() {
            eprintln!("no display; skipping");
            return;
        }

        let ui = Ui::build(Rc::new(crate::client::connect()));
        let track = |title: &str| Track {
            title: title.into(),
            artists: vec!["Someone".into()],
            ..Default::default()
        };

        let mut state = State {
            player: Some(Player {
                bus_name: "org.mpris.MediaPlayer2.test".into(),
                identity: "Test".into(),
                status: Status::Playing,
                track: Some(track("Now")),
                position_ms: 0,
                shuffle: None,
                repeat: None,
            }),
            ..Default::default()
        };

        ui.render(&state);
        assert!(!ui.queue.is_visible(), "an empty queue is not a section to show");

        state.spotify.queue = vec![track("First"), track("Second")];
        ui.render(&state);
        assert!(ui.queue.is_visible());
        assert_eq!(rows(&ui.queue_list), vec!["First", "Second"]);

        // More than fits. The window has to stay a predictable size.
        state.spotify.queue = (0..12).map(|i| track(&format!("Track {i}"))).collect();
        ui.render(&state);
        assert_eq!(rows(&ui.queue_list).len(), QUEUE_ROWS);
        assert_eq!(rows(&ui.queue_list)[0], "Track 0", "the next one comes first");

        // Rendering the same queue again must not stack duplicate rows, which is
        // what a rebuild on every frame would do a second later.
        ui.render(&state);
        assert_eq!(rows(&ui.queue_list).len(), QUEUE_ROWS);

        state.spotify.queue.clear();
        ui.render(&state);
        assert!(!ui.queue.is_visible(), "the section goes away with its contents");

        check_lyrics(&ui, &mut state);
        check_the_position_carries_forward(&ui, &mut state);
    }

    /// The window advances the position itself between frames, which is what
    /// keeps a lyric line from landing up to a second late.
    fn check_the_position_carries_forward(ui: &Ui, state: &mut State) {
        let player = state.player.as_mut().expect("a player");
        player.status = Status::Playing;
        player.position_ms = 60_000;
        ui.render(state);

        std::thread::sleep(std::time::Duration::from_millis(40));
        let advanced = ui.position();
        assert!(advanced >= 60_040, "playing carries forward, got {advanced}");
        assert!(advanced < 61_000, "and only by the time that passed");

        // Paused is a position, not a rate. Advancing it would be inventing
        // playback that is not happening.
        state.player.as_mut().unwrap().status = Status::Paused;
        ui.render(state);
        let held = ui.position();
        std::thread::sleep(std::time::Duration::from_millis(40));
        assert_eq!(ui.position(), held, "a paused track stays where it was left");

        // The highlight moves on the window's own clock, with no new frame from
        // the daemon in between.
        state.lyrics = Some(Lyrics {
            lines: vec![
                LyricLine { at_ms: 10_000, text: "First".into() },
                LyricLine { at_ms: 20_000, text: "Second".into() },
            ],
            plain: None,
        });
        state.player.as_mut().unwrap().position_ms = 10_000;
        ui.render(state);
        assert_eq!(highlighted(ui).as_deref(), Some("First"));

        ui.highlight_line(20_500);
        assert_eq!(highlighted(ui).as_deref(), Some("Second"), "without a state frame");
    }

    fn check_lyrics(ui: &Ui, state: &mut State) {
        let position = |state: &mut State, ms: u64| {
            state.player.as_mut().expect("a player").position_ms = ms;
        };

        ui.render(state);
        assert!(!ui.lyrics.is_visible(), "no lyrics, no pane");

        state.lyrics = Some(Lyrics {
            lines: vec![
                LyricLine { at_ms: 10_000, text: "First".into() },
                LyricLine { at_ms: 20_000, text: "Second".into() },
                LyricLine { at_ms: 30_000, text: "Third".into() },
            ],
            plain: None,
        });

        position(state, 0);
        ui.render(state);
        assert!(ui.lyrics.is_visible());
        assert_eq!(ui.lyric_lines.borrow().len(), 3);
        assert_eq!(highlighted(ui), None, "nothing is highlighted through the intro");

        position(state, 15_000);
        ui.render(state);
        assert_eq!(highlighted(ui).as_deref(), Some("First"), "a line holds until the next");

        position(state, 30_000);
        ui.render(state);
        assert_eq!(highlighted(ui).as_deref(), Some("Third"));
        assert_eq!(
            ui.lyric_lines.borrow().iter().filter(|l| l.has_css_class("current")).count(),
            1,
            "the line it moved off is no longer highlighted"
        );

        // A second frame on the same line must not rebuild the pane, which
        // would drop the highlight and fight the scroll position once a second.
        let before: Vec<_> = ui.lyric_lines.borrow().clone();
        ui.render(state);
        assert!(
            before.iter().zip(ui.lyric_lines.borrow().iter()).all(|(a, b)| a == b),
            "the same lyrics are not rebuilt"
        );
        assert_eq!(highlighted(ui).as_deref(), Some("Third"));

        // Lyrics with no timing are the same list without a highlight, rather
        // than a pane that stays empty because nothing can be current.
        state.lyrics = Some(Lyrics { lines: Vec::new(), plain: Some("One\nTwo".into()) });
        ui.render(state);
        assert!(ui.lyrics.is_visible());
        assert_eq!(ui.lyric_lines.borrow().len(), 2);
        assert_eq!(highlighted(ui), None, "there is no current line without timings");

        state.lyrics = None;
        ui.render(state);
        assert!(!ui.lyrics.is_visible());
    }

    /// The text of the highlighted lyric line, if there is one.
    fn highlighted(ui: &Ui) -> Option<String> {
        ui.lyric_lines
            .borrow()
            .iter()
            .find(|l| l.has_css_class("current"))
            .map(|l| l.text().to_string())
    }

    /// The title of each row, in order.
    fn rows(list: &gtk4::Box) -> Vec<String> {
        let mut titles = Vec::new();
        let mut child = list.first_child();
        while let Some(row) = child {
            let title = row
                .first_child()
                .and_then(|w| w.downcast::<gtk4::Label>().ok())
                .expect("every row leads with its title");
            titles.push(title.text().to_string());
            child = row.next_sibling();
        }
        titles
    }
}
