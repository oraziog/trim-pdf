//! What a long job tells the interface: the phase it is in, how far it has come, and whether the
//! user has asked it to stop.
//!
//! A [`Job`] is a reporter plus a stop flag. Every long loop — a page read, a four-megabyte copy —
//! asks it first whether the work is still wanted, and tells it how it is going. The reporter runs
//! on the thread doing the work, so it must not block: the window only puts the numbers into a
//! queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::Error;

/// The phases of a job, in the order they happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Reading the header and the cross-reference data.
    Open,
    /// Walking the page tree and the outline.
    Structure,
    /// Reading the text of the pages, for a search or a name taken from it.
    Search,
    /// Planning the parts: what each one holds and what it is called.
    Plan,
    /// Writing a part, stream by stream.
    Write,
    /// Reading each written part back and counting its pages.
    Verify,
}

/// What the numbers of an [`Update`] count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Pages,
    Parts,
    Bytes,
}

/// One report of a job's progress. Every number is a count of one [`Unit`], never a percentage:
/// the interface decides how to weigh the phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Update {
    pub stage: Stage,
    pub done: u64,
    pub total: u64,
    pub unit: Unit,
    /// Which part is being written or checked, 1-based. 0 when the phase is not per part.
    pub part: usize,
    /// How many parts the job writes. 0 when unknown or not applicable.
    pub parts: usize,
}

impl Update {
    /// A report for a whole phase.
    pub fn new(stage: Stage, done: u64, total: u64, unit: Unit) -> Self {
        Self { stage, done, total, unit, part: 0, parts: 0 }
    }

    /// A report for one part of a phase.
    pub fn of_part(stage: Stage, part: usize, parts: usize, done: u64, total: u64, unit: Unit) -> Self {
        Self { stage, done, total, unit, part, parts }
    }

    /// How far this phase has come, from 0 to 1. A phase with nothing to do is done.
    #[must_use]
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32
    }
}

/// A handle that stops a job from another thread: the window keeps one, the work keeps another.
#[derive(Clone, Debug, Default)]
pub struct Canceller(Arc<AtomicBool>);

impl Canceller {
    /// Ask the job to stop at its next checkpoint. Already written temporary files are removed.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// True once [`Canceller::cancel`] has been called.
    #[must_use]
    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// What the work reports on, and the flag it looks at.
type Sink = dyn Fn(Update) + Send + Sync;

/// A job in progress: where to report, and the stop flag the interface sets.
pub struct Job {
    stop: Canceller,
    sink: Box<Sink>,
}

impl Job {
    /// A job that reports through `report`, called on the thread doing the work.
    pub fn new(report: impl Fn(Update) + Send + Sync + 'static) -> Self {
        Self::stopping(report, Canceller::default())
    }

    /// The same, with a stop handle of your own: a caller that also wants to stop the job from
    /// inside the reporter — or from a second thread — keeps one and hands this one over.
    pub fn stopping(report: impl Fn(Update) + Send + Sync + 'static, stop: Canceller) -> Self {
        Self { stop, sink: Box::new(report) }
    }

    /// A job nobody is watching: no reports, never stopped. What a plain call uses.
    pub fn silent() -> Self {
        Self::new(|_| {})
    }

    /// The stop handle, to keep where the interface can reach it while the work runs elsewhere.
    #[must_use]
    pub fn handle(&self) -> Canceller {
        self.stop.clone()
    }

    /// True when the interface has asked to stop.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.stop.cancelled()
    }

    /// Stop here if the interface asked to.
    pub(crate) fn check(&self) -> Result<(), Error> {
        if self.stopped() { Err(Error::Cancelled) } else { Ok(()) }
    }

    /// Report a whole phase.
    pub(crate) fn phase(&self, stage: Stage, done: u64, total: u64, unit: Unit) {
        (self.sink)(Update::new(stage, done, total, unit));
    }

    /// Report where one part of a phase has come to.
    pub(crate) fn part(&self, stage: Stage, part: usize, parts: usize, done: u64, total: u64, unit: Unit) {
        (self.sink)(Update::of_part(stage, part, parts, done, total, unit));
    }
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job").field("stopped", &self.stopped()).finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_phase_with_nothing_to_do_is_done() {
        assert!((Update::new(Stage::Plan, 0, 0, Unit::Parts).fraction() - 1.0).abs() < f32::EPSILON);
        assert!((Update::new(Stage::Write, 3, 4, Unit::Parts).fraction() - 0.75).abs() < f32::EPSILON);
        // A job whose parts grew past the source still asks for a bar that stops at the end.
        assert_eq!(Update::new(Stage::Write, 9, 4, Unit::Bytes).fraction(), 1.0);
    }

    #[test]
    fn cancelling_reaches_every_handle() {
        let job = Job::silent();
        let handle = job.handle();
        assert!(!job.stopped());
        assert!(job.check().is_ok());
        handle.cancel();
        assert!(job.stopped());
        assert!(matches!(job.check(), Err(Error::Cancelled)));
        assert!(handle.cancelled());
    }

    #[test]
    fn the_report_arrives_on_the_working_thread() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let job = Job::new(move |update| {
            let _ = sender.send(update);
        });
        job.part(Stage::Write, 2, 3, 10, 20, Unit::Bytes);
        let update = receiver.try_recv().unwrap();
        assert_eq!(update.part, 2);
        assert_eq!(update.parts, 3);
        assert!((update.fraction() - 0.5).abs() < f32::EPSILON);
    }
}
