//! Live measurement against a real rust-analyzer . Not part of the
//! normal run: it needs the toolchain, a big workspace and minutes of CPU.
//!
//! ```text
//! OPENCRAYLSP_LIVE_WS=/path/to/rust/workspace OPENCRAYLSP_LIVE_QUERY=some_symbol \
//!   cargo test -p opencraylsp-core --features testing --test live_measure -- --ignored --nocapture
//! ```

#![allow(clippy::print_stderr)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opencraylsp_core::{LanguageSelection, LspBackend, LspConfig, LspError, Pool};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn mib(bytes: Option<u64>) -> String {
    bytes.map_or("-".to_owned(), |b| format!("{} MiB", b / (1024 * 1024)))
}

#[tokio::test]
#[ignore = "live measurement; needs rust-analyzer and a real workspace"]
async fn measure_rust_analyzer_on_a_real_workspace() {
    let Ok(ws) = std::env::var("OPENCRAYLSP_LIVE_WS") else {
        eprintln!("set OPENCRAYLSP_LIVE_WS to run");
        return;
    };
    let ws = PathBuf::from(ws);
    let query = std::env::var("OPENCRAYLSP_LIVE_QUERY").unwrap_or_else(|_| "new".to_owned());
    let target = std::env::var("OPENCRAYLSP_LIVE_TARGET_DIR")
        .unwrap_or_else(|_| "/tmp/cargo-opencraylsp-live".to_owned());
    let src = format!(
        "[limits]\nstartup_timeout_ms = 600000\nrequest_timeout_ms = 180000\n\
         startup_grace_ms = 0\nidle_shutdown_secs = 0\nmax_rss_mb = 65536\n\
         [[server]]\nname = \"rust-analyzer\"\ncommand = \"nice\"\n\
         args = [\"-n\", \"10\", \"rust-analyzer\"]\n\
         extensions = {{ rs = \"rust\" }}\nroot_markers = [\"Cargo.toml\"]\n\
         env = {{ CARGO_TARGET_DIR = {target:?}, CARGO_BUILD_JOBS = \"8\" }}\n\
         initialization_options = {{ files = {{ watcher = \"server\" }} }}\n"
    );
    let pool = Pool::new(Arc::new(
        LspConfig::from_toml_str_without_presets(&src).unwrap(),
    ));
    let backend = pool.bind(&ws, LanguageSelection::All);
    let cancel = CancellationToken::new();
    let started = Instant::now();

    // Poll `workspace/symbol` until it returns real data: that is "ready".
    let mut first_answer = None;
    let mut ready_at = None;
    let mut samples = Vec::new();
    while started.elapsed() < Duration::from_secs(900) {
        let outcome = backend
            .request_workspace(
                "rust-analyzer",
                "workspace/symbol",
                json!({"query": query}),
                &cancel,
            )
            .await;
        pool.guard_once().await;
        let info = backend.status().await.instances.into_iter().next();
        let (state, rss, pct) = info
            .as_ref()
            .map(|i| {
                (
                    format!("{:?}", i.state),
                    i.rss_bytes,
                    i.indexing.as_ref().and_then(|x| x.percent),
                )
            })
            .unwrap_or_default();
        eprintln!(
            "t={:>4}s state={state:<9} rss={:<10} idx={:?} -> {}",
            started.elapsed().as_secs(),
            mib(rss),
            pct,
            match &outcome {
                Ok(s) => format!("ok({} bytes)", s.value.to_string().len()),
                Err(LspError::Indexing { message, .. }) => format!("indexing: {message}"),
                Err(e) => format!("error: {e}"),
            }
        );
        samples.push((started.elapsed().as_secs(), rss));
        if first_answer.is_none() && outcome.is_ok() {
            first_answer = Some(started.elapsed());
        }
        if let Ok(served) = &outcome
            && served.indexing.is_none()
            && served.value.as_array().is_some_and(|a| !a.is_empty())
        {
            ready_at = Some(started.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    eprintln!("RESULT first_answer={first_answer:?} ready_at={ready_at:?}");

    // Steady state: latency of repeated queries and RSS after settling.
    let mut latencies = Vec::new();
    for _ in 0..10 {
        let t = Instant::now();
        let _ = backend
            .request_workspace(
                "rust-analyzer",
                "workspace/symbol",
                json!({"query": query}),
                &cancel,
            )
            .await;
        latencies.push(t.elapsed());
    }
    latencies.sort();
    eprintln!(
        "RESULT query_latency median={:?} max={:?}",
        latencies[5], latencies[9]
    );
    tokio::time::sleep(Duration::from_secs(30)).await;
    pool.guard_once().await;
    let info = backend.status().await.instances.into_iter().next().unwrap();
    eprintln!(
        "RESULT steady_rss={} pid={:?}",
        mib(info.rss_bytes),
        info.pid
    );

    // Sharing: three more connections, still one process.
    let extra: Vec<_> = (0..3)
        .map(|_| pool.bind(&ws, LanguageSelection::All))
        .collect();
    for b in &extra {
        let served = b
            .request_workspace(
                "rust-analyzer",
                "workspace/symbol",
                json!({"query": query}),
                &cancel,
            )
            .await;
        assert!(served.is_ok(), "{served:?}");
    }
    eprintln!(
        "RESULT instances_with_4_connections={}",
        pool.instance_count().await
    );
    pool.shutdown().await;
}
