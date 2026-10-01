//! In-process [`ToolHost`] doubles.
//!
//! Two consumers: the `#[tokio::test]` suite (always) and the hidden
//! `--fake-host` flag of the binary, which is compiled in only with the
//! `test-fake-host` feature so a release build never carries it.
//!
//! The fake is deliberately controllable per test: a test can pin the latency
//! of one tool, park another behind a gate (to keep a request in flight),
//! make `list_tools` fail, or make `call_tool` fail. It is never more
//! permissive than the real hosts: a gated call still returns whatever the
//! test told it to, and a failing tool still reports `is_error = true`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use opencraylsp_proto::{HostError, ToolDef, ToolHost, ToolOutput};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// How a scripted tool call should end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The tool ran and failed: an error [`ToolOutput`], not a protocol error.
    Tool(String),
    /// The host itself could not run the tool.
    Host(HostError),
}

/// A latch a test can hold a tool call open on.
#[derive(Default)]
pub struct Gate {
    open: AtomicBool,
    notify: Notify,
    arrivals: AtomicUsize,
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("open", &self.open.load(Ordering::SeqCst))
            .field("arrivals", &self.arrivals.load(Ordering::SeqCst))
            .finish()
    }
}

impl Gate {
    /// A closed gate: waiters block until [`Gate::open`] is called.
    pub fn closed() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Let every waiter proceed.
    pub fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Number of calls currently parked on this gate.
    pub fn arrivals(&self) -> usize {
        self.arrivals.load(Ordering::SeqCst)
    }
}

