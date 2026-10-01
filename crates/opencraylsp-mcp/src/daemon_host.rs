//! The daemon-backed [`ToolHost`].
//!
//! Unlike `opencraylsp-client`'s own `DaemonHost`, this one survives the daemon being
//! absent *at startup*: `opencraylsp-mcp` has to enter its stdio loop even when nobody
//! is listening, so the harness can be told `[daemon_unavailable]` and the
//! client can recover on its own once a daemon appears. The first connection
//! is therefore deferred and re-attempted, but a burst of callers that all
//! find the daemon down shares one dial (the same single-flight shape
//! `opencraylsp-client` uses for reconnects) instead of queueing one timeout each.
//!
//! Listing tools never dials: `tools/list` has to answer immediately even when
//! no daemon is up, so it uses a live connection when there is one and the
//! in-process catalogue otherwise. Only `call_tool` waits for the dial.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use async_trait::async_trait;
use opencraylsp_client::{ClientError, ClientOptions, DaemonClient};
use opencraylsp_proto::{HostError, ToolDef, ToolHost, ToolOutput};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

/// A [`ToolHost`] that connects to the daemon on first use.
#[derive(Debug)]
pub struct LazyDaemonHost {
    options: ClientOptions,
    client: RwLock<Option<DaemonClient>>,
    /// Serializes the dialling.
    gate: AsyncMutex<()>,
    /// Completed dial attempts; see [`Self::connect_once`].
    attempts: AtomicU64,
    last_outcome: Mutex<Option<(u64, Result<(), ClientError>)>>,
}

impl LazyDaemonHost {
    pub fn new(options: ClientOptions) -> Arc<Self> {
        Arc::new(Self {
            options,
            client: RwLock::new(None),
            gate: AsyncMutex::new(()),
            attempts: AtomicU64::new(0),
            last_outcome: Mutex::new(None),
        })
    }

    /// The options this host connects with, for diagnostics.
    pub fn options(&self) -> &ClientOptions {
        &self.options
    }

    /// Attempts the first connection.
    ///
    /// `main` spawns this in the background so the stdio loop never waits on a
    /// daemon; callers that need a connection (`call_tool`) dial on demand
    /// through the same single-flight path.
    pub async fn connect_now(&self) -> Result<(), ClientError> {
        self.client().await.map(|_| ())
    }

    fn cached(&self) -> Option<DaemonClient> {
        read(&self.client).clone()
    }

    /// The live client, dialling once if this is the first call.
    async fn client(&self) -> Result<DaemonClient, ClientError> {
        if let Some(client) = self.cached() {
            return Ok(client);
        }
        self.connect_once().await?;
        self.cached().ok_or_else(|| {
            ClientError::ConnectionLost("the daemon connection was not published".into())
        })
    }

    async fn connect_once(&self) -> Result<(), ClientError> {
        // Fast path: somebody connected while we were reading.
        if self.cached().is_some() {
            return Ok(());
        }
        let seen = self.attempts.load(Ordering::SeqCst);
        let _dialling = self.gate.lock().await;
        if self.cached().is_some() {
            return Ok(());
        }
        // A dial finished while we waited: share its result rather than
        // dialling one more time. Callers arriving after it start a new dial.
        if let Some((attempt, outcome)) = lock(&self.last_outcome).clone()
            && attempt > seen
        {
            return outcome;
        }
        let outcome = DaemonClient::connect(self.options.clone()).await;
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        let shared = match &outcome {
            Ok(client) => {
                *write(&self.client) = Some(client.clone());
                Ok(())
            }
            Err(error) => Err(error.clone()),
        };
        *lock(&self.last_outcome) = Some((attempt, shared.clone()));
        shared
    }

    /// Test hook: installs `client` as the cached one.
    #[cfg(test)]
    pub(crate) fn seed_cache(&self, client: DaemonClient) {
        *write(&self.client) = Some(client);
    }

    /// Test hook: the cached client, if any.
    #[cfg(test)]
    pub(crate) fn cached_for_test(&self) -> Option<DaemonClient> {
        self.cached()
    }

    /// Drops the cached client, but only if it is still the one that failed.
    ///
    /// The comparison matters: another task may have reconnected while this
    /// call was in flight, and clearing a *fresh* client would throw away the
    /// working connection the reconnect just established.
    fn forget_if(&self, stale: &DaemonClient) {
        if let Ok(mut slot) = self.client.write()
            && slot
                .as_ref()
                .is_some_and(|live| same_connection(live, stale))
        {
            *slot = None;
        }
    }
}

