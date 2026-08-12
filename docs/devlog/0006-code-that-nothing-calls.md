# 0006: Code that nothing calls

The roadmap finished this session. The queue and lyrics both went in, which were
the last two features, and the interesting part of both was the same thing: work
that existed and was not reachable, and a claim in the docs that was not true.

## A function with no callers

The Spotify client had a `queue()` method. The state had a `queue` field. The
popup rendered no queue. Three pieces, none of them joined, sitting there long
enough to feel finished.

It was found by grepping for writes to the field rather than by reading the code,
which is worth repeating: reading the code shows you what is there, and the thing
that was wrong was the absence of something.

```
state.spotify.queue writes:
popup renders queue:
(no matches)
```

The same class of gap had already been found once, in `set_remote_volume`, which
also existed with nothing calling it. Two in one codebase suggests the failure
mode is structural rather than careless. Writing a client method and writing the
thing that calls it are separate sittings, and between them the method looks
finished, because it compiles, has a test, and is the kind of thing that would be
called.

The check that catches it is cheap: for any new method on a client or field on
the state, grep for the callers before calling it done. If there are none, either
wire it or delete it. Shipping it is the one option that is not honest.

That rule is why the lyrics module lost its `cache_path` helper before the first
commit. It was written, it was reasonable, nothing called it.

## The queue belongs to whatever is playing

Wiring it up raised a question the device list never had. The Connect device list
describes the account, so it is equally true whatever is playing. A queue
describes a session. Your account still has one during a YouTube video, and
listing Spotify's next track under that video would be worse than listing
nothing.

The gate is the current track's Spotify catalogue id, not the name of the
attached player. Playback on a phone over Connect has no local MPRIS player at
all, and the queue is still real in that case.

Spotify also answers the queue endpoint with 204 and an empty body when there is
no playback to have a queue for. Parsing that as JSON fails, and treating the
failure as an error would leave the previous queue on screen after playback
stopped. That is an answer, not a failure.

## Lyrics were mostly other people's edge cases

lrclib is the only free source with synced timings, no account and no key. Its
whole terms are a User-Agent saying who is calling.

Almost all the work was in what the data actually contains rather than what the
format documents. Timestamps come as hundredths or thousandths, sometimes with a
colon where the dot should be. One line can carry several timestamps, which is
how a repeated chorus is written without repeating its text. Blank lines are
timed, and dropping them as empty leaves the last line of a verse on screen
through a thirty second solo.

The choosing matters as much as the parsing. Live versions, radio edits and
remasters share a title and an artist, and lyrics timed against one scroll
visibly wrong against another. Anything more than five seconds from the length
being played is a different recording, and no answer beats a wrong one.

Misses are cached along with hits. Most tracks have no synced lyrics, and without
that the window asks lrclib the same question every time it opens, forever.

## The live test earned its place

Everything above is tested against fixed input. One test hits the real service
and is excluded from the normal run. It caught the only real bug in the module,
and it was not a bug about lrclib.

Players publish a length of zero while a track is still loading. Reading that as
a length asks for a recording of no duration, matches nothing, and then caches
that as the answer for the track. Zero is not a length, it is a player that has
not said yet.

No amount of local testing finds that, because the fixtures all have lengths.

## The claim that was not true

The last bug was the best one, and it was in a sentence I had written myself.

Three features are documented as being fetched only while the window is open: the
device list, the queue, and now lyrics. The daemon decided how hard to work from
who was connected. The window stays connected while hidden, because reopening it
has to be instant and starting GTK takes long enough to feel broken.

So a window opened once at login left the daemon polling the Spotify API and
lrclib on every track change for the rest of the session, for frames nobody could
see. The docs said otherwise. Two commit messages said otherwise. The code had
said otherwise since the device list went in.

Connecting and watching are now separate. The window says when it is on screen,
and everything fetched on its behalf waits for that. A hidden window falls back to
the pace of a bar, which is what it is while hidden.

It has to say it again when the daemon comes back, since a restarted daemon knows
nothing about a window that is still open. Without that, the queue, the device
list and the lyrics sit frozen until the user closes and reopens it. That is the
kind of bug that gets reported as "it randomly stops updating" and takes a week
to reproduce.

Confirmed against a live daemon rather than reasoned about: four seconds
subscribed and hidden produced no request at all, and lrclib was contacted a
second after the window announced itself.

## Testing a window without opening one

The window had no tests. Everything about it had been verified by opening it and
looking, which does not survive a refactor and cannot run anywhere but a desktop.

Widgets do not need a window. Building the tree and calling `render` into it
exercises the same code the compositor would drive, and nothing is ever
presented, so nothing appears on screen. That turned the whole of the rendering
into something testable: that the queue caps at the number of rows it claims, that
the same state arriving a second later does not stack duplicate rows, that the
highlighted lyric line moves on the window's own clock with no new frame from the
daemon.

Both halves were confirmed by breaking them first. Removing the row cap fails on
twelve rows; skipping the clear fails on seven. A test that has never failed is a
test you are trusting on faith.

GTK may only be used from the thread that initialised it, and the harness gives
every test its own thread, so this is one test rather than several. It skips
where there is no display. CI runs it under Xvfb, because a test that skips on
the runner is a test that passes having tested nothing.

## What the daemon publishes, and what the window works out

Lyrics are the first feature where the publish rate was visible. The daemon
publishes once a second while playing. That is fine for a scrubber and wrong for
a lyric line, which then lands up to a second after it is sung.

The bar cannot fix this, and that is exactly why the daemon republishes at all:
the bar receives text that has already been rendered, so it has nothing to
interpolate. The window receives a position and a status, so it can work out
where playback has got to instead of waiting to be told. It re-anchors on every
frame, so it never drifts away from the daemon, and a seek still lands where the
daemon says it did.

That distinction is the architecture doing what it was arranged to do, two
devlogs after it was written down.