impl Gate {
    async fn park(&self) {
        self.arrivals.fetch_add(1, Ordering::SeqCst);
        loop {
            let notified = self.notify.notified();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

/// How many calls reached each tool, in order.
#[derive(Debug, Clone)]
pub struct CallLog {
    pub name: String,
    pub arguments: Value,
    /// Whether the cancellation token was already triggered on arrival.
    pub cancelled_on_entry: bool,
}

/// A scriptable [`ToolHost`].
#[derive(Debug, Default)]
pub struct FakeHost {
    tools: Mutex<Vec<ToolDef>>,
    calls: Mutex<Vec<CallLog>>,
    /// Tools whose reply is produced by [`Failure`] instead of the default.
    failures: Mutex<HashMap<String, Failure>>,
    /// Tools parked on a gate before replying.
    gates: Mutex<HashMap<String, Arc<Gate>>>,
    /// Artificial per-tool latency, in milliseconds.
    delays_ms: Mutex<HashMap<String, u64>>,
    /// When set, `list_tools` fails with this [`HostError`].
    list_failure: Mutex<Option<HostError>>,
    /// Number of `call_tool` invocations that returned `Cancelled`.
    cancellations: AtomicUsize,
    /// When set, a parked call returns `Cancelled` as soon as its token fires
    /// instead of waiting for the gate.
    cancel_releases_gated: AtomicBool,
    /// Counts `list_tools` calls, so tests can prove a catalogue is cached.
    list_count: AtomicUsize,
    /// When set, a cancelled call still answers normally. Models a host that
    /// only learns about the cancellation on the next round trip, which is how
    /// the protocol layer's own suppression gets tested.
    ignores_cancellation: AtomicBool,
}

impl FakeHost {
    /// A host advertising the two tools the tests need.
    pub fn with_default_tools() -> Arc<Self> {
        let host = Arc::new(Self::default());
        host.add_tool(tool(
            "lsp_status",
            "Report daemon and instance state.",
            json!({"type": "object", "properties": {}}),
        ));
        host.add_tool(tool(
            "lsp_definition",
            "Jump to the definition of a symbol.",
            json!({
                "type": "object",
                "properties": {"symbol": {"type": "string"}},
                "required": ["symbol"]
            }),
        ));
        host
    }

    /// An empty host: no tools advertised at all.
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn add_tool(&self, def: ToolDef) {
        self.tools.lock().unwrap().push(def);
    }

    pub fn tools(&self) -> Vec<ToolDef> {
        self.tools.lock().unwrap().clone()
    }

    /// Make `name` fail as scripted.
    pub fn fail(&self, name: &str, failure: Failure) {
        self.failures
            .lock()
            .unwrap()
            .insert(name.to_owned(), failure);
    }

    /// Park calls to `name` on a fresh closed gate.
    pub fn gate(&self, name: &str) -> Arc<Gate> {
        let gate = Gate::closed();
        self.gates
            .lock()
            .unwrap()
            .insert(name.to_owned(), gate.clone());
        gate
    }

    /// Add `ms` of artificial latency to `name`.
    pub fn delay(&self, name: &str, ms: u64) {
        self.delays_ms.lock().unwrap().insert(name.to_owned(), ms);
    }

    /// Make `list_tools` fail.
    pub fn fail_list(&self, error: HostError) {
        *self.list_failure.lock().unwrap() = Some(error);
    }

    /// Let `list_tools` succeed again.
    pub fn heal_list(&self) {
        *self.list_failure.lock().unwrap() = None;
    }

    /// How many times `list_tools` was called.
    pub fn list_count(&self) -> usize {
        self.list_count.load(Ordering::SeqCst)
    }

    /// Cancelled calls stop waiting on their gate and return right away.
    pub fn set_cancel_releases_gated(&self, value: bool) {
        self.cancel_releases_gated.store(value, Ordering::SeqCst);
    }

    /// Cancelled calls answer as if nothing happened.
    pub fn set_ignores_cancellation(&self, value: bool) {
        self.ignores_cancellation.store(value, Ordering::SeqCst);
    }

    pub fn calls(&self) -> Vec<CallLog> {
        self.calls.lock().unwrap().clone()
    }

    pub fn call_count(&self, name: &str) -> usize {
        self.calls().iter().filter(|c| c.name == name).count()
    }

    /// How many calls returned `Cancelled`.
    pub fn cancellations(&self) -> usize {
        self.cancellations.load(Ordering::SeqCst)
    }
}

/// A tool definition with the project's default annotations.
pub fn tool(name: &str, description: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema,
        annotations: Default::default(),
    }
}

#[async_trait]
impl ToolHost for FakeHost {
    async fn list_tools(&self) -> Result<Vec<ToolDef>, HostError> {
        self.list_count.fetch_add(1, Ordering::SeqCst);
        if let Some(err) = self.list_failure.lock().unwrap().clone() {
            return Err(err);
        }
        Ok(self.tools())
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, HostError> {
        self.calls.lock().unwrap().push(CallLog {
            name: name.to_owned(),
            arguments,
            cancelled_on_entry: cancel.is_cancelled(),
        });

        let delay = self
            .delays_ms
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or(0);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }

        let gate = self.gates.lock().unwrap().get(name).cloned();
        if let Some(gate) = gate {
            if self.cancel_releases_gated.load(Ordering::SeqCst) {
                // Race the gate against cancellation: whichever arrives first
                // decides, so tests can be deterministic either way.
                tokio::select! {
                    _ = gate.park() => {}
                    _ = cancel.cancelled() => {
                        self.cancellations.fetch_add(1, Ordering::SeqCst);
                        return Err(HostError::Cancelled);
                    }
                }
            } else {
                gate.park().await;
            }
        }

        if cancel.is_cancelled() && !self.ignores_cancellation.load(Ordering::SeqCst) {
            self.cancellations.fetch_add(1, Ordering::SeqCst);
            return Err(HostError::Cancelled);
        }

        match self.failures.lock().unwrap().get(name).cloned() {
            Some(Failure::Tool(text)) => Ok(ToolOutput::error(text)),
            Some(Failure::Host(err)) => Err(err),
            None => Ok(ToolOutput::ok(format!("{name} ran"))),
        }
    }
}

/// Wait until `predicate` holds, polling `check` until it does.
pub async fn wait_until(timeout: std::time::Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return check();
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::McpServer;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test]
    async fn a_gated_call_parks_until_the_gate_opens() {
        let host = FakeHost::with_default_tools();
        let gate = host.gate("lsp_status");
        let token = CancellationToken::new();
        let task = tokio::spawn({
            let host = host.clone();
            async move { host.call_tool("lsp_status", json!({}), &token).await }
        });
        assert!(wait_until(Duration::from_secs(2), || gate.arrivals() == 1).await);
        gate.open();
        let out = task.await.unwrap().unwrap();
        assert_eq!(out.text, "lsp_status ran");
    }

    #[tokio::test]
    async fn list_tools_reports_a_scripted_failure() {
        let host = FakeHost::with_default_tools();
        host.fail_list(HostError::Unavailable("down".into()));
        assert!(matches!(
            host.list_tools().await,
            Err(HostError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn wait_until_reports_a_condition_that_never_holds() {
        assert!(!wait_until(Duration::from_millis(20), || false).await);
    }

    #[test]
    fn debug_output_names_the_type() {
        assert!(format!("{:?}", Gate::closed()).starts_with("Gate"));
        assert!(format!("{:?}", McpServer::new(FakeHost::empty(), "1.2.3")).contains("1.2.3"));
    }
}
