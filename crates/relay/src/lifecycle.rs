//! When a broadcast starts and ends: the encoder's comings and goings, the
//! streamer's End stream and Resume, and the destination connection, as one
//! state machine.
//!
//! It does no I/O. The core feeds it what happened, with the facts it needs
//! about the delay buffer ([`Facts`]), and carries out the [`Effect`]s it asks
//! for: starting and stopping the connection to the destination, and what to do
//! with the buffer. Kept apart like this, the rules can be tested on their own
//! against random sequences of events (see the tests below).

use std::time::Duration;

use tokio::time::Instant;
use tracing::{info, warn};

/// Once the encoder has gone for good, how long past the moment the last of the
/// stream was due to air a broadcast may keep draining to a destination that
/// has stopped taking data.
pub(crate) const DRAIN_SLACK: Duration = Duration::from_secs(30);

/// The encoder, as far as the broadcast is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Encoder {
    /// Streaming to stream-delay.
    Publishing,
    /// Its connection ended at `since`: on purpose if `stopped` (it
    /// unpublished), else it crashed or lost its connection and may come back.
    Left { since: Instant, stopped: bool },
    /// No stream: none yet, or the last one is over (its rest aired or was
    /// thrown away). `stopped` as for the last one; true before the first.
    Idle { stopped: bool },
}

/// What the broadcast is heading for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Air what the buffer has due.
    Live,
    /// End stream (after air): air up to the end mark, then end.
    EndAtMark { deadline: Instant },
    /// The encoder started a new stream while the previous one's end was still
    /// airing: end that broadcast at the end mark, then start a new one.
    RestartAtMark { deadline: Instant },
    /// Ended by the streamer: nothing is sent until they resume, or the encoder
    /// starts a new stream.
    Ended,
}

/// The connection to the destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Egress {
    Off,
    On { since: Instant },
}

/// What the core does for the lifecycle, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Stop the destination connection cleanly (what was written airs).
    StopEgress,
    /// Reset it: what the OS still holds to send is dropped.
    AbortEgress,
    /// Throw away the buffer; nothing in it may air.
    Discard,
    /// Nothing received after what has arrived so far may air: set the engine's
    /// end mark at the newest message.
    MarkEnd,
    /// Forget the end mark.
    CancelEnd,
    /// The broadcast ended at the end mark: prepare a new one for what came after.
    RestartAfterEnd,
    /// Show why a broadcast ended, as the destination's last error.
    Explain(&'static str),
}

/// What the lifecycle needs to know about the buffer and the destination.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Facts {
    /// A destination is set up.
    pub destination: bool,
    /// Something has been received (an end mark can be set).
    pub has_mark: bool,
    /// Everything up to the end mark has been handed to the connection.
    pub end_reached: bool,
    /// Nothing is queued for the connection.
    pub backlog_empty: bool,
    /// Everything received has been handed to the connection.
    pub drained: bool,
    /// The buffer holds something that may air.
    pub has_buffered: bool,
    /// The buffer has something due.
    pub output_wanted: bool,
    /// The connection is up.
    pub connected: bool,
    /// The delay in effect.
    pub delay: Duration,
}

/// What [`Lifecycle::tick`] wants the core to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tick {
    Nothing,
    /// Start the destination connection: call [`Lifecycle::started`] once it is
    /// on its way, or [`Lifecycle::start_failed`] if it cannot be (settings).
    Start,
}

#[derive(Debug)]
pub(crate) struct Lifecycle {
    encoder: Encoder,
    plan: Plan,
    egress: Egress,
    /// How long a crashed encoder may take to come back.
    grace: Duration,
}

impl Lifecycle {
    pub(crate) fn new(grace: Duration) -> Self {
        Self {
            encoder: Encoder::Idle { stopped: true },
            plan: Plan::Live,
            egress: Egress::Off,
            grace,
        }
    }

    pub(crate) fn ended(&self) -> bool {
        self.plan == Plan::Ended
    }

    /// End stream (after air) is under way.
    pub(crate) fn ending(&self) -> bool {
        matches!(self.plan, Plan::EndAtMark { .. })
    }

    pub(crate) fn egress_on(&self) -> bool {
        matches!(self.egress, Egress::On { .. })
    }

    #[cfg(test)]
    pub(crate) fn encoder(&self) -> Encoder {
        self.encoder
    }

    #[cfg(test)]
    pub(crate) fn plan(&self) -> Plan {
        self.plan
    }

