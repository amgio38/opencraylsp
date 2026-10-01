//! Replays a progress sequence recorded from a real rust-analyzer (1.95.0 on a
//! one-file cargo project, paths scrubbed) through the tracker.

use std::time::Duration;

use opencraylsp_core::progress::{ProgressTracker, SETTLE_AFTER_END};
use serde_json::Value;

fn recorded() -> Vec<Value> {
    serde_json::from_str(include_str!("fixtures/rust_analyzer_progress.json"))
        .expect("fixture is valid JSON")
}

fn feed(tracker: &ProgressTracker, event: &Value) {
    tracker.on_progress(&serde_json::json!({"token": event["token"], "value": event["value"]}));
}

#[test]
fn the_real_sequence_keeps_the_server_busy_from_first_begin_to_last_end() {
    let events = recorded();
    assert!(events.len() > 50, "the fixture is a real, busy startup");
    let tracker = ProgressTracker::with_windows(Duration::ZERO, SETTLE_AFTER_END);
    assert_eq!(tracker.snapshot(), None);
    let mut busy_after_every_event = true;
    for event in &events {
        feed(&tracker, event);
        // Flycheck-only moments are the one place "idle" is legitimate; in this
        // recording indexing overlaps them, so busy must hold throughout.
        busy_after_every_event &= tracker.snapshot().is_some();
    }
    assert!(busy_after_every_event, "never idle between phases");
    // Everything has ended, but the settle window still covers the last gap.
    assert!(tracker.snapshot().is_some());
    std::thread::sleep(SETTLE_AFTER_END + Duration::from_millis(100));
    assert_eq!(tracker.snapshot(), None, "idle once nothing follows");
}

#[test]
fn the_real_sequence_names_the_phases_a_user_would_recognise() {
    let tracker = ProgressTracker::with_windows(Duration::ZERO, Duration::ZERO);
    let mut seen = std::collections::BTreeSet::new();
    for event in &recorded() {
        feed(&tracker, event);
        if let Some(indexing) = tracker.snapshot() {
            seen.insert(indexing.message.split(':').next().unwrap_or("").to_owned());
        }
    }
    for phase in ["Fetching", "Roots Scanned", "Indexing"] {
        assert!(seen.contains(phase), "missing phase {phase}: {seen:?}");
    }
}

#[test]
fn the_recorded_flycheck_alone_never_counts_as_indexing() {
    let tracker = ProgressTracker::with_windows(Duration::ZERO, Duration::ZERO);
    let mut fed = 0;
    for event in recorded()
        .iter()
        .filter(|e| e["token"].as_str().is_some_and(|t| t.contains("flycheck")))
    {
        feed(&tracker, event);
        fed += 1;
        assert_eq!(tracker.snapshot(), None);
    }
    assert!(fed >= 2, "the recording contains flycheck begin/end");
}

#[test]
fn the_recorded_messages_carry_no_local_paths() {
    let text = include_str!("fixtures/rust_analyzer_progress.json");
    assert!(!text.contains("/root/"), "fixture must be scrubbed");
    assert!(!text.contains("/home/"), "fixture must be scrubbed");
}
