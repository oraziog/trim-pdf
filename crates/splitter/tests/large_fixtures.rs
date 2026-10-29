//! Runs over the synthetic benchmark fixtures from M0 (`MD/m0-fase0.md`), when they are present.
//!
//! The fixtures live under `target/benchmark`, which is not part of the repository, so every test
//! here skips when its file is missing. The 6 GB one is ignored by default: it is a real read of
//! six gigabytes, and `cargo test -- --ignored` runs it deliberately.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use splitter::{Canceller, Document, Error, Job, SplitOptions, SplitPlan, Stage, Unit, Update};

/// A fixture under `target/benchmark`, when it exists.
fn fixture(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join("target/benchmark").join(name);
    path.exists().then_some(path)
}

/// A folder of this test's own.
fn folder(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("pdfsplit-large-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn a_hundred_megabyte_fixture_reads_like_a_small_one() {
    let Some(path) = fixture("small-128mb.pdf") else { return };
    let mut document = Document::open(&path).unwrap();
    assert_eq!(document.pdf_version(), "1.7");
    assert_eq!(document.page_count().unwrap(), 2);
    let bookmarks = document.bookmarks().unwrap();
    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].page, Some(1));
}

#[test]
fn the_progress_of_a_split_moves_in_small_steps() {
    let Some(path) = fixture("small-128mb.pdf") else { return };
    let directory = folder("steps");
    let mut document = Document::open(&path).unwrap();
    let pages = document.page_count().unwrap();
    let plan = SplitPlan::equal_parts(pages, 2).unwrap();
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Update>>> = std::sync::Arc::default();
    let sink = std::sync::Arc::clone(&seen);
    let job = Job::new(move |update| sink.lock().map(|mut list| list.push(update)).unwrap_or(()));
    let options = SplitOptions { out_dir: Some(directory.clone()), ..SplitOptions::default() };
    document.split_with(&plan, &options, &job).unwrap();

    let updates: Vec<Update> = seen.lock().map(|list| list.clone()).unwrap_or_default();
    let written: Vec<u64> = updates.iter().filter(|update| update.stage == Stage::Write).map(|update| update.done).collect();
    // A 128 MB file copied four megabytes at a time: the bar moves tens of times, not once.
    assert!(written.len() >= 30, "only {} reports for 128 MB", written.len());
    assert!(written.windows(2).all(|pair| pair[0] <= pair[1]), "the bytes went backwards");
    assert!(updates.iter().any(|update| update.unit == Unit::Bytes && update.total > 0));
    for entry in std::fs::read_dir(&directory).into_iter().flatten().flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
}

#[test]
#[ignore = "reads a 6 GB fixture: run with `cargo test -- --ignored`"]
fn the_six_gigabyte_fixture_opens_without_being_loaded() {
    let Some(path) = fixture("pdfsplit-6gb.pdf") else { return };
    let mut document = Document::open(&path).unwrap();
    assert_eq!(document.file_size(), 6_200_001_228);
    assert_eq!(document.page_count().unwrap(), 2);
    let bookmarks = document.bookmarks().unwrap();
    assert_eq!(bookmarks.len(), 1);
    assert_eq!(bookmarks[0].page, Some(1));
    assert_eq!(bookmarks[0].title, "Synthetic benchmark");
}

#[test]
#[ignore = "writes part of a 6 GB fixture: run with `cargo test -- --ignored`"]
fn cancelling_a_six_gigabyte_split_stops_within_a_chunk_and_leaves_nothing() {
    let Some(path) = fixture("pdfsplit-6gb.pdf") else { return };
    let directory = folder("cancel-6gb");
    let mut document = Document::open(&path).unwrap();
    let pages = document.page_count().unwrap();
    let plan = SplitPlan::equal_parts(pages, 2).unwrap();
    // Stop as soon as the first chunk of a part has been written: this is the cancel button.
    let stop = Canceller::default();
    let hit = stop.clone();
    let job = Job::stopping(
        move |update| {
            if update.stage == Stage::Write && update.done > 0 {
                hit.cancel();
            }
        },
        stop,
    );
    let options = SplitOptions { out_dir: Some(directory.clone()), overwrite: true, ..SplitOptions::default() };
    let started = Instant::now();
    let error = document.split_with(&plan, &options, &job).unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(error, Error::Cancelled), "{error}");
    assert!(elapsed < Duration::from_secs(120), "cancelling took {elapsed:?}");
    let left: Vec<PathBuf> = std::fs::read_dir(&directory).unwrap().filter_map(|entry| entry.ok().map(|entry| entry.path())).collect();
    assert!(left.is_empty(), "a cancelled split left {left:?}");
    println!("cancelled a 6 GB split in {elapsed:?}");
    let _ = std::fs::remove_dir(&directory);
}
