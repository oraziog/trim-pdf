//! One bar and one estimate out of the phases a job reports.
//!
//! The phases do not cost the same: writing six gigabytes takes a minute, planning the parts takes
//! milliseconds. Each phase therefore carries a weight, and the bar is the weighted sum of what is
//! finished plus the share of the phase in progress. The weights come from the measurements in
//! `MD/m2-split.md` and `MD/m3-cut.md`: the writing is most of the time, the verification about a
//! fifth of it, a text pass about a third of the writing.
//!
//! The estimate is the elapsed time divided by how far the bar has come, averaged with the
//! previous estimate so it does not jump between two frames.

use std::time::{Duration, Instant};

use splitter::{Stage, Update};

/// How much of the work each phase is, relative to the others.
fn weight(stage: Stage) -> f32 {
    match stage {
        Stage::Open => 0.5,
        Stage::Structure => 1.0,
        Stage::Search => 6.0,
        Stage::Plan => 1.0,
        Stage::Write => 14.0,
        Stage::Verify => 4.0,
    }
}

/// The phases of one job: the bar, the phase in progress, and the estimate.
#[derive(Debug)]
pub struct Meter {
    /// The phases this job will go through, in order, with their weights.
    phases: Vec<(Stage, f32)>,
    /// Which of them is running.
    current: usize,
    /// How far the running phase has come, from 0 to 1.
    fraction: f32,
    started: Instant,
    /// The last estimate, smoothed with the next one.
    eta: Option<Duration>,
    /// True once the work said it had finished its last phase.
    finished: bool,
}

impl Meter {
    /// A meter for a job that will pass through `stages`, starting now.
    #[must_use]
    pub fn new(stages: &[Stage], now: Instant) -> Self {
        let mut phases: Vec<(Stage, f32)> = stages.iter().map(|stage| (*stage, weight(*stage))).collect();
        if phases.is_empty() {
            phases.push((Stage::Plan, 1.0));
        }
        Self { phases, current: 0, fraction: 0.0, started: now, eta: None, finished: false }
    }

    /// Take one report. A report from a phase that has already gone by is ignored, and so is one
    /// from a phase further ahead than the meter knows about.
    pub fn feed(&mut self, update: Update, now: Instant) {
        let Some(index) = self.phases.iter().position(|(stage, _)| *stage == update.stage) else { return };
        if index < self.current {
            return;
        }
        // A new phase starts its own count from nothing.
        if index > self.current {
            self.fraction = 0.0;
            self.current = index;
        }
        // Pages read by several threads arrive out of order: the bar only moves forward.
        self.fraction = self.fraction.max(update.fraction());
        if self.current + 1 == self.phases.len() && update.fraction() >= 1.0 {
            self.finished = true;
        }
        self.reestimate(now);
    }

    /// How far the whole job has come, from 0 to 1.
    #[must_use]
    pub fn overall(&self) -> f32 {
        if self.finished {
            return 1.0;
        }
        let total: f32 = self.phases.iter().map(|(_, weight)| weight).sum();
        let done: f32 = self.phases.iter().take(self.current).map(|(_, weight)| weight).sum();
        let running = self.phases.get(self.current).map_or(0.0, |(_, weight)| weight * self.fraction);
        if total <= 0.0 { 1.0 } else { ((done + running) / total).clamp(0.0, 1.0) }
    }

    /// The phase being reported.
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.phases.get(self.current).map_or(Stage::Plan, |(stage, _)| *stage)
    }

    /// How long the job has been running.
    #[must_use]
    pub fn elapsed(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.started)
    }

    /// How long is left, from how far the job has come. `None` until there is enough to go on.
    #[must_use]
    pub fn eta(&self, now: Instant) -> Option<Duration> {
        if self.finished {
            return Some(Duration::ZERO);
        }
        let overall = self.overall();
        if overall < 0.02 {
            return None;
        }
        let elapsed = self.elapsed(now).as_secs_f32().max(0.001);
        let raw = elapsed * (1.0 - overall) / overall;
        Some(Duration::from_secs_f32(raw.min(86_400.0)))
    }

    /// Average the new estimate with the old one, so the number does not twitch.
    fn reestimate(&mut self, now: Instant) {
        let Some(raw) = self.eta(now) else { return };
        self.eta = Some(match self.eta {
            Some(previous) => {
                let mixed = previous.as_secs_f32() * 0.6 + raw.as_secs_f32() * 0.4;
                Duration::from_secs_f32(mixed)
            }
            None => raw,
        });
    }

    /// The estimate as it stands after the last report.
    #[must_use]
    pub fn last_eta(&self) -> Option<Duration> {
        self.eta
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use splitter::Unit;

    fn at(seconds: u64) -> Instant {
        // A fixed origin: the meter only ever subtracts one instant from another.
        Instant::now() + Duration::from_secs(seconds)
    }

    #[test]
    fn the_bar_moves_through_the_phases() {
        let start = at(0);
        let mut meter = Meter::new(&[Stage::Structure, Stage::Plan, Stage::Write, Stage::Verify], start);
        assert_eq!(meter.stage(), Stage::Structure);
        assert!(meter.overall() < 0.01);

        meter.feed(Update::of_part(Stage::Write, 1, 2, 0, 100, Unit::Bytes), at(1));
        let after_first = meter.overall();
        meter.feed(Update::of_part(Stage::Write, 2, 2, 50, 100, Unit::Bytes), at(2));
        assert!(meter.overall() > after_first, "the bar moves with the writing");

        meter.feed(Update::of_part(Stage::Verify, 2, 2, 2, 2, Unit::Parts), at(3));
        assert!(meter.overall() == 1.0);
        assert_eq!(meter.overall(), 1.0);
    }

    #[test]
    fn a_report_from_an_earlier_phase_does_not_drag_the_bar_back() {
        let start = at(0);
        let mut meter = Meter::new(&[Stage::Structure, Stage::Write, Stage::Verify], start);
        meter.feed(Update::of_part(Stage::Write, 1, 1, 300, 600, Unit::Bytes), at(1));
        let reached = meter.overall();
        meter.feed(Update::new(Stage::Structure, 1, 1, Unit::Pages), at(2));
        assert!(meter.overall() >= reached);
        assert_eq!(meter.stage(), Stage::Write);
    }

    #[test]
    fn the_estimate_shrinks_as_the_work_advances() {
        let start = at(0);
        let mut meter = Meter::new(&[Stage::Write], start);
        meter.feed(Update::of_part(Stage::Write, 1, 1, 100, 1000, Unit::Bytes), at(10));
        let first = meter.eta(at(10)).unwrap();
        meter.feed(Update::of_part(Stage::Write, 1, 1, 900, 1000, Unit::Bytes), at(90));
        let second = meter.eta(at(90)).unwrap();
        assert!(second < first, "the estimate should fall: {second:?} !< {first:?}");
        assert!(meter.last_eta().is_some());
    }

    #[test]
    fn nothing_is_estimated_before_the_work_has_begun() {
        let start = at(0);
        let meter = Meter::new(&[Stage::Write], start);
        assert!(meter.eta(start).is_none());
    }
}
