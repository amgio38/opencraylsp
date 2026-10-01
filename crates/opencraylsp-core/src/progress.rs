//! Work-done progress tracking: is this server still indexing?
//!
//! Servers announce long-running work with `window/workDoneProgress/create`
//! and then stream `$/progress` notifications (`begin`, `report`, `end`)
//! under a token. While any such work is active the server's answers are not
//! to be trusted — an empty `references` list may simply mean "not indexed
//! yet" — so the pool reports [`Indexing`] instead of an empty result.
//!
//! Two refinements keep this honest without crying wolf:
//! * background *check* runs (rust-analyzer's `flycheck`, `cargo check`) are
//!   diagnostics work, not indexing, and are ignored;
//! * a token that never ends (a buggy server) stops counting after
//!   [`STALE_AFTER`] without an update.
//!
//! A freshly started server has not announced anything yet, so for a short
//! grace period after start a server that has not yet reported any progress is
//! treated as still starting up.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use opencraylsp_proto::Indexing;
use serde_json::Value;

/// A token with no update for this long stops counting as active work.
pub const STALE_AFTER: Duration = Duration::from_secs(120);

/// How long a just-started server that has reported no progress yet is still
/// treated as starting up.
pub const STARTUP_GRACE: Duration = Duration::from_secs(3);

/// After the last active token ends the server still counts as busy for this
/// long. Servers announce their phases one after another with gaps of a few
/// tens of milliseconds (measured on rust-analyzer: up to 57 ms between
/// `Fetching` and `Building CrateGraph`); without this window a request landing
/// in such a gap would be trusted although the next phase is about to begin.
pub const SETTLE_AFTER_END: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
struct Active {
    title: String,
    message: Option<String>,
    percent: Option<u32>,
    updated: Instant,
}

#[derive(Debug)]
struct State {
    active: HashMap<String, Active>,
    started: Instant,
    /// What the server was last busy with and when that changed, for the
    /// [`SETTLE_AFTER_END`] window.
    last_busy: Option<(Indexing, Instant)>,
    /// Any `begin` seen since the last reset: after this the grace period no
    /// longer applies, because the server has shown it does report progress.
    seen_progress: bool,
}

/// See the module docs.
#[derive(Debug)]
pub struct ProgressTracker {
    state: Mutex<State>,
    grace: Duration,
    settle: Duration,
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new(STARTUP_GRACE)
    }
}

/// Whether `token`/`title` name a background diagnostics run rather than
/// indexing.
fn is_background_check(token: &str, title: &str) -> bool {
    let token = token.to_ascii_lowercase();
    let title = title.to_ascii_lowercase();
    token.contains("flycheck")
        || title.contains("flycheck")
        || title.starts_with("cargo check")
        || title.starts_with("cargo clippy")
}

