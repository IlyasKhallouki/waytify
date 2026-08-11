# Theming

waytify has two halves and they are styled in two different places.

The **bar module** is a Waybar widget, so it is styled from your Waybar
stylesheet. waytify only decides what CSS classes it carries.

The **player window** is a GTK4 application and is styled from
`~/.config/waytify/style.css`, which is reloaded whenever you save it.

Widget names and classes are treated as public API. Renaming one is a breaking
change, so a stylesheet written against this document keeps working.

## The bar module

waytify sets classes describing the current state. Select them from your existing
Waybar stylesheet, alongside your other modules.

```css
#custom-waytify.playing   { color: #a6e3a1; }
#custom-waytify.paused    { color: #6c7086; }
#custom-waytify.no-player { padding: 0; margin: 0; }
```

| Class | When |
| --- | --- |
| `playing` | Something is playing |
| `paused` | Playback is paused |
| `stopped` | A player is running with nothing loaded |
| `no-player` | No player at all. The module renders empty text, so it collapses |
| `liked` | The current track is saved to your library |
| `remote` | Audio is on a Spotify Connect device rather than this machine |
| `no-premium` | An account is connected but cannot use playback controls |

The last three arrive with the Spotify layer. Styling them now is safe and does
nothing until then.

## The player window

Start from nothing: every rule below is already applied by the built-in
stylesheet, and yours only needs to contain what you want to change.

```sh
mkdir -p ~/.config/waytify
$EDITOR ~/.config/waytify/style.css
```

Saving the file restyles the window in place. There is no need to restart
anything, and the window does not even need to be closed.

### Structure

```
#waytify-window                     the layer-shell window, transparent
└── #waytify-popup                  the panel: background, radius, padding
    ├── .waytify-header
    │   ├── .waytify-art            cover image
    │   │   └── .art-missing        stands in when there is no cover
    │   └── .waytify-meta
    │       ├── .track-title
    │       ├── .track-artist
    │       └── .track-album
    ├── .waytify-scrubber
    │   ├── .elapsed
    │   ├── .scrubber               a GtkScale: trough, highlight, slider
    │   └── .duration
    ├── .waytify-volume             hidden when there is no stream to control
    │   ├── .mute                   gains .muted when muted
    │   ├── .volume-slider          a GtkScale
    │   └── .output                 device picker, hidden with only one output
    └── .waytify-transport
        ├── .shuffle                a GtkToggleButton, so :checked applies
        ├── .prev
        ├── .playpause
        ├── .next
        └── .repeat

#waytify-dismiss                    full-screen click catcher, transparent
└── .waytify-backdrop               style this for a dimmed backdrop

.waytify-outputs                    the output picker popover
└── .device                         one output, gains .active for the current one
```

The volume row disappears entirely when the player has no local audio stream,
which is normal while nothing is producing sound and always true once playback
moves to a remote device. A slider that does nothing is worse than no slider.

`#waytify-popup` also carries the same state classes as the bar module, so
`#waytify-popup.paused` and `#waytify-popup.no-player` work. It additionally
carries `offline` when the daemon has gone away and the window is showing the
last state it knew.

### Album art colours

Three colours are extracted from the current cover and exposed as named GTK
colours. They change with the track.

| Colour | What it is |
| --- | --- |
| `@art_vibrant` | The most saturated colour in the cover that stays legible against the panel background |
| `@art_muted` | A low saturation companion, for large fills where the vibrant one would be too loud |
| `@art_on_vibrant` | Black or white, whichever is readable on top of `@art_vibrant` |

Nothing in the default stylesheet uses them. Following the record is a choice a
theme makes rather than one imposed on everyone:

```css
#waytify-popup .scrubber highlight        { background-color: @art_vibrant; }
#waytify-popup .waytify-transport .playpause {
  background-color: @art_vibrant;
  color: @art_on_vibrant;
}
```

They are always defined, so referencing one is safe even before any cover has
loaded. Before the first track they fall back to the GTK theme's own background
and foreground.

The contrast check is against the default panel background. If your theme uses a
very different background, a colour that technically passes may still look wrong,
so check the result rather than trusting it.

### Examples

A dimmed backdrop while the window is open:

```css
#waytify-dismiss .waytify-backdrop {
  background-color: rgba(0, 0, 0, 0.35);
}
```

Light theme:

```css
#waytify-popup {
  background-color: #faf9f7;
  color: #1c1b19;
}
#waytify-popup .track-artist { color: #5c5850; }
#waytify-popup .track-album  { color: #8a857b; }
#waytify-popup .scrubber trough { background-color: #e2ded6; }
#waytify-popup .waytify-transport .playpause {
  background-color: #1c1b19;
  color: #faf9f7;
}
```

Bigger cover art:

```css
/* The window sizes itself to its contents, so the panel grows to match. */
#waytify-popup .waytify-art { min-width: 128px; min-height: 128px; }
```

Note that art is drawn at a fixed pixel size set in code, so CSS can change the
box around it but not the image resolution.

### What GTK4 CSS does not do

It looks like web CSS and is not. There is no `float`, no `flex`, no
`position: absolute`, and no descendant layout control. Rules affect painting and
spacing, not structure.

`min-width` and `min-height` are how you size things. `padding`, `margin`,
`border`, `border-radius`, `background`, `color`, `font-*`, `opacity` and
`transition` all work as you would expect.

## Debugging a stylesheet

GTK reports parse errors on stderr rather than refusing to load, so a broken rule
is skipped quietly while the rest applies. To see the errors, run the window in
the foreground:

```sh
waytify popup --show
```

A rule that parses but does not apply is usually a selector that does not match.
`GTK_DEBUG=interactive waytify popup --show` opens the inspector, where the CSS
tab shows the real node tree and which rules matched.

To see the window without involving your music:

```sh
waytify mock-player &          # a fake player, no audio
waytify popup --show
```

Setting `WAYTIFY_MOCK_ART=/path/to/an/image.png` gives the mock cover art, which
is the only way to exercise the art colours without playing something real.
