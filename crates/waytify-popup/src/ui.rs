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

/// How long the lyric lines take to fade out before the next one is written in.
///
/// The same again on the way back, so a line change costs twice this. Short
/// enough to stay ahead of the singing, long enough to read as a change of line
/// rather than a flicker.
const LYRIC_FADE: std::time::Duration = std::time::Duration::from_millis(130);

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
    /// Whether the icon theme has the one-track repeat glyph, asked once rather
    /// than on every frame.
    has_repeat_one: bool,
    like: gtk4::Button,
    volume_row: gtk4::Box,
    volume: gtk4::Scale,
    mute: gtk4::Button,
    output: gtk4::MenuButton,
    output_list: gtk4::Box,
    lyrics: gtk4::Box,
    /// The line before, the line being sung, and the line after. Three fixed
    /// labels rather than one per lyric: the view is a window onto the song, so
    /// its size does not depend on the length of it.
    lyric_lines: [gtk4::Label; 3],
    /// The lyrics as last built, so the rows are rebuilt when the track changes
    /// rather than once a second.
    listed_lyrics: std::cell::RefCell<Option<waytify_ipc::Lyrics>>,
    /// Which line is highlighted, so a frame that does not move to a new line
    /// costs nothing.
    current_line: Cell<Option<usize>>,
    /// The fade that is part way through, so a run of quick changes does not
    /// leave several of them fighting over the same three labels.
    lyric_fade: Rc<std::cell::RefCell<Option<glib::SourceId>>>,
    /// Where playback was when the last frame arrived, and when that was.
    ///
    /// The bar cannot do this because it receives text the daemon has already
    /// rendered. The window receives a position and a status, so between frames
    /// it can work out where playback has got to rather than waiting to be told.
    anchor: Cell<(u64, std::time::Instant)>,
    playing: Cell<bool>,
    queue: gtk4::Box,
    queue_toggle: gtk4::ToggleButton,
    queue_list: gtk4::Box,
    /// The queue as last rendered, so rows are rebuilt when it moves rather than
    /// on every state frame.
    listed_queue: std::cell::RefCell<Vec<waytify_ipc::Track>>,
    /// Devices currently listed in the picker, so it is only rebuilt when the
    /// set changes rather than on every state frame.
    listed_devices: std::cell::RefCell<Vec<waytify_ipc::Device>>,
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
        let has_repeat_one = gtk4::IconTheme::for_display(
            &gtk4::gdk::Display::default().expect("a display, since the widgets are being built"),
        )
        .has_icon("media-playlist-repeat-song-symbolic");

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

        // Ask the moment it opens. The list is polled while the window is up,
        // but somebody who has just woken a speaker should not have to wait out
        // a poll interval to see it.
        {
            let client = Rc::clone(&client);
            outputs.connect_show(move |_| client.send(Command::RefreshDevices));
        }
        output.set_popover(Some(&outputs));

        volume_row.append(&mute);
        volume_row.append(&volume);
        volume_row.append(&output);

        let lyrics = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        lyrics.add_css_class("waytify-lyrics");
        let lyric_lines = [lyric_label(), lyric_label(), lyric_label()];
        lyric_lines[1].add_css_class("current");
        for line in &lyric_lines {
            lyrics.append(line);
        }

        // Read only. Spotify has no endpoint for jumping to an arbitrary queue
        // position, so these rows are labels rather than buttons: a row that
        // looked clickable and did nothing would be worse than one that does not.
        //
        // Closed to start with. It is the answer to a question you have to ask,
        // unlike the track and the transport, and a window that opens tall
        // enough to list five songs you did not want to see is worse than one
        // click.
        //
        // A toggle and a revealer rather than a GtkExpander. The expander
        // animates its own arrow and child on a timing that is not ours, which
        // reads as a lurch against a window that is resizing at the same time.
        // This way the only motion is a fade while the window grows.
        let queue = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        queue.add_css_class("waytify-queue");

        let queue_toggle = gtk4::ToggleButton::new();
        queue_toggle.add_css_class("queue-heading");
        queue_toggle.add_css_class("flat");
        let heading = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let heading_label = label("queue-heading-label");
        heading_label.set_text("Up next");
        heading_label.set_hexpand(true);
        let chevron = gtk4::Image::from_icon_name("pan-end-symbolic");
        chevron.add_css_class("queue-chevron");
        heading.append(&heading_label);
        heading.append(&chevron);
        queue_toggle.set_child(Some(&heading));

        let queue_list = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
        let queue_reveal = gtk4::Revealer::new();
        queue_reveal.set_transition_type(gtk4::RevealerTransitionType::Crossfade);
        queue_reveal.set_transition_duration(160);
        queue_reveal.set_child(Some(&queue_list));

        {
            let reveal = queue_reveal.clone();
            let chevron = chevron.clone();
            queue_toggle.connect_toggled(move |t| {
                reveal.set_reveal_child(t.is_active());
                chevron.set_icon_name(Some(if t.is_active() {
                    "pan-down-symbolic"
                } else {
                    "pan-end-symbolic"
                }));
            });
        }

        queue.append(&queue_toggle);
        queue.append(&queue_reveal);

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
            has_repeat_one,
            like,
            volume_row,
            volume,
            mute,
            output,
            output_list,
            lyrics,
            lyric_lines,
            listed_lyrics: std::cell::RefCell::new(None),
            current_line: Cell::new(None),
            lyric_fade: Rc::new(std::cell::RefCell::new(None)),
            anchor: Cell::new((0, std::time::Instant::now())),
            playing: Cell::new(false),
            queue,
            queue_toggle,
            queue_list,
            listed_queue: std::cell::RefCell::new(Vec::new()),
            listed_devices: std::cell::RefCell::new(Vec::new()),
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
            self.render_repeat(player.repeat);
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

        // The picker is about which device plays, so it belongs to the account
        // rather than to whether there is a local stream to control.
        let can_pick = state.spotify.authorized;
        let available = state.audio.route != VolumeRoute::Unavailable;
        self.volume_row.set_visible(available || can_pick);
        self.volume.set_visible(available);
        self.mute.set_visible(available);
        // Shown even with one device. It is how you find out there is only one,
        // and how you refresh after waking another.
        self.output.set_visible(can_pick);
        if !available && !can_pick {
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

        // Only rebuild when the set changed. State arrives once a second while
        // playing, and rebuilding this that often would close the popover under
        // the pointer every time.
        if *self.listed_devices.borrow() != state.spotify.devices {
            self.rebuild_outputs(state);
            *self.listed_devices.borrow_mut() = state.spotify.devices.clone();
        }
    }

    /// Show which of the three repeat states is on.
    ///
    /// The real client cycles off, then the whole context, then the one track,
    /// and shows the last of those with a mark on the icon. Cycling without the
    /// icon changing is worse than having no button: the state does change, and
    /// nothing says so.
    fn render_repeat(&self, repeat: Option<waytify_ipc::Repeat>) {
        use waytify_ipc::Repeat;

        self.repeat.set_visible(repeat.is_some());
        let repeat = repeat.unwrap_or_default();

        // Not every icon theme carries the one-track variant. Adwaita does not.
        // Falling back to the plain icon keeps the colour and the class, so the
        // state is still legible where the glyph is missing.
        let one = "media-playlist-repeat-song-symbolic";
        let icon = match repeat {
            Repeat::Track if self.has_repeat_one => one,
            _ => "media-playlist-repeat-symbolic",
        };
        self.repeat.set_icon_name(icon);

        for class in ["off", "all", "one"] {
            self.repeat.remove_css_class(class);
        }
        self.repeat.add_css_class(match repeat {
            Repeat::Off => "off",
            Repeat::Playlist => "all",
            Repeat::Track => "one",
        });
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
        let Some(lyrics) = lyrics else { return };

        if self.listed_lyrics.borrow().as_ref() == Some(lyrics) {
            self.highlight_line(position_ms);
            return;
        }

        // A new song. The line at this position may be at the same index as the
        // one already shown, so the text is written out rather than left to the
        // guard in highlight_line.
        *self.listed_lyrics.borrow_mut() = Some(lyrics.clone());
        let current = lyrics.line_at(position_ms);
        self.current_line.set(current);
        self.show_lines(lyrics, current);
    }

    /// Move the view to whichever line is being sung.
    fn highlight_line(&self, position_ms: u64) {
        let listed = self.listed_lyrics.borrow();
        let Some(lyrics) = listed.as_ref() else { return };

        let current = lyrics.line_at(position_ms);
        if current == self.current_line.get() {
            return;
        }
        self.current_line.set(current);
        self.show_lines(lyrics, current);
    }

    /// Write the line being sung and its neighbours into the three labels.
    ///
    /// `current` is `None` through the intro, before the first line has been
    /// reached. The line that is coming still shows, one slot down, which is
    /// what makes the wait look intentional rather than broken.
    fn show_lines(&self, lyrics: &waytify_ipc::Lyrics, current: Option<usize>) {
        let at = |index: Option<usize>| {
            index.and_then(|i| lyrics.lines.get(i)).map(|l| l.text.clone()).unwrap_or_default()
        };

        let (previous, singing, next) = match current {
            Some(i) => (i.checked_sub(1), Some(i), Some(i + 1)),
            None => (None, None, Some(0)),
        };
        let texts = [at(previous), at(singing), at(next)];

        // Fade the three labels down, swap the words while they cannot be read,
        // and let the stylesheet bring them back. Swapping text under a reader's
        // eye is the one thing that makes synced lyrics feel mechanical.
        //
        // A fade already running is cancelled rather than left to finish: its
        // callback would clear the class part way through this one and show a
        // flash of the old line at full brightness.
        if let Some(pending) = self.lyric_fade.borrow_mut().take() {
            pending.remove();
        }
        self.lyrics.add_css_class("stepping");

        let container = self.lyrics.clone();
        let labels = self.lyric_lines.clone();
        let slot = Rc::clone(&self.lyric_fade);
        let id = glib::timeout_add_local_once(LYRIC_FADE, move || {
            for (label, text) in labels.iter().zip(&texts) {
                label.set_text(text);
            }
            container.remove_css_class("stepping");
            // The source has run, so the handle left behind is stale and must
            // not be removed later by the next change.
            slot.borrow_mut().take();
        });
        *self.lyric_fade.borrow_mut() = Some(id);
    }

    fn render_queue(&self, state: &State) {
        let upcoming = &state.spotify.queue[..state.spotify.queue.len().min(QUEUE_ROWS)];
        self.queue.set_visible(!upcoming.is_empty());

        // A section that has gone away has no open state worth keeping. Coming
        // back already open, showing songs nobody asked to see, contradicts it
        // being closed to start with.
        if upcoming.is_empty() {
            self.queue_toggle.set_active(false);
        }

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

    /// Rebuild the Connect device list.
    ///
    /// Only Spotify devices. Which machine plays and which speaker on this
    /// machine it comes out of are different questions, and listing local sinks
    /// beside remote devices implied you were choosing between them when in fact
    /// one contains the other.
    fn rebuild_outputs(&self, state: &State) {
        while let Some(child) = self.output_list.first_child() {
            self.output_list.remove(&child);
        }

        let heading = label("outputs-heading");
        heading.set_text("Playing on");
        self.output_list.append(&heading);

        for device in &state.spotify.devices {
            let row = gtk4::Button::new();
            row.add_css_class("device");
            row.add_css_class("flat");
            let active = Some(&device.id) == state.spotify.active_device.as_ref();
            if active {
                row.add_css_class("active");
            }

            let content = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
            let name = label("device-name");
            name.set_text(&device.name);
            name.set_hexpand(true);
            let kind = label("device-kind");
            kind.set_text(&device.kind);
            content.append(&name);
            content.append(&kind);
            row.set_child(Some(&content));

            // Transferring is a write to /me/player, so it needs Premium. A row
            // that cannot work is shown disabled rather than hidden, because its
            // absence would look like the device is not there at all.
            row.set_sensitive(state.caps.can_transfer && !active);

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

        // Spotify only reports devices with a live session, so a phone with the
        // app closed is missing here while being visible in the app itself,
        // which does its own discovery. Saying so is the difference between a
        // limitation and a bug.
        let hint = label("outputs-hint");
        hint.set_text("Not seeing a device? Open Spotify on it first.");
        hint.set_wrap(true);
        hint.set_max_width_chars(28);
        self.output_list.append(&hint);

        let refresh = gtk4::Button::with_label("Refresh");
        refresh.add_css_class("outputs-refresh");
        refresh.add_css_class("flat");
        let client = Rc::clone(&self.client);
        refresh.connect_clicked(move |_| client.send(Command::RefreshDevices));
        self.output_list.append(&refresh);
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

/// One slot in the three line lyric view.
///
/// A fixed height whatever it holds, including nothing: the labels are written
/// to on every line change, and a window that grew and shrank by a line as the
/// song moved would be unusable.
fn lyric_label() -> gtk4::Label {
    let l = label("lyric-line");
    l.set_xalign(0.5);
    l.set_wrap(false);
    l.set_max_width_chars(34);
    // Keeps the row occupying its space through instrumental breaks, which are
    // timed blank lines rather than gaps in the data.
    l.set_height_request(22);
    l
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
        assert!(!ui.queue_toggle.is_active(), "closed until asked for");

        // Opening is ours to animate, so the rows have to actually be revealed
        // by the toggle rather than by a widget doing it for us.
        ui.queue_toggle.set_active(true);
        assert!(ui.queue_list.parent().and_downcast::<gtk4::Revealer>().unwrap().reveals_child());
        ui.queue_toggle.set_active(false);
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

        ui.queue_toggle.set_active(true);
        state.spotify.queue.clear();
        ui.render(&state);
        assert!(!ui.queue.is_visible(), "the section goes away with its contents");
        assert!(!ui.queue_toggle.is_active(), "and comes back closed, as it started");

        check_stylesheets();
        check_repeat(&ui);
        check_art_colors_cross_providers(&ui);
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
        });
        state.player.as_mut().unwrap().position_ms = 10_000;
        ui.render(state);
        assert_eq!(shown(ui)[1], "First");

        // The line moves on the window's own clock, with no new frame from the
        // daemon in between.
        ui.highlight_line(20_500);
        assert_eq!(shown(ui)[1], "Second");
    }

    /// GTK reports CSS problems through a signal and then carries on with
    /// whatever it managed to parse, so a broken stylesheet shows up as a window
    /// that looks subtly wrong rather than as an error. This turns that into a
    /// failure.
    ///
    /// Set `WAYTIFY_CSS_CHECK` to a path to check your own stylesheet too:
    ///
    /// ```sh
    /// WAYTIFY_CSS_CHECK=~/.config/waytify/style.css cargo test -p waytify-popup
    /// ```
    fn check_stylesheets() {
        let mut sheets = vec![("default.css".to_string(), crate::style::default_css().to_string())];
        if let Ok(path) = std::env::var("WAYTIFY_CSS_CHECK") {
            let css = std::fs::read_to_string(&path).expect("reading WAYTIFY_CSS_CHECK");
            sheets.push((path, css));
        }

        for (name, css) in sheets {
            let provider = gtk4::CssProvider::new();
            let problems = Rc::new(std::cell::RefCell::new(Vec::new()));
            let seen = Rc::clone(&problems);
            provider.connect_parsing_error(move |_, section, error| {
                seen.borrow_mut().push(format!("{}: {error}", section.to_str()));
            });
            provider.load_from_string(&css);
            let problems = problems.borrow();
            assert!(problems.is_empty(), "{name} does not parse:\n  {}", problems.join("\n  "));
        }
    }

    /// The whole art-colour feature rests on one assumption about GTK: that a
    /// colour defined by one provider can be used by a rule in another. waytify
    /// publishes `@art_vibrant` from its own provider so a user stylesheet can
    /// refer to it, and the two are necessarily different providers because one
    /// is rewritten on every track and the other is a file on disk.
    ///
    /// Nothing else would notice if that stopped working. A named colour that
    /// cannot be resolved is not a parse error, so the rule would quietly fall
    /// back and every art-aware theme would go flat.
    fn check_art_colors_cross_providers(ui: &Ui) {
        let display = gtk4::prelude::WidgetExt::display(&ui.root);

        let defines = gtk4::CssProvider::new();
        defines.load_from_string("@define-color probe_colour rgb(255,0,0);");
        gtk4::style_context_add_provider_for_display(
            &display,
            &defines,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let uses = gtk4::CssProvider::new();
        uses.load_from_string("#waytify-popup .track-artist { color: @probe_colour; }");
        gtk4::style_context_add_provider_for_display(
            &display,
            &uses,
            gtk4::STYLE_PROVIDER_PRIORITY_USER,
        );

        let colour = gtk4::prelude::WidgetExt::color(&ui.artist);
        assert_eq!(
            (colour.red(), colour.green(), colour.blue()),
            (1.0, 0.0, 0.0),
            "a colour defined in one provider must reach a rule in another"
        );

        gtk4::style_context_remove_provider_for_display(&display, &uses);
        gtk4::style_context_remove_provider_for_display(&display, &defines);
    }

    /// Repeat has three states and the button has to show which one it is in.
    ///
    /// Reported as "it doesn't change also it does change it", which is exactly
    /// what a button that cycles a real setting while looking identical feels
    /// like from the outside.
    fn check_repeat(ui: &Ui) {
        use waytify_ipc::Repeat;

        let class = |ui: &Ui| {
            ["off", "all", "one"]
                .into_iter()
                .find(|c| ui.repeat.has_css_class(c))
                .map(str::to_string)
        };

        ui.render_repeat(Some(Repeat::Off));
        assert!(ui.repeat.is_visible());
        assert_eq!(class(ui).as_deref(), Some("off"));

        ui.render_repeat(Some(Repeat::Playlist));
        assert_eq!(class(ui).as_deref(), Some("all"), "the whole context");

        ui.render_repeat(Some(Repeat::Track));
        assert_eq!(class(ui).as_deref(), Some("one"), "the one track");

        // Exactly one state at a time, or a stylesheet sees two and the last
        // rule in the file wins rather than the state the player is in.
        let held = ["off", "all", "one"].into_iter().filter(|c| ui.repeat.has_css_class(c)).count();
        assert_eq!(held, 1);

        // Cycling in the order the real client uses, so muscle memory carries.
        assert_eq!(Repeat::Off.next(), Repeat::Playlist);
        assert_eq!(Repeat::Playlist.next(), Repeat::Track);
        assert_eq!(Repeat::Track.next(), Repeat::Off);

        // A player that does not report repeat at all gets no button, rather
        // than one that lies about being off.
        ui.render_repeat(None);
        assert!(!ui.repeat.is_visible());
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
        });

        position(state, 0);
        ui.render(state);
        assert!(ui.lyrics.is_visible());
        // Through the intro there is no line being sung, but the one that is
        // coming shows, so the wait looks intentional.
        assert_eq!(shown(ui), ["", "", "First"]);

        position(state, 15_000);
        ui.render(state);
        assert_eq!(shown(ui), ["", "First", "Second"], "nothing came before the first");

        position(state, 25_000);
        ui.render(state);
        assert_eq!(shown(ui), ["First", "Second", "Third"]);

        position(state, 35_000);
        ui.render(state);
        assert_eq!(shown(ui), ["Second", "Third", ""], "nothing comes after the last");

        // A different song whose current line lands on the same index has to
        // redraw anyway, which the highlight guard on its own would skip.
        state.lyrics = Some(Lyrics {
            lines: vec![
                LyricLine { at_ms: 10_000, text: "Other one".into() },
                LyricLine { at_ms: 20_000, text: "Other two".into() },
            ],
        });
        position(state, 35_000);
        ui.render(state);
        assert_eq!(shown(ui), ["Other one", "Other two", ""]);

        state.lyrics = None;
        ui.render(state);
        assert!(!ui.lyrics.is_visible());
    }

    /// What the three lyric slots read, top to bottom, once the fade has run.
    ///
    /// The words are written part way through a fade rather than immediately,
    /// so reading the labels straight after a render sees the previous line.
    /// Pumping the loop is what a running window does between frames.
    fn shown(ui: &Ui) -> [String; 3] {
        let context = glib::MainContext::default();
        let deadline = std::time::Instant::now() + LYRIC_FADE * 3;
        while std::time::Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        std::array::from_fn(|i| ui.lyric_lines[i].text().to_string())
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