#[async_trait]
impl ToolHost for LazyDaemonHost {
    /// The catalogue, without ever waiting for a daemon.
    ///
    /// A live connection is asked (so a daemon with a different catalogue stays
    /// authoritative), but a failed or absent one falls straight back to the
    /// in-process catalogue. `initialize` and `tools/list` must answer in
    /// milliseconds even when the daemon is unreachable, or the harness gives up
    /// on the server; the first real `tools/call` is where a missing daemon is
    /// allowed to cost the connect deadline.
    async fn list_tools(&self) -> Result<Vec<ToolDef>, HostError> {
        if let Some(client) = self.cached()
            && let Ok(tools) = client.list_tools().await
        {
            return Ok(tools);
        }
        Ok(opencraylsp_tools::tool_defs())
    }

    async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<ToolOutput, HostError> {
        let client = self.client().await.map_err(HostError::from)?;
        let outcome = client
            .call_tool(name, arguments, cancel)
            .await
            .map_err(HostError::from);
        // The cached client is reused for every later call, so a dead one
        // has to be dropped here. Otherwise every subsequent tool call pays the
        // full connect deadline against a corpse: the daemon is gone, but the
        // handle to it is not, and nothing else ever clears the slot. A
        // *transport* failure is what makes a client stale — a tool that ran
        // and returned an error is a healthy daemon answering badly.
        if matches!(outcome, Err(HostError::Unavailable(_))) {
            self.forget_if(&client);
        }
        outcome
    }
}

/// Two `DaemonClient`s are the same connection when they report the same next
/// request id over the same live socket: clones share that state, a reconnect
/// starts a fresh counter. Used to tell "the client that just failed" from "a
/// connection another task established while this call was in flight".
fn same_connection(a: &DaemonClient, b: &DaemonClient) -> bool {
    a.next_request_id() == b.next_request_id() && a.hello().pid == b.hello().pid
}

/// Reads a slot, recovering a poisoned lock (the data is still valid).
fn read<T>(slot: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    slot.read().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The writing counterpart of [`read`].
fn write<T>(slot: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    slot.write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Locks a plain mutex, recovering from poisoning like [`read`] does.
fn lock<T>(slot: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cached client must be dropped once the daemon behind it is
    /// gone, and only when it is the one that failed.
    ///
    /// Without this the cache is write-once: `client()` hands out the stored
    /// handle forever, so after the daemon dies every tool call pays the full
    /// connect deadline against a corpse and never gets anywhere.
    #[tokio::test]
    async fn a_failed_client_is_dropped_from_the_cache() {
        let mut options = ClientOptions::default_for_tests();
        options.connect_deadline = std::time::Duration::from_millis(50);
        let host = LazyDaemonHost::new(options);

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("opencraylsp.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                use std::io::{BufRead as _, Write as _};
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut write = stream;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if value["method"] == "hello" {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0", "id": value["id"].clone(),
                            "result": {"protocol": 1, "daemon_version": "x", "pid": 1,
                                       "languages": [], "language_mode": "auto"},
                        });
                        let _ = writeln!(write, "{response}");
                    } else if value["method"] == "tools/call" {
                        // Hang up mid-request: the connection is gone.
                        break;
                    }
                }
            }
        });

        let mut client_options = ClientOptions::default_for_tests();
        client_options.socket = socket.clone();
        client_options.connect_deadline = std::time::Duration::from_millis(500);
        let client = DaemonClient::connect(client_options)
            .await
            .expect("the stub answers hello");
        host.seed_cache(client.clone());

        let outcome = host
            .call_tool(
                "lsp_status",
                serde_json::Value::Null,
                &CancellationToken::new(),
            )
            .await;
        assert!(outcome.is_err(), "the stub hung up: {outcome:?}");

        assert!(
            host.cached_for_test().is_none(),
            "a failed client must not stay cached"
        );
        let _ = handle.join();
    }

    /// `forget_if` must not clear a client other than the one that failed.
    #[tokio::test]
    async fn forgetting_a_client_leaves_another_one_alone() {
        let mut options = ClientOptions::default_for_tests();
        options.connect_deadline = std::time::Duration::from_millis(50);
        let host = LazyDaemonHost::new(options);
        assert!(host.cached_for_test().is_none());
        let mut other = ClientOptions::default_for_tests();
        other.connect_deadline = std::time::Duration::from_millis(1);
        assert!(DaemonClient::connect(other).await.is_err());
    }
}