    fn stop(&mut self, fx: &mut Vec<Effect>, how: Effect) {
        if self.egress_on() {
            self.egress = Egress::Off;
            fx.push(how);
        }
    }

    /// Ends the broadcast, if one is running, and forgets what is left of the
    /// stream, so none of it can start a broadcast later. A pending end at the
    /// mark is over too.
    fn finish(&mut self, fx: &mut Vec<Effect>) {
        self.stop(fx, Effect::StopEgress);
        fx.push(Effect::Discard);
        if let Encoder::Left { stopped, .. } = self.encoder {
            self.encoder = Encoder::Idle { stopped };
        }
        if matches!(
            self.plan,
            Plan::EndAtMark { .. } | Plan::RestartAtMark { .. }
        ) {
            self.plan = Plan::Live;
        }
    }

    fn deadline(now: Instant, f: &Facts) -> Instant {
        now + f.delay + DRAIN_SLACK
    }

    // ----- events -------------------------------------------------------------------

    /// The destination changed: a running connection reconnects with the new
    /// settings (the next tick starts it again).
    pub(crate) fn destination_changed(&mut self, fx: &mut Vec<Effect>) {
        self.stop(fx, Effect::StopEgress);
    }

    /// End stream now: nothing buffered airs, and the connection is reset so
    /// that what the OS still holds to send does not either.
    pub(crate) fn end_now(&mut self, fx: &mut Vec<Effect>) {
        // The buffer first, so nothing in it can be sent meanwhile.
        fx.push(Effect::Discard);
        self.stop(fx, Effect::AbortEgress);
        self.plan = Plan::Ended;
        info!("stream ended by the streamer; buffered content discarded");
    }

    /// End stream (after air): what has been received so far airs, then the
    /// broadcast ends.
    pub(crate) fn end_after_air(&mut self, now: Instant, f: &Facts, fx: &mut Vec<Effect>) {
        match self.plan {
            Plan::Ended | Plan::EndAtMark { .. } => {}
            // The stream was restarted in the encoder while the previous one's end
            // aired: that end still airs, and nothing of the new stream does.
            Plan::RestartAtMark { deadline } => {
                self.plan = Plan::EndAtMark { deadline };
                info!("the stream ends once the previous one's end has aired");
            }
            Plan::Live => {
                // Nothing is left to air, or nothing can. While connected, even with
                // nothing buffered, the end waits for what is queued for the
                // destination.
                let nothing_to_air = !f.destination || (f.drained && !self.egress_on());
                if f.has_mark && !nothing_to_air {
                    fx.push(Effect::MarkEnd);
                    self.plan = Plan::EndAtMark {
                        deadline: Self::deadline(now, f),
                    };
                    info!("the stream ends once what is buffered has aired");
                } else {
                    self.finish(fx);
                    self.plan = Plan::Ended;
                    info!("stream ended by the streamer; nothing was left to air");
                }
            }
        }
    }

    /// Resume: broadcast again after End stream, or take back an End stream
    /// (after air) that has not aired yet.
    pub(crate) fn resume(&mut self, fx: &mut Vec<Effect>) {
        match self.plan {
            Plan::Ended => {
                // The encoder may have kept sending while ended. None of that may
                // air: the new broadcast starts from what arrives from now.
                fx.push(Effect::Discard);
                self.plan = Plan::Live;
                info!("resuming the broadcast");
            }
            Plan::EndAtMark { .. } => {
                fx.push(Effect::CancelEnd);
                self.plan = Plan::Live;
                info!("the stream keeps going; End stream was cancelled");
            }
            Plan::Live | Plan::RestartAtMark { .. } => {}
        }
    }

