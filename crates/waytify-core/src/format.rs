//! Rendering the bar label from a template.
//!
//! Templates support two constructs.
//!
//! `{name}` is a placeholder, replaced with a value. An unrecognised name is
//! left in the output untouched, so a typo shows up on the bar instead of
//! silently vanishing.
//!
//! `[ ... ]` is an optional group. It is emitted only if every placeholder
//! inside it resolved to something non-empty. This is what lets one template
//! cover a track with no album, or a podcast with no artist, without leaving a
//! dangling separator behind. Groups nest.
//!
//! Markup in the template is passed through so that `<b>{title}</b>` works, while
//! substituted values are escaped. That ordering matters more than it looks:
//! plenty of real track titles contain an ampersand, and an unescaped one makes
//! Pango drop the entire label.

use crate::config::{BarConfig, Icons};
use std::collections::HashMap;
use waytify_ipc::{BarOutput, State, Status};

/// Values available to a template.
pub type Vars = HashMap<&'static str, String>;

/// Escape a value for Pango markup.
///
/// Matches what `g_markup_escape_text` does, so a value is safe in both element
/// content and attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\'' => out.push_str("&apos;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// `3:45`, or `1:02:03` once past an hour.
pub fn format_time(ms: u64) -> String {
    let total = ms / 1_000;
    let (h, m, s) = (total / 3_600, (total % 3_600) / 60, total % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Expand a template against a set of values.
pub fn render(template: &str, vars: &Vars) -> String {
    let chars: Vec<char> = template.chars().collect();
    let mut p = Parser { chars: &chars, i: 0, vars };
    let (text, _) = p.group(true);
    text
}

struct Parser<'a> {
    chars: &'a [char],
    i: usize,
    vars: &'a Vars,
}

impl Parser<'_> {
    /// Parse until the end of input, or until the `]` closing this group.
    ///
    /// Returns the rendered text and whether every placeholder seen resolved to
    /// a non-empty value. A group containing no placeholders at all counts as
    /// resolved, so brackets around literal text are harmless.
    fn group(&mut self, top_level: bool) -> (String, bool) {
        let mut out = String::new();
        let mut resolved = true;

        while self.i < self.chars.len() {
            match self.chars[self.i] {
                ']' if !top_level => {
                    self.i += 1;
                    return (out, resolved);
                }
                '[' => {
                    self.i += 1;
                    let (inner, inner_ok) = self.group(false);
                    if inner_ok {
                        out.push_str(&inner);
                    }
                }
                '{' => {
                    let (text, ok) = self.placeholder();
                    out.push_str(&text);
                    resolved &= ok;
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        (out, resolved)
    }

    /// Read `{name}` starting at the current `{`.
    fn placeholder(&mut self) -> (String, bool) {
        let start = self.i;
        self.i += 1;
        let name_start = self.i;
        while self.i < self.chars.len() && self.chars[self.i] != '}' {
            self.i += 1;
        }
        if self.i >= self.chars.len() {
            // Unterminated. Emit the rest verbatim so the mistake is visible.
            let text: String = self.chars[start..].iter().collect();
            return (text, true);
        }
        let name: String = self.chars[name_start..self.i].iter().collect();
        self.i += 1;

        match self.vars.get(name.as_str()) {
            Some(v) if !v.is_empty() => (escape(v), true),
            Some(_) => (String::new(), false),
            // Not a placeholder we know. Leave it alone rather than eating it.
            None => (format!("{{{name}}}"), true),
        }
    }
}

/// Build the value set a template can reference.
pub fn vars_from_state(state: &State, icons: &Icons) -> Vars {
    let status = state.status();
    let mut v = Vars::new();

    v.insert("icon", icons.for_status(status).to_string());
    v.insert("status", status.css_class().to_string());

    let (title, artist, album) = match state.track() {
        Some(t) => (t.title.clone(), t.artist_line(), t.album.clone().unwrap_or_default()),
        None => (String::new(), String::new(), String::new()),
    };
    v.insert("title", title);
    v.insert("artist", artist);
    v.insert("album", album);

    v.insert("player", state.player.as_ref().map(|p| p.identity.clone()).unwrap_or_default());

    let position = state.player.as_ref().map(|p| p.position_ms).unwrap_or(0);
    v.insert("position", format_time(position));
    // A live stream has no duration, so leave it empty and let optional groups
    // drop whatever surrounds it.
    v.insert(
        "duration",
        state.track().and_then(|t| t.length_ms).map(format_time).unwrap_or_default(),
    );
    v.insert("percent", state.percentage().to_string());

    v
}

/// Render the complete Waybar payload for a state.
pub fn render_bar(state: &State, cfg: &BarConfig) -> BarOutput {
    let status = state.status();
    let vars = vars_from_state(state, &cfg.icons);

    let text = render(cfg.template(status), &vars);
    let tooltip = render(&cfg.tooltip, &vars);

    BarOutput {
        text,
        alt: status.css_class().to_string(),
        // A tooltip made only of whitespace still renders as an empty box, so
        // treat it as absent.
        tooltip: if tooltip.trim().is_empty() { String::new() } else { tooltip },
        class: state.css_classes(),
        percentage: state.percentage(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use waytify_ipc::{Player, Track};

    fn vars(pairs: &[(&'static str, &str)]) -> Vars {
        pairs.iter().map(|(k, v)| (*k, v.to_string())).collect()
    }

    #[test]
    fn placeholders_are_substituted() {
        let v = vars(&[("title", "Digital Love")]);
        assert_eq!(render("Now: {title}", &v), "Now: Digital Love");
    }

    #[test]
    fn unknown_placeholders_survive_so_typos_are_visible() {
        let v = vars(&[("title", "x")]);
        assert_eq!(render("{titel}", &v), "{titel}");
    }

    #[test]
    fn optional_groups_drop_when_a_value_is_missing() {
        let v = vars(&[("title", "Track"), ("artist", "")]);
        assert_eq!(render("{title}[ · {artist}]", &v), "Track");
    }

    #[test]
    fn optional_groups_render_when_the_value_is_present() {
        let v = vars(&[("title", "Track"), ("artist", "Someone")]);
        assert_eq!(render("{title}[ · {artist}]", &v), "Track · Someone");
    }

    #[test]
    fn groups_nest() {
        let v = vars(&[("a", "A"), ("b", "")]);
        assert_eq!(render("[{a}[ and {b}]]", &v), "A");

        let v = vars(&[("a", "A"), ("b", "B")]);
        assert_eq!(render("[{a}[ and {b}]]", &v), "A and B");
    }

    #[test]
    fn a_group_with_no_placeholders_is_literal_text() {
        assert_eq!(render("[just text]", &vars(&[])), "just text");
    }

    #[test]
    fn an_unterminated_placeholder_is_left_visible() {
        assert_eq!(render("{title", &vars(&[("title", "x")])), "{title");
    }

    #[test]
    fn values_are_escaped_but_template_markup_is_not() {
        // This is the case that matters: an ampersand in a real track title
        // makes Pango discard the whole label if it reaches the output raw.
        let v = vars(&[("title", "Me & You <3")]);
        assert_eq!(render("<b>{title}</b>", &v), "<b>Me &amp; You &lt;3</b>");
    }

    #[test]
    fn time_formats_drop_the_hour_until_needed() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(9_000), "0:09");
        assert_eq!(format_time(225_000), "3:45");
        assert_eq!(format_time(3_723_000), "1:02:03");
    }

    fn playing_state() -> State {
        let mut s = State::default();
        s.player = Some(Player {
            bus_name: "org.mpris.MediaPlayer2.spotify".into(),
            identity: "Spotify".into(),
            status: Status::Playing,
            track: Some(Track {
                title: "Digital Love".into(),
                artists: vec!["Daft Punk".into()],
                album: Some("Discovery".into()),
                length_ms: Some(301_000),
                ..Default::default()
            }),
            position_ms: 60_000,
            shuffle: None,
            repeat: None,
        });
        s
    }

    #[test]
    fn default_template_renders_a_full_track() {
        let cfg = BarConfig::default();
        let out = render_bar(&playing_state(), &cfg);
        assert_eq!(out.text, "▶  Digital Love · Daft Punk");
        assert_eq!(out.class, vec!["playing"]);
        assert_eq!(out.percentage, 20);
    }

    #[test]
    fn stopped_renders_empty_so_waybar_hides_the_module() {
        let out = render_bar(&State::default(), &BarConfig::default());
        assert_eq!(out.text, "");
        assert!(out.class.contains(&"no-player".to_string()));
    }

    #[test]
    fn a_track_with_no_artist_leaves_no_dangling_separator() {
        let mut s = playing_state();
        s.player.as_mut().unwrap().track.as_mut().unwrap().artists.clear();
        let out = render_bar(&s, &BarConfig::default());
        assert_eq!(out.text, "▶  Digital Love");
    }

    #[test]
    fn a_live_stream_drops_the_duration_group_from_the_tooltip() {
        let mut s = playing_state();
        s.player.as_mut().unwrap().track.as_mut().unwrap().length_ms = None;
        let out = render_bar(&s, &BarConfig::default());
        assert!(!out.tooltip.contains('/'), "tooltip kept an empty duration: {:?}", out.tooltip);
        assert!(out.tooltip.contains("Digital Love"));
    }

    #[test]
    fn a_whitespace_only_tooltip_counts_as_absent() {
        let cfg = BarConfig { tooltip: "[{artist}]".into(), ..Default::default() };
        let mut s = playing_state();
        s.player.as_mut().unwrap().track.as_mut().unwrap().artists.clear();
        assert_eq!(render_bar(&s, &cfg).tooltip, "");
    }
}
