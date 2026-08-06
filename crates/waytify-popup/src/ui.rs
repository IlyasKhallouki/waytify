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

        header.append(&art);
        header.append(&art_placeholder);
        header.append(&meta);

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

        root.append(&header);
        root.append(&scrub_row);
        root.append(&transport);

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

        // Take the seek on release rather than continuously. Sending on every
        // motion event would put a D-Bus call behind every pixel of the drag.
        let drag = gtk4::GestureClick::new();
        {
            let dragging = Rc::clone(&self.dragging);
            drag.connect_pressed(move |_, _, _, _| dragging.set(true));
        }
        {
            let dragging = Rc::clone(&self.dragging);
            let length = Rc::clone(&self.length_ms);
            let client = Rc::clone(client);
            let scrubber = self.scrubber.clone();
            drag.connect_released(move |_, _, _, _| {
                dragging.set(false);
                let len = length.get();
                if len == 0 {
                    return;
                }
                let position_ms = (scrubber.value().clamp(0.0, 1.0) * len as f64) as u64;
                client.send(Command::Seek { position_ms });
            });
        }
        self.scrubber.add_controller(drag);

        // While dragging, keep the elapsed label under the thumb so the drag
        // reads as a scrub rather than a slider with a lagging number.
        {
            let dragging = Rc::clone(&self.dragging);
            let length = Rc::clone(&self.length_ms);
            let elapsed = self.elapsed.clone();
            self.scrubber.connect_value_changed(move |scale| {
                if !dragging.get() {
                    return;
                }
                let at = (scale.value().clamp(0.0, 1.0) * length.get() as f64) as u64;
                elapsed.set_text(&format_time(at));
            });
        }
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

        self.binding.set(false);
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
pub fn drive(ui: Rc<Ui>, window: gtk4::ApplicationWindow, client: Rc<Client>) {
    glib::spawn_future_local(async move {
        while let Ok(update) = client.updates.recv().await {
            match update {
                crate::client::Update::State(state) => ui.render(&state),
                crate::client::Update::Popup(action) => crate::window::apply(&window, action),
                crate::client::Update::Disconnected => {
                    // Keep the last state on screen. A window that empties itself
                    // because a background service restarted is worse than one
                    // showing something a second out of date.
                    ui.root.add_css_class("offline");
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