    /// An encoder started publishing. `takeover`: it replaced a connection of
    /// the encoder's that had gone silent.
    pub(crate) fn publishing(
        &mut self,
        now: Instant,
        takeover: bool,
        f: &Facts,
        fx: &mut Vec<Effect>,
    ) {
        let stopped_before = match self.encoder {
            Encoder::Publishing => false,
            Encoder::Left { stopped, .. } | Encoder::Idle { stopped } => stopped && !takeover,
        };
        // The previous stream was stopped on purpose, and is still airing: it
        // gets its whole end, then this stream a new broadcast, as when streaming
        // straight to the destination. Carrying on with the old broadcast would
        // leave it without data for as long as the encoder was stopped, which
        // destinations take badly.
        let new_stream = matches!(self.encoder, Encoder::Left { stopped: true, .. });
        if new_stream && self.egress_on() {
            match self.plan {
                Plan::Live if f.has_mark => {
                    fx.push(Effect::MarkEnd);
                    self.plan = Plan::RestartAtMark {
                        deadline: Self::deadline(now, f),
                    };
                }
                // Starting a new stream after End stream (after air) means going
                // on after all: in a new broadcast.
                Plan::EndAtMark { deadline } => self.plan = Plan::RestartAtMark { deadline },
                _ => {}
            }
            if matches!(self.plan, Plan::RestartAtMark { .. }) {
                info!(
                    "new encoder stream; it starts a new broadcast once the previous one has aired"
                );
            }
        }
        self.encoder = Encoder::Publishing;
        // Stopping and starting the stream in the encoder is the natural way to go
        // live again after End stream. An encoder reconnecting by itself after its
        // connection dropped (OBS does this) is not: that stays ended.
        if self.plan == Plan::Ended {
            if stopped_before {
                self.plan = Plan::Live;
                info!("new encoder stream; broadcasting again");
            } else {
                info!("encoder reconnected; the stream stays ended until resumed");
            }
        }
    }

    /// The publishing encoder's connection ended; `stopped` if it unpublished.
    pub(crate) fn encoder_left(&mut self, now: Instant, stopped: bool) {
        if self.encoder == Encoder::Publishing {
            self.encoder = Encoder::Left {
                since: now,
                stopped,
            };
        }
    }

    /// The connection is on its way.
    pub(crate) fn started(&mut self, now: Instant) {
        self.egress = Egress::On { since: now };
    }

    /// The connection could not be started (the destination is not set up).
    pub(crate) fn start_failed(&mut self, now: Instant, fx: &mut Vec<Effect>) {
        if self.encoder_gone(now) {
            // Nor may the stream air later: once the settings are fixed, that
            // would start a broadcast of a stream that has finished.
            info!("encoder gone and the destination is not set up; discarding the stream");
            self.finish(fx);
        }
    }

    /// The encoder left more than the grace period ago and did not come back:
    /// the stream is over, apart from airing what is still buffered.
    fn encoder_gone(&self, now: Instant) -> bool {
        matches!(self.encoder, Encoder::Left { since, .. } if now.duration_since(since) >= self.grace)
    }

    /// Decides what the destination connection should do now.
    pub(crate) fn tick(&mut self, now: Instant, f: &Facts, fx: &mut Vec<Effect>) -> Tick {
        if !f.destination {
            self.stop(fx, Effect::StopEgress);
            return Tick::Nothing;
        }
        if self.end_at_mark(now, f, fx) {
            return Tick::Nothing;
        }
        let gone = self.encoder_gone(now);
        let (left_at, stopped) = match self.encoder {
            Encoder::Left { since, stopped } => (Some(since), stopped),
            _ => (None, false),
        };
        let Egress::On { since: started } = self.egress else {
            if gone && (self.ended() || !f.has_buffered) {
                // Nothing of this stream is left to air.
                self.finish(fx);
            } else if !self.ended() && f.output_wanted {
                // A stream that ended before any of it was due (shorter than the
                // delay) still airs, on schedule.
                return Tick::Start;
            }
            return Tick::Nothing;
        };
        // While the encoder streams, or may come back, the broadcast goes on.
        let Some(left_at) = left_at.filter(|_| gone || stopped) else {
            return Tick::Nothing;
        };
        // End the broadcast once everything buffered has been written to the
        // destination (not just queued for it: stopping would cut off the rest).
        if f.connected && f.drained && f.backlog_empty {
            info!("the stream is over and has all aired; ending the broadcast");
            self.finish(fx);
        } else if !f.connected {
            // Unreachable. While the encoder may come back, and until the
            // destination has had the grace period to answer, a reconnect may still
            // air the rest; after that, it would start a new broadcast of a stream
            // that has finished.
            if gone && now.duration_since(started) >= self.grace {
                info!("encoder gone and the destination is not connected; ending the broadcast");
                fx.push(Effect::Explain(
                    "The stream ended while the destination could not be reached; \
                     what had not aired yet was discarded.",
                ));
                self.finish(fx);
            }
        } else {
            let due = if stopped {
                f.delay
            } else {
                self.grace.max(f.delay)
            };
            if now.duration_since(left_at) >= due + DRAIN_SLACK {
                warn!("the destination stopped taking data; ending the broadcast without the rest");
                fx.push(Effect::Explain(
                    "The destination stopped taking data, so the broadcast was ended \
                     without the rest of the stream.",
                ));
                self.finish(fx);
            }
        }
        Tick::Nothing
    }