fn token_key(token: &Value) -> Option<String> {
    match token {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

impl ProgressTracker {
    /// A tracker whose startup grace period is `grace` and whose end-of-work
    /// window is [`SETTLE_AFTER_END`].
    pub fn new(grace: Duration) -> Self {
        Self::with_windows(grace, SETTLE_AFTER_END)
    }

    /// A tracker with explicit startup-grace and end-of-work windows.
    pub fn with_windows(grace: Duration, settle: Duration) -> Self {
        Self {
            state: Mutex::new(State {
                active: HashMap::new(),
                started: Instant::now(),
                last_busy: None,
                seen_progress: false,
            }),
            grace,
            settle,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Forgets everything and restarts the grace period (the server was just
    /// (re)started).
    pub fn reset(&self) {
        let mut state = self.state();
        state.active.clear();
        state.started = Instant::now();
        state.last_busy = None;
        state.seen_progress = false;
    }

    /// Starts the grace period now, keeping any progress already reported (the
    /// `initialize` handshake finished; the server is ready for requests).
    pub fn mark_ready(&self) {
        self.state().started = Instant::now();
    }

    /// Forgets active work without restarting the grace period (the server
    /// stopped).
    pub fn clear(&self) {
        let mut state = self.state();
        state.active.clear();
        state.last_busy = None;
        state.seen_progress = false;
        state.started = Instant::now() - self.grace - Duration::from_secs(1);
    }

    /// Handles one `$/progress` notification. Malformed payloads are ignored:
    /// a server bug must never poison the tracker.
    pub fn on_progress(&self, params: &Value) {
        let Some(token) = params.get("token").and_then(token_key) else {
            tracing::debug!("lsp: $/progress without a usable token ignored");
            return;
        };
        let Some(value) = params.get("value") else {
            return;
        };
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let percent = value
            .get("percentage")
            .and_then(Value::as_u64)
            .map(|p| p.min(100) as u32);
        let mut state = self.state();
        match value.get("kind").and_then(Value::as_str) {
            Some("begin") => {
                let title = text("title").unwrap_or_default();
                state.seen_progress = true;
                if is_background_check(&token, &title) {
                    return;
                }
                state.active.insert(
                    token,
                    Active {
                        title,
                        message: text("message"),
                        percent,
                        updated: Instant::now(),
                    },
                );
            }
            Some("report") => {
                if let Some(active) = state.active.get_mut(&token) {
                    if let Some(message) = text("message") {
                        active.message = Some(message);
                    }
                    if percent.is_some() {
                        active.percent = percent;
                    }
                    active.updated = Instant::now();
                }
            }
            Some("end") => {
                // Remember what just finished, described from the token that
                // ended rather than from a placeholder: during the settle
                // window this is the only thing the model is told, and
                // "finishing" with no percentage says strictly less than the
                // phase the server actually named.
                if let Some(finished) = state.active.remove(&token)
                    && state.active.is_empty()
                {
                    let described = Self::describe(std::slice::from_ref(&finished));
                    state.last_busy = Some((described, Instant::now()));
                }
            }
            _ => {}
        }
    }

    fn describe(actives: &[Active]) -> Indexing {
        let latest = actives
            .iter()
            .max_by_key(|a| a.updated)
            .expect("describe needs at least one active token");
        let message = match &latest.message {
            Some(detail) if !latest.title.is_empty() => format!("{}: {detail}", latest.title),
            Some(detail) => detail.clone(),
            None if !latest.title.is_empty() => latest.title.clone(),
            None => "working".to_owned(),
        };
        Indexing {
            message,
            percent: latest.percent,
        }
    }

    /// The current indexing state, or `None` when the server is idle.
    ///
    /// Stale tokens are dropped first. With work active, the most recently
    /// updated token describes it; right after the last token ended the server
    /// still counts as busy for the settle window; and a server that has shown
    /// no progress yet and started less than the grace period ago counts as
    /// starting up.
    pub fn snapshot(&self) -> Option<Indexing> {
        let mut state = self.state();
        state
            .active
            .retain(|_, active| active.updated.elapsed() < STALE_AFTER);
        if !state.active.is_empty() {
            let actives: Vec<Active> = state.active.values().cloned().collect();
            let described = Self::describe(&actives);
            state.last_busy = Some((described.clone(), Instant::now()));
            return Some(described);
        }
        if let Some((described, since)) = &state.last_busy
            && since.elapsed() < self.settle
        {
            return Some(described.clone());
        }
        if !state.seen_progress && state.started.elapsed() < self.grace {
            return Some(Indexing {
                message: "language server is starting up".to_owned(),
                percent: None,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A tracker that is past its startup grace, so only progress counts.
    fn settled() -> ProgressTracker {
        ProgressTracker::with_windows(Duration::ZERO, Duration::ZERO)
    }

    fn begin(t: &ProgressTracker, token: Value, title: &str, pct: Option<u64>) {
        let mut value = json!({"kind": "begin", "title": title});
        if let Some(pct) = pct {
            value["percentage"] = json!(pct);
        }
        t.on_progress(&json!({"token": token, "value": value}));
    }

    fn end(t: &ProgressTracker, token: Value) {
        t.on_progress(&json!({"token": token, "value": {"kind": "end"}}));
    }

    #[test]
    fn begin_report_end_walks_through_the_states() {
        let t = settled();
        assert_eq!(t.snapshot(), None);
        begin(&t, json!("ra/roots"), "Roots Scanned", Some(10));
        assert_eq!(
            t.snapshot(),
            Some(Indexing {
                message: "Roots Scanned".into(),
                percent: Some(10)
            })
        );
        t.on_progress(&json!({"token": "ra/roots",
            "value": {"kind": "report", "message": "3/10", "percentage": 30}}));
        assert_eq!(
            t.snapshot(),
            Some(Indexing {
                message: "Roots Scanned: 3/10".into(),
                percent: Some(30)
            })
        );
        end(&t, json!("ra/roots"));
        assert_eq!(t.snapshot(), None);
    }

    #[test]
    fn several_tokens_keep_the_server_busy_until_all_end() {
        let t = settled();
        begin(&t, json!(1), "Fetching", None);
        std::thread::sleep(Duration::from_millis(2));
        begin(&t, json!(2), "Indexing", Some(5));
        // The most recently updated token describes the state.
        assert_eq!(t.snapshot().unwrap().message, "Indexing");
        end(&t, json!(2));
        assert_eq!(t.snapshot().unwrap().message, "Fetching");
        end(&t, json!(1));
        assert_eq!(t.snapshot(), None);
    }

    #[test]
    fn numeric_and_string_tokens_are_both_accepted() {
        let t = settled();
        begin(&t, json!(7), "A", None);
        begin(&t, json!("seven"), "B", None);
        end(&t, json!(7));
        assert_eq!(t.snapshot().unwrap().message, "B");
    }

    #[test]
    fn flycheck_and_cargo_check_are_not_indexing() {
        let t = settled();
        begin(&t, json!("rustAnalyzer/flycheck/0"), "cargo check", None);
        begin(&t, json!("x"), "cargo clippy", None);
        begin(&t, json!("y"), "Flycheck run", None);
        assert_eq!(t.snapshot(), None);
    }

    #[test]
    fn malformed_payloads_are_ignored_without_panicking() {
        let t = settled();
        for bad in [
            json!(null),
            json!({}),
            json!({"token": null, "value": {"kind": "begin"}}),
            json!({"token": [1], "value": {"kind": "begin"}}),
            json!({"token": "t"}),
            json!({"token": "t", "value": {}}),
            json!({"token": "t", "value": {"kind": 5}}),
            json!({"token": "t", "value": {"kind": "explode"}}),
            json!({"token": "t", "value": {"kind": "report", "message": "x"}}),
        ] {
            t.on_progress(&bad);
        }
        assert_eq!(t.snapshot(), None);
    }

    #[test]
    fn begin_without_a_title_still_counts() {
        let t = settled();
        t.on_progress(&json!({"token": "t", "value": {"kind": "begin"}}));
        assert_eq!(t.snapshot().unwrap().message, "working");
        t.on_progress(&json!({"token": "t",
            "value": {"kind": "report", "message": "step 2"}}));
        assert_eq!(t.snapshot().unwrap().message, "step 2");
    }

    #[test]
    fn percentages_are_clamped_and_kept_across_reports_without_one() {
        let t = settled();
        begin(&t, json!("t"), "T", Some(250));
        assert_eq!(t.snapshot().unwrap().percent, Some(100));
        t.on_progress(&json!({"token": "t", "value": {"kind": "report", "message": "m"}}));
        assert_eq!(t.snapshot().unwrap().percent, Some(100));
    }

    #[test]
    fn a_token_that_never_ends_stops_counting_after_the_stale_limit() {
        let t = settled();
        begin(&t, json!("stuck"), "Stuck", None);
        {
            let mut state = t.state();
            state.active.get_mut("stuck").unwrap().updated =
                Instant::now() - STALE_AFTER - Duration::from_secs(1);
        }
        assert_eq!(t.snapshot(), None);
        // ...and it was dropped, not merely hidden.
        assert!(t.state().active.is_empty());
    }

    #[test]
    fn a_report_refreshes_the_stale_clock() {
        let t = settled();
        begin(&t, json!("t"), "T", None);
        {
            let mut state = t.state();
            state.active.get_mut("t").unwrap().updated =
                Instant::now() - STALE_AFTER + Duration::from_secs(1);
        }
        t.on_progress(&json!({"token": "t", "value": {"kind": "report", "message": "m"}}));
        assert!(t.snapshot().is_some());
    }

    #[test]
    fn a_fresh_server_is_starting_up_until_the_grace_ends_or_progress_appears() {
        let t = ProgressTracker::new(Duration::from_millis(80));
        assert_eq!(
            t.snapshot().unwrap().message,
            "language server is starting up"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(t.snapshot(), None, "grace over, nothing reported");

        let t = ProgressTracker::with_windows(Duration::from_secs(60), Duration::ZERO);
        begin(&t, json!("t"), "Indexing", None);
        end(&t, json!("t"));
        assert_eq!(
            t.snapshot(),
            None,
            "once the server has shown progress, quiet means idle"
        );
    }

    #[test]
    fn a_background_check_begin_also_ends_the_grace() {
        let t = ProgressTracker::with_windows(Duration::from_secs(60), Duration::ZERO);
        begin(&t, json!("rust-analyzer/flycheck/0"), "cargo check", None);
        assert_eq!(t.snapshot(), None);
    }

    #[test]
    fn reset_restarts_the_grace_and_forgets_work() {
        let t = ProgressTracker::with_windows(Duration::from_secs(60), Duration::ZERO);
        begin(&t, json!("t"), "T", None);
        end(&t, json!("t"));
        assert_eq!(t.snapshot(), None);
        t.reset();
        assert_eq!(
            t.snapshot().unwrap().message,
            "language server is starting up"
        );
    }

    #[test]
    fn mark_ready_restarts_the_grace_but_keeps_reported_work() {
        let t = ProgressTracker::with_windows(Duration::from_secs(60), Duration::ZERO);
        begin(&t, json!("t"), "Indexing", None);
        t.mark_ready();
        assert_eq!(t.snapshot().unwrap().message, "Indexing");
        end(&t, json!("t"));
        assert_eq!(t.snapshot(), None, "progress was seen, so no grace applies");

        let quiet = ProgressTracker::with_windows(Duration::from_secs(60), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(5));
        quiet.mark_ready();
        assert_eq!(
            quiet.snapshot().unwrap().message,
            "language server is starting up"
        );
    }

    #[test]
    fn clear_drops_work_and_ends_the_grace() {
        let t = ProgressTracker::with_windows(Duration::from_secs(60), Duration::ZERO);
        begin(&t, json!("t"), "T", None);
        t.clear();
        assert_eq!(t.snapshot(), None);
    }

    #[test]
    fn default_uses_the_documented_windows() {
        let t = ProgressTracker::default();
        assert_eq!(t.grace, STARTUP_GRACE);
        assert_eq!(t.settle, SETTLE_AFTER_END);
    }

    #[test]
    fn the_server_stays_busy_through_the_gap_between_two_phases() {
        // rust-analyzer ends `Fetching` and begins `Building CrateGraph` up to
        // ~57 ms later; a request in that gap must not be trusted.
        let t = ProgressTracker::with_windows(Duration::ZERO, Duration::from_millis(150));
        begin(&t, json!("a"), "Fetching", None);
        end(&t, json!("a"));
        assert!(t.snapshot().is_some(), "inside the settle window");
        std::thread::sleep(Duration::from_millis(60));
        begin(&t, json!("b"), "Building CrateGraph", None);
        assert_eq!(t.snapshot().unwrap().message, "Building CrateGraph");
        end(&t, json!("b"));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(t.snapshot(), None, "the window closes when nothing follows");
    }

    /// The settle window is there because the *next* phase may follow, not to
    /// replace what the server last said. A request landing in the window must
    /// still be told which phase just finished and how far it got — that is the
    /// only information the model has about whether to retry now or in a while.
    #[test]
    fn the_settle_window_reports_what_just_finished_not_a_placeholder() {
        let t = ProgressTracker::with_windows(Duration::ZERO, Duration::from_secs(60));
        begin(&t, json!("a"), "cachePriming", None);
        t.on_progress(&json!({"token": "a",
                "value": {"kind": "report", "message": "Indexing", "percentage": 90}}));
        assert_eq!(
            t.snapshot().unwrap(),
            Indexing {
                message: "cachePriming: Indexing".into(),
                percent: Some(90)
            }
        );
        end(&t, json!("a"));
        // Still busy, but honest about what: the real last message and percent,
        // not a synthesised placeholder.
        assert_eq!(
            t.snapshot(),
            Some(Indexing {
                message: "cachePriming: Indexing".into(),
                percent: Some(90)
            }),
            "the settle window kept a placeholder instead of the real phase"
        );
    }

    /// The same window, for the shape a server sends most often: no percentage
    /// at all. The message must still be the real one.
    #[test]
    fn the_settle_window_keeps_the_last_message_when_there_was_no_percentage() {
        let t = ProgressTracker::with_windows(Duration::ZERO, Duration::from_secs(60));
        begin(&t, json!("a"), "Roots Scanned", None);
        end(&t, json!("a"));
        assert_eq!(
            t.snapshot().unwrap().message,
            "Roots Scanned",
            "the settle window lost the phase the server last named"
        );
    }

    #[test]
    fn ending_an_unknown_token_does_not_open_a_window() {
        let t = ProgressTracker::with_windows(Duration::ZERO, Duration::from_secs(60));
        end(&t, json!("never-began"));
        assert_eq!(t.snapshot(), None);
    }

    #[test]
    fn ending_one_of_two_tokens_keeps_reporting_the_other_not_a_window() {
        let t = ProgressTracker::with_windows(Duration::ZERO, Duration::from_secs(60));
        begin(&t, json!("a"), "A", None);
        begin(&t, json!("b"), "B", None);
        end(&t, json!("a"));
        assert_eq!(t.snapshot().unwrap().message, "B");
    }
}
