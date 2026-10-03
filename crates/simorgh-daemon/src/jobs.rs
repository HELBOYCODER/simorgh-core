//! Long-running jobs: discovery, delay tests, scans, WARP registration,
//! subscription-URL rewriting and CDN fronting.
//!
//! This mirrors `crates/zray-mobile/src/android.rs`'s `start_job` — the same
//! `zero-discovery` entry points, the same separate two-worker runtime so a
//! sweep of hundreds of sockets cannot starve anything else, the same
//! catch-unwind guard that turns a panicking job into an `error` event plus a
//! `done {"error": true}` — with the JNI listener replaced by an in-process
//! event log. HTTP clients attach to that log through
//! `GET /job/<id>/events`, which replays everything produced so far and then
//! streams, batching at most [`EVENT_BATCH_INTERVAL`] the way the listener's
//! `batching_sink` does.

use std::collections::HashMap;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::runtime::Runtime;
use zero_discovery::events::batching_sink_with_interval;
use zero_discovery::{
    CancellationToken, DiscoverRequest, EndReason, EventCallback, EventSink, ScanRequest,
    TestRequest, WarpRequest,
};

/// Minimum spacing between two flushes into the event log. The Android
/// listener batches at 100 ms; an HTTP streamer can afford to feel live, so
/// this stays at 50 ms — events land at the client at most that late.
pub const EVENT_BATCH_INTERVAL: Duration = Duration::from_millis(50);

/// Finished jobs kept for a late `events` attach before the oldest are
/// evicted.
const MAX_JOBS: usize = 512;

/// Everything one job produces, kept until eviction.
struct EventLog {
    lines: Vec<String>,
    /// Set once a `{"t":"done",…}` line has been appended: the terminal
    /// event of every job, per the contract.
    done: bool,
}

/// A handle the HTTP layer attaches to.
pub struct Job {
    cancel: CancellationToken,
    log: Mutex<EventLog>,
}

impl Job {
    /// Lines produced from index `from` on, plus the done flag.
    pub fn snapshot(&self, from: usize) -> (Vec<String>, bool) {
        let log = self.log.lock().unwrap_or_else(|p| p.into_inner());
        (log.lines[from..].to_vec(), log.done)
    }

    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

static JOBS: Mutex<Option<HashMap<String, Arc<Job>>>> = Mutex::new(None);
static NEXT_JOB: AtomicU64 = AtomicU64::new(1);

fn jobs() -> std::sync::MutexGuard<'static, Option<HashMap<String, Arc<Job>>>> {
    JOBS.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The runtime jobs run on. Separate from the proxy's (which zray-mobile
/// builds itself) and from the HTTP server's, exactly like the mobile
/// `zray-jobs` runtime. Two workers: the jobs are I/O bound. The blocking
/// pool is for name lookups — a TCP sweep resolves many hostnames at once.
fn job_runtime() -> Option<&'static Runtime> {
    static RUNTIME: OnceLock<Option<Runtime>> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(64)
                .thread_name("simorgh-jobs")
                .enable_all()
                .build()
                .map_err(|error| {
                    tracing::error!(%error, "the job runtime failed to start");
                })
                .ok()
        })
        .as_ref()
}

pub fn is_job_method(method: &str) -> bool {
    matches!(
        method,
        "discover" | "test_links" | "scan" | "warp_register" | "subscription_fetch" | "front_links"
    )
}

/// Start the job named `method` with `body` as its request JSON. `Ok(id)`
/// once the job is spawned; `Err(message)` when the request is unusable and
/// the job was never started (the mobile contract delivers an error event and
/// handle 0 here; over HTTP an immediate `{"error": …}` is more useful to the
/// GUI, and the same message text appears in both).
pub fn start(method: &str, body: &Value, data_dir: &Path) -> Result<String, String> {
    match method {
        "discover" => {
            let mut request = body.clone();
            // Feed caches live under the daemon's data dir unless the host
            // says otherwise; the GUI should not have to know the layout.
            if request.get("cache_dir").and_then(Value::as_str).is_none() {
                if let Some(map) = request.as_object_mut() {
                    map.insert(
                        "cache_dir".to_string(),
                        json!(data_dir.join("cache").join("feeds")),
                    );
                }
            }
            let request: DiscoverRequest = serde_json::from_value(request)
                .map_err(|error| format!("invalid discover request: {error}"))?;
            Ok(spawn_job(request, zero_discovery::discover))
        }
        "test_links" => {
            let request: TestRequest = serde_json::from_value(body.clone())
                .map_err(|error| format!("invalid test_links request: {error}"))?;
            Ok(spawn_job(request, zero_discovery::test_links))
        }
        "scan" => {
            let request: ScanRequest = serde_json::from_value(body.clone())
                .map_err(|error| format!("invalid scan request: {error}"))?;
            Ok(spawn_job(request, zero_discovery::scan))
        }
        "warp_register" => {
            let request: WarpRequest = serde_json::from_value(body.clone())
                .map_err(|error| format!("invalid warp_register request: {error}"))?;
            Ok(spawn_job(request, zero_discovery::warp_job))
        }
        "subscription_fetch" => {
            // `subscriptionFetchUrl(address)` in android.rs: the address to
            // download for a subscription the user added, with BPB panels
            // rewritten to their smallest equivalent. A pure, fast string
            // transform — but it arrives and leaves as a job to keep the
            // daemon's long-work surface uniform. Never logged: a panel's
            // path is its password.
            let url = body
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| "subscription_fetch needs {\"url\": \"…\"}".to_string())?;
            let url = url.to_string();
            Ok(spawn_job(url, |url, sink, _cancel| async move {
                let result = zero_discovery::subscription_fetch_url(&url);
                sink.emit(json!({"t": "result", "url": result}));
                sink.emit(json!({"t": "done", "reason": "exhausted", "elapsed_ms": 0}));
                EndReason::Exhausted
            }))
        }
        "front_links" => {
            // `frontLinks(requestJson)` in android.rs:
            // {"links": […], "seed": n, "max": n}. Each CDN-fronted TLS link
            // is re-aimed at a bounded sample of Cloudflare edge IPs (SNI and
            // Host kept). Bounded however the caller asks: every variant is
            // a probe.
            let links: Vec<String> = body["links"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|link| link.as_str().map(str::to_string))
                .collect();
            let seed = body["seed"].as_u64().unwrap_or(0);
            let max = body["max"].as_u64().unwrap_or(18).min(64) as usize;
            Ok(spawn_job(
                (links, seed, max),
                |(links, seed, max), sink, _cancel| async move {
                    let produced = zero_discovery::front_via_edges(&links, seed, 1, max);
                    sink.emit(json!({"t": "result", "links": produced}));
                    sink.emit(json!({"t": "done", "reason": "exhausted", "elapsed_ms": 0}));
                    EndReason::Exhausted
                },
            ))
        }
        other => Err(format!("unknown job method {other:?}")),
    }
}

