//! The window of the splitting app: what the user picks, what the work does with it, and how far
//! the work has come.
//!
//! The window never touches a PDF itself. It fills a [`work::Setup`], hands it to a thread, and
//! draws what that thread reports: [`meter::Meter`] turns a job's phase reports into one bar and
//! an estimate, and [`work::Event`] brings back the parts or the reason there are none.
//!
//! The window is built so its whole state machine can be driven without a screen: `App::ui` takes
//! an [`egui::Context`], and a test can run frames on a bare one.

pub mod app;
pub mod meter;
pub mod work;

use splitter::Stage;

/// What the window calls the phases of a job.
pub fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Open => "Apertura del file",
        Stage::Structure => "Analisi di pagine e segnalibri",
        Stage::Search => "Lettura del testo",
        Stage::Plan => "Calcolo delle parti",
        Stage::Write => "Scrittura delle parti",
        Stage::Verify => "Verifica delle parti",
    }
}

/// A size a person reads: `6.2 GiB`, `412 KiB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10), ("B", 1)];
    for (unit, size) in UNITS {
        if bytes >= size {
            return if unit == "B" { format!("{bytes} B") } else { format!("{:.1} {unit}", bytes as f64 / size as f64) };
        }
    }
    "0 B".to_string()
}

/// A stretch of time as a person reads it: `12 s`, `1 min 20 s`, `2 h 05 min`.
pub fn human_duration(time: std::time::Duration) -> String {
    let seconds = time.as_secs();
    match seconds {
        0 => "meno di un secondo".to_string(),
        1..=59 => format!("{seconds} s"),
        60..=3599 => {
            let (minutes, rest) = (seconds / 60, seconds % 60);
            if rest == 0 { format!("{minutes} min") } else { format!("{minutes} min {rest:02} s") }
        }
        _ => {
            let (hours, rest) = (seconds / 3600, (seconds % 3600) / 60);
            format!("{hours} h {rest:02} min")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_size_is_shown_in_the_unit_that_fits() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(4096), "4.0 KiB");
        assert_eq!(human_size(6_200_001_228), "5.8 GiB");
    }

    #[test]
    fn a_time_is_shown_the_way_a_person_says_it() {
        use std::time::Duration;
        assert_eq!(human_duration(Duration::from_millis(400)), "meno di un secondo");
        assert_eq!(human_duration(Duration::from_secs(12)), "12 s");
        assert_eq!(human_duration(Duration::from_secs(80)), "1 min 20 s");
        assert_eq!(human_duration(Duration::from_secs(7200)), "2 h 00 min");
    }

    #[test]
    fn every_phase_has_a_name_a_person_understands() {
        for stage in [Stage::Open, Stage::Structure, Stage::Search, Stage::Plan, Stage::Write, Stage::Verify] {
            assert!(!stage_label(stage).is_empty());
        }
    }
}