    /// Ends the broadcast at the end mark once everything up to it has been
    /// written, or when the destination has had long enough. True if it did.
    fn end_at_mark(&mut self, now: Instant, f: &Facts, fx: &mut Vec<Effect>) -> bool {
        let (Plan::EndAtMark { deadline } | Plan::RestartAtMark { deadline }) = self.plan else {
            return false;
        };
        let aired = f.end_reached && f.backlog_empty;
        if !aired && now < deadline {
            return false;
        }
        if !aired {
            warn!("the destination stopped taking data; ending the broadcast without the rest");
        }
        if matches!(self.plan, Plan::RestartAtMark { .. }) {
            info!("the previous stream has aired; ending its broadcast for the new one");
            self.stop(fx, Effect::StopEgress);
            // Keeps what the new stream has sent; its broadcast starts when due.
            fx.push(Effect::RestartAfterEnd);
            self.plan = Plan::Live;
        } else {
            info!("everything up to End stream has aired; ending the broadcast");
            self.finish(fx);
            self.plan = Plan::Ended;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const GRACE: Duration = Duration::from_secs(30);

    /// What can happen, with the facts the core would pass along.
    #[derive(Debug, Clone)]
    enum Step {
        Wait(u64),
        Publish { takeover: bool },
        Leave { stopped: bool },
        EndNow,
        EndAfterAir,
        Resume,
        DestinationChanged,
        Tick { start_fails: bool },
    }

    fn step() -> impl Strategy<Value = Step> {
        prop_oneof![
            (0u64..90).prop_map(Step::Wait),
            any::<bool>().prop_map(|takeover| Step::Publish { takeover }),
            any::<bool>().prop_map(|stopped| Step::Leave { stopped }),
            Just(Step::EndNow),
            Just(Step::EndAfterAir),
            Just(Step::Resume),
            Just(Step::DestinationChanged),
            any::<bool>().prop_map(|start_fails| Step::Tick { start_fails }),
        ]
    }

    fn facts() -> impl Strategy<Value = Facts> {
        (any::<[bool; 9]>(), 0u64..60).prop_map(|(b, delay)| Facts {
            destination: b[0] || b[8],
            has_mark: b[1],
            end_reached: b[2],
            backlog_empty: b[3],
            drained: b[4],
            has_buffered: b[5],
            output_wanted: b[6],
            connected: b[7],
            delay: Duration::from_secs(delay),
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// Whatever happens, in whatever order and with whatever the buffer says:
        #[test]
        fn lifecycle_rules_hold(steps in prop::collection::vec((step(), facts()), 1..60)) {
            let mut life = Lifecycle::new(GRACE);
            let mut now = Instant::now();
            // The streamer ended the stream, and neither resumed nor started a new
            // stream in the encoder since.
            let mut ended_by_streamer = false;
            // The engine has an end mark (effects set and clear it).
            let mut marked = false;
            for (step, f) in steps {
                let before = life.egress;
                let mut fx = Vec::new();
                match step {
                    Step::Wait(s) => now += Duration::from_secs(s),
                    Step::Publish { takeover } => {
                        let new_stream = matches!(
                            life.encoder(),
                            Encoder::Left { stopped: true, .. } | Encoder::Idle { stopped: true }
                        ) && !takeover;
                        life.publishing(now, takeover, &f, &mut fx);
                        if new_stream {
                            ended_by_streamer = false;
                        }
                    }
                    Step::Leave { stopped } => life.encoder_left(now, stopped),
                    Step::EndNow => {
                        life.end_now(&mut fx);
                        ended_by_streamer = true;
                        // Nothing buffered airs, and the connection is reset.
                        prop_assert_eq!(fx.first(), Some(&Effect::Discard));
                        prop_assert!(!fx.contains(&Effect::StopEgress));
                    }
                    Step::EndAfterAir => {
                        let was = life.plan();
                        life.end_after_air(now, &f, &mut fx);
                        if life.ended() && was != Plan::Ended {
                            ended_by_streamer = true;
                            // Ending at once throws the rest away.
                            prop_assert!(fx.contains(&Effect::Discard));
                        }
                    }
                    Step::Resume => {
                        life.resume(&mut fx);
                        if !life.ended() {
                            ended_by_streamer = false;
                        }
                    }
                    Step::DestinationChanged => life.destination_changed(&mut fx),
                    Step::Tick { start_fails } => {
                        let was = life.plan();
                        if life.tick(now, &f, &mut fx) == Tick::Start {
                            prop_assert!(!life.egress_on(), "started twice");
                            prop_assert!(!life.ended(), "started while ended");
                            prop_assert!(f.destination && f.output_wanted);
                            if start_fails {
                                life.start_failed(now, &mut fx);
                            } else {
                                life.started(now);
                            }
                        }
                        if life.ended() && was != Plan::Ended {
                            // Only an End stream (after air) reaching its mark ends
                            // it here, and what is left is thrown away.
                            prop_assert!(matches!(was, Plan::EndAtMark { .. }), "ended from {:?}", was);
                            prop_assert!(fx.contains(&Effect::Discard));
                            ended_by_streamer = true;
                        }
                    }
                }
                // Connections are stopped only when running, and never started
                // except through `started`.
                let stops = fx.iter().filter(|e| matches!(e, Effect::StopEgress | Effect::AbortEgress)).count();
                prop_assert!(stops <= 1);
                if stops == 1 {
                    prop_assert!(matches!(before, Egress::On { .. }), "stopped while {:?}", before);
                    prop_assert_eq!(life.egress, Egress::Off);
                }
                // Ended means nothing is sent.
                if life.ended() {
                    prop_assert_eq!(life.egress, Egress::Off);
                }
                // Once ended by the streamer, only Resume or a new stream from the
                // encoder (not a reconnect) broadcasts again.
                if ended_by_streamer {
                    prop_assert!(life.ended(), "{:?}", life.plan());
                }
                // An end at the mark always has a mark to end at.
                for e in &fx {
                    match e {
                        Effect::MarkEnd => marked = true,
                        Effect::CancelEnd | Effect::RestartAfterEnd | Effect::Discard => {
                            marked = false
                        }
                        _ => {}
                    }
                }
                if let Plan::EndAtMark { .. } | Plan::RestartAtMark { .. } = life.plan() {
                    prop_assert!(marked, "{:?} without a mark", life.plan());
                }
                prop_assert!(!(fx.contains(&Effect::MarkEnd) && fx.contains(&Effect::CancelEnd)));
            }
        }
    }

    #[test]
    fn a_reconnecting_encoder_does_not_undo_end_stream() {
        let mut life = Lifecycle::new(GRACE);
        let now = Instant::now();
        let f = Facts {
            destination: true,
            has_mark: true,
            output_wanted: true,
            ..Default::default()
        };
        let mut fx = Vec::new();
        life.publishing(now, false, &f, &mut fx);
        assert_eq!(life.tick(now, &f, &mut fx), Tick::Start);
        life.started(now);
        life.end_now(&mut fx);
        assert_eq!(fx, vec![Effect::Discard, Effect::AbortEgress]);
        // OBS lost its connection and came back by itself.
        life.encoder_left(now, false);
        life.publishing(now, false, &f, &mut fx);
        assert!(life.ended());
        assert_eq!(life.tick(now, &f, &mut fx), Tick::Nothing);
        // The streamer stops and starts the stream in OBS: a new broadcast.
        life.encoder_left(now, true);
        life.publishing(now, false, &f, &mut fx);
        assert!(!life.ended());
        assert_eq!(life.tick(now, &f, &mut fx), Tick::Start);
    }

    #[test]
    fn end_stream_during_a_restart_ends_after_the_previous_end() {
        let mut life = Lifecycle::new(GRACE);
        let now = Instant::now();
        let f = Facts {
            destination: true,
            has_mark: true,
            output_wanted: true,
            connected: true,
            delay: Duration::from_secs(30),
            ..Default::default()
        };
        let mut fx = Vec::new();
        life.publishing(now, false, &f, &mut fx);
        life.tick(now, &f, &mut fx);
        life.started(now);
        // OBS stops and starts again while the delayed end airs.
        life.encoder_left(now, true);
        life.publishing(now, false, &f, &mut fx);
        assert!(matches!(life.plan(), Plan::RestartAtMark { .. }));
        // The streamer ends the stream: the previous end still airs, then it ends.
        life.end_after_air(now, &f, &mut fx);
        assert!(life.ending());
        let aired = Facts {
            end_reached: true,
            backlog_empty: true,
            ..f
        };
        fx.clear();
        life.tick(now, &aired, &mut fx);
        assert!(life.ended());
        assert_eq!(fx, vec![Effect::StopEgress, Effect::Discard]);
    }
}