/// Register one job and run it on the job runtime, batching its events into
/// the event log. Same shape as android.rs's `start_job`, minus the JVM.
///
/// The catch-unwind guard, like the mobile one, is only load-bearing in a
/// `panic = "unwind"` build: the workspace `release` profile aborts on panic
/// (a daemon that aborts is restarted by its GUI; a mobile library that
/// aborted would take the host app down). Build with `--profile
/// release-mobile` — or the debug profile — where one panicking job must
/// not end the process.
fn spawn_job<R, F, Fut>(request: R, run: F) -> String
where
    R: Send + 'static,
    F: FnOnce(R, EventSink, CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = EndReason> + Send + 'static,
{
    let id = NEXT_JOB.fetch_add(1, Ordering::Relaxed).to_string();
    let job = Arc::new(Job {
        cancel: CancellationToken::new(),
        log: Mutex::new(EventLog {
            lines: Vec::new(),
            done: false,
        }),
    });
    register(id.clone(), Arc::clone(&job));

    // Unreachable in practice: `job_runtime()` is probed before the first
    // spawn below; if even then it is gone, deliver a contract-shaped failure
    // instead of dropping the caller's request.
    let Some(runtime) = job_runtime() else {
        push_lines(
            &job,
            vec![
                json!({"t": "error", "message": "the job runtime is unavailable"}).to_string(),
                json!({"t": "done", "reason": "cancelled", "error": true}).to_string(),
            ],
        );
        return id;
    };

    let spawned_id = id.clone();
    runtime.spawn(async move {
        let callback: EventCallback = {
            let job = Arc::clone(&job);
            Arc::new(move |batch: String| {
                let lines: Vec<String> = batch
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(str::to_string)
                    .collect();
                push_lines(&job, lines);
            })
        };
        let (sink, flusher) = batching_sink_with_interval(callback, EVENT_BATCH_INTERVAL);
        let guard_sink = sink.clone();
        let outcome = AssertUnwindSafe(run(request, sink, job.cancel.clone()))
            .catch_unwind()
            .await;
        if outcome.is_err() {
            tracing::error!(id = %spawned_id, "a job panicked internally");
            guard_sink.emit(json!({"t": "error", "message": "the job failed internally"}));
            guard_sink.emit(json!({"t": "done", "reason": "cancelled", "error": true}));
        }
        drop(guard_sink);
        let _ = flusher.await;
    });
    id
}

/// Append already-serialised events, flagging the terminal `done`.
fn push_lines(job: &Job, lines: Vec<String>) {
    let mut log = job
        .log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    for line in lines {
        if !log.done {
            // `EventSink::emit` serialises compactly, but never trust the
            // wire: parse and look at the `t` field.
            if serde_json::from_str::<Value>(&line)
                .ok()
                .is_some_and(|event| event["t"] == "done")
            {
                log.done = true;
            }
        }
        log.lines.push(line);
    }
}

fn register(id: String, job: Arc<Job>) {
    let mut slot = jobs();
    let map = slot.get_or_insert_with(HashMap::new);
    if map.len() >= MAX_JOBS {
        // Drop the oldest finished jobs first; a still-running job keeps its
        // slot so its events cannot be evicted from under a listener.
        let mut finished: Vec<String> = map
            .iter()
            .filter(|(_, candidate)| {
                candidate
                    .log
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .done
            })
            .map(|(existing, _)| existing.clone())
            .collect();
        finished.sort_by_key(|existing| existing.parse::<u64>().unwrap_or(u64::MAX));
        let mut evict = map.len() + 1 - MAX_JOBS;
        for existing in finished {
            if evict == 0 {
                break;
            }
            map.remove(&existing);
            evict -= 1;
        }
    }
    map.insert(id, job);
}

/// Look one job up. The `Arc` lives on after eviction, so a streamer that
/// attached earlier keeps reading.
pub fn lookup(id: &str) -> Option<Arc<Job>> {
    jobs().as_ref().and_then(|map| map.get(id).cloned())
}

/// `POST /job/<id>/cancel`: signal the token. Unknown or finished handles are
/// ignored, as in the contract's `cancel(handle)`.
pub fn cancel(id: &str) -> bool {
    if let Some(job) = lookup(id) {
        job.cancel();
        return true;
    }
    false
}
