# 0005: The window

The bar module was never the point. A string in a bar is a nicer `playerctl`
script. The window is the thing the architecture was arranged around, and it went
in as a second client of the existing daemon rather than as a rewrite, which is
the main thing worth reporting: the split held.

## What the split bought

The window process holds no state. It renders what the daemon sends and sends
commands back. Killing it loses nothing, and it reconnects on its own when the
daemon restarts underneath it.

The concrete payoff showed up mid-session. A fix landed in the daemon and took
effect in the bar without restarting Waybar, because the bar client is a pipe and
all the logic lives one process over.

## Two main loops

GTK owns the thread it was initialised on and runs glib's loop there. tokio wants
a loop too. Rather than marrying them, the socket lives on its own thread with
its own runtime and the two sides talk over `async-channel`, which glib can await
directly. Everything crossing the boundary is owned data, so no GTK type is ever
touched off the main thread.

This is the part the architecture notes flagged as an afternoon lost to nothing
if approached wrongly. Written this way it was uneventful.

## The scrubber does not take a gesture

The first version added a `GestureClick` to the scale to detect press and release,
so a drag would produce one seek at the end rather than one per motion event. It
silently did nothing: `GtkScale` claims the pointer sequence with its own internal
drag gesture, and an added click gesture never sees either event.

The working version drives off `change-value`, which is emitted for user changes
only and so cannot be confused with the position being written in from the daemon,
and fires the seek once the value stops changing for 150ms. That behaves like
release for a drag and like an immediate seek for a click, without needing to know
which one happened.

## A blank window, and a wrong first answer

The window mapped a layer surface at the right size and painted nothing. The
regression appeared right after the scrubber rewrite, so that was the first
suspect, and it was wrong.

Bisecting against the last commit that rendered narrowed it to the click-catching
surface added for dismissal. That surface is full screen and sits one layer below
the window. While it was opaque, the window above it did not draw at all.
Presumably a full-screen opaque surface lets the compositor treat what is behind
it as not worth drawing, and something in that reasoning catches the layer above
too. The mechanism is not established; the behaviour is reproducible and the fix
is one line.

`GtkApplicationWindow` carries a `background` style class that paints the theme's
window colour, and a transparent rule in your own stylesheet does not override it.
The class has to come off as well.

Present order was also suspected, and a deliberate check found it does not
matter. That check took two minutes and stopped a comment going in that would
have told the next person to preserve an ordering with no reason behind it.

## The mock player

Verification kept meaning "play something and look at it", which is a poor loop
and a worse one when the person whose speakers they are is in the room.

`waytify mock-player` serves a fake MPRIS player on the session bus. No audio, no
Spotify, a three-track playlist chosen for the shapes worth exercising: a normal
track, one with no album, and one long enough to make seeking visible.

It emits property changes from its transport methods, because a real player does.
An earlier version mutated state silently and the first test written against it
failed, correctly: the engine had nothing to react to. A mock that is quieter
than the thing it stands in for tests less than it appears to.

The integration suite now drives the same mock the command runs, so what CI
checks is what a contributor can reproduce by hand.

## Verified, not assumed

Against the mock: rendering, transport, drag to seek, Escape and click-outside
dismissal, the process staying resident while hidden, a user stylesheet applying,
and that stylesheet hot reloading into an open window.

The last one is the feature worth having. Editing `style.css` restyles a window
that is already on screen, which turns theming from a restart loop into
something you can actually iterate on.
