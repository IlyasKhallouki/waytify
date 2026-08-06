//! Playback position, tracked locally instead of asked for.
//!
//! Querying a player's `Position` property often enough to drive a smooth
//! scrubber means a D-Bus round trip every frame. Instead we record the last
//! position we saw along with the instant we saw it, then advance it against the
//! local clock and re-anchor whenever the player tells us anything at all.
//!
//! MPRIS does have a `Seeked` signal for announcing jumps, and it is used when it
//! arrives. It is just not depended on. Support varies across players, and a
//! clock that only corrects itself on `Seeked` drifts silently on any player that
//! does not send one. Re-anchoring on every property change costs nothing and
//! works either way.
//!
//! For the record, Spotify's Linux client does emit `Seeked` correctly, measured
//! on 1.2.92.147. That is worth stating because the opposite is often repeated.

use std::time::{Duration, Instant};

/// Who is currently looking at the position, which decides how often it is worth
/// spending a D-Bus round trip to correct drift.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Attention {
    /// No clients connected. Nothing is rendering a position, so nothing needs one.
    #[default]
    Idle,
    /// Only the bar is connected. It shows at most a coarse elapsed time.
    Bar,
    /// The popup is open and drawing a scrubber, so accuracy is visible.
    Popup,
}

impl Attention {
    /// How often to re-read the player's real status and position, or `None` to
    /// leave it alone entirely.
    ///
    /// With no clients connected nothing is rendering, so nothing can be visibly
    /// wrong and nothing is polled. That is what keeps an idle machine at zero
    /// D-Bus traffic.
    ///
    /// While playing, the rate follows who is watching, since a visible scrubber
    /// deserves a tighter position than a bar showing coarse elapsed time.
    ///
    /// While paused the position cannot drift, but the belief that playback is
    /// paused can itself be wrong if a status signal was missed. That case does
    /// not self-correct: nothing else will re-check until the track changes, so
    /// the bar would sit on the wrong icon indefinitely. A slow tick costs one
    /// call every thirty seconds and makes it recover on its own.
    pub fn poll_interval(self, playing: bool) -> Option<Duration> {
        match (self, playing) {
            (Attention::Idle, _) => None,
            (Attention::Popup, true) => Some(Duration::from_secs(5)),
            (Attention::Bar, true) => Some(Duration::from_secs(30)),
            (_, false) => Some(Duration::from_secs(30)),
        }
    }
}

/// Interpolates playback position between the points where we actually observe it.
#[derive(Debug, Clone)]
pub struct PositionClock {
    anchor_ms: u64,
    anchor_at: Instant,
    playing: bool,
    length_ms: Option<u64>,
    /// While the user drags the scrubber the clock is frozen, so the thumb does
    /// not fight the pointer.
    held: bool,
}

impl PositionClock {
    pub fn new(now: Instant) -> Self {
        Self { anchor_ms: 0, anchor_at: now, playing: false, length_ms: None, held: false }
    }

    /// Record an observed position. Called on every property change, on `Seeked`
    /// if the player bothers to send one, and on each drift correction.
    pub fn anchor(&mut self, position_ms: u64, playing: bool, now: Instant) {
        self.anchor_ms = position_ms;
        self.anchor_at = now;
        self.playing = playing;
    }

    /// Change play state without moving the position.
    ///
    /// Re-anchoring first matters: the time already elapsed belongs to the old
    /// state, and folding it in afterwards would either lose or invent it.
    pub fn set_playing(&mut self, playing: bool, now: Instant) {
        if self.playing == playing {
            return;
        }
        self.anchor_ms = self.position_at(now);
        self.anchor_at = now;
        self.playing = playing;
    }

    /// Track length, used to clamp. `None` for live streams, which have no end.
    pub fn set_length(&mut self, length_ms: Option<u64>) {
        self.length_ms = length_ms;
    }

    /// Freeze the clock while the scrubber is being dragged.
    pub fn hold(&mut self, now: Instant) {
        if !self.held {
            self.anchor_ms = self.position_at(now);
            self.anchor_at = now;
            self.held = true;
        }
    }

