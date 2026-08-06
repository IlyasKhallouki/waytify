# 0004: A clock that only moved when something else did

Next report: the elapsed time is wrong.

This one took two minutes, because the first thing done was to watch it rather
than reason about it. Forty five seconds of playback, and the bar received
exactly one frame.

## Why

The daemon publishes when state changes. A position advancing smoothly is not a
change, so nothing published, so the time sat wherever it was until some other
event happened to come along and carry a fresh position with it.

That is why it looked intermittently correct rather than obviously broken. Track
changes, pauses and seeks all publish, and each of those publishes the right
time. Between them it froze. Checking it right after pressing something always
showed the correct value, which is exactly when anyone would check.

## The part that was already written down

The design notes said clients interpolate position locally, and that is true of
the popup, which will receive a position and a play state and can advance them
against its own clock.

It is not true of the bar, and the reason is a decision made on the first day:
rendering happens in the daemon so that format strings live in one config file
and the bar client carries no formatting logic. That decision is still right. It
also means the bar receives finished text. There is no position in it to advance,
so a client that wanted to keep the clock moving could not.

Two reasonable decisions, and the gap between them was never noticed because each
was written down in a different place.

## The fix

While playing, and while at least one client is connected, the daemon republishes
once a second. No D-Bus call, just a clone and a channel send, with the position
coming from the interpolated clock rather than from the player. Reconciliation
keeps its own slower schedule, since asking the player anything is a different
cost from re-rendering what we already know.

The idle guarantee survives: no clients means no heartbeat and no traffic.

## The test

The mock's own position is deliberately left where it is, so the test can only
pass if the interpolated clock is reaching clients rather than the player being
polled harder. It was checked against the unfixed code, where it fails with
`Err(Elapsed)` after five seconds of nothing arriving.

That check is becoming the habit worth keeping from this stretch of work. Three
bugs in a row now, and the useful step each time was the same one: look at what
the thing actually emits before deciding what is wrong with it.