    /// Resume after a drag, anchoring optimistically at where the user let go
    /// rather than waiting for the player to confirm the seek.
    pub fn release(&mut self, position_ms: u64, now: Instant) {
        self.held = false;
        self.anchor(position_ms, self.playing, now);
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Interpolated position, clamped to the track length when one is known.
    pub fn position_at(&self, now: Instant) -> u64 {
        let mut pos = self.anchor_ms;
        if self.playing && !self.held {
            pos += now.saturating_duration_since(self.anchor_at).as_millis() as u64;
        }
        match self.length_ms {
            Some(len) => pos.min(len),
            None => pos,
        }
    }

    pub fn position(&self) -> u64 {
        self.position_at(Instant::now())
    }

    /// Whether an observed position differs from the interpolated one by enough
    /// to be worth pushing to clients.
    ///
    /// Small disagreements are constant and meaningless: D-Bus latency alone puts
    /// a few milliseconds between the two. Only a gap large enough to be visible
    /// should cause a redraw.
    pub fn drifted(&self, observed_ms: u64, now: Instant, tolerance_ms: u64) -> bool {
        self.position_at(now).abs_diff(observed_ms) > tolerance_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn paused_position_does_not_advance() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.anchor(10_000, false, start);
        assert_eq!(c.position_at(start + Duration::from_secs(60)), 10_000);
    }

    #[test]
    fn playing_position_advances_with_the_clock() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.anchor(10_000, true, start);
        assert_eq!(c.position_at(start + Duration::from_secs(5)), 15_000);
    }

    #[test]
    fn pausing_banks_elapsed_time_instead_of_losing_it() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.anchor(0, true, start);

        let at_3s = start + Duration::from_secs(3);
        c.set_playing(false, at_3s);

        // Three seconds were played, so they count, and nothing accrues after.
        assert_eq!(c.position_at(at_3s), 3_000);
        assert_eq!(c.position_at(at_3s + Duration::from_secs(90)), 3_000);
    }

    #[test]
    fn resuming_continues_from_where_it_paused() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.anchor(0, true, start);
        let at_3s = start + Duration::from_secs(3);
        c.set_playing(false, at_3s);

        let at_60s = start + Duration::from_secs(60);
        c.set_playing(true, at_60s);
        // The 57 seconds spent paused must not appear in the position.
        assert_eq!(c.position_at(at_60s + Duration::from_secs(2)), 5_000);
    }

    #[test]
    fn redundant_state_changes_are_ignored() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.anchor(0, true, start);
        // Some players re-emit PlaybackStatus on every metadata change. Treating
        // each as a transition would re-anchor constantly, which is harmless but
        // makes the drift check useless.
        let at_5s = start + Duration::from_secs(5);
        c.set_playing(true, at_5s);
        assert_eq!(c.position_at(at_5s), 5_000);
    }

    #[test]
    fn position_clamps_to_track_length() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.set_length(Some(30_000));
        c.anchor(29_000, true, start);
        assert_eq!(c.position_at(start + Duration::from_secs(10)), 30_000);
    }

    #[test]
    fn live_streams_are_not_clamped() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.set_length(None);
        c.anchor(0, true, start);
        assert_eq!(c.position_at(start + Duration::from_secs(3_600)), 3_600_000);
    }

    #[test]
    fn holding_freezes_the_clock_during_a_drag() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.anchor(10_000, true, start);

        let at_2s = start + Duration::from_secs(2);
        c.hold(at_2s);
        assert_eq!(c.position_at(at_2s + Duration::from_secs(30)), 12_000);

        // Releasing jumps to wherever the user dropped it and resumes from there.
        let at_40s = start + Duration::from_secs(40);
        c.release(90_000, at_40s);
        assert_eq!(c.position_at(at_40s + Duration::from_secs(1)), 91_000);
    }

    #[test]
    fn drift_tolerates_round_trip_latency() {
        let start = t0();
        let mut c = PositionClock::new(start);
        c.anchor(10_000, true, start);
        let at_1s = start + Duration::from_secs(1);
        // 40ms of disagreement is normal D-Bus latency, not a seek.
        assert!(!c.drifted(11_040, at_1s, 250));
        // Two seconds is somebody hitting the scrubber in another client.
        assert!(c.drifted(13_000, at_1s, 250));
    }

    #[test]
    fn nothing_is_polled_when_nobody_is_watching() {
        // This is the check that keeps an idle machine at zero D-Bus traffic.
        assert_eq!(Attention::Idle.poll_interval(true), None);
        assert_eq!(Attention::Idle.poll_interval(false), None);
    }

    #[test]
    fn poll_rate_follows_who_is_watching() {
        assert_eq!(Attention::Bar.poll_interval(true), Some(Duration::from_secs(30)));
        assert_eq!(Attention::Popup.poll_interval(true), Some(Duration::from_secs(5)));
    }

    #[test]
    fn a_paused_player_is_still_reconciled_slowly() {
        // Not for position, which cannot drift while paused, but for status.
        // If a PlaybackStatus signal is ever missed while we believe playback is
        // paused, no event will arrive to correct it and the bar would show the
        // wrong icon until the track changed. Regression guard for exactly that.
        for a in [Attention::Bar, Attention::Popup] {
            assert_eq!(
                a.poll_interval(false),
                Some(Duration::from_secs(30)),
                "{a:?} left a paused player unreconciled"
            );
        }
    }
}
