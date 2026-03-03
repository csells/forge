use crate::{WatchArgs, build_executor, build_runtime_persistence, cxdb_host_config_from_env, load_dot_source};
use forge_attractor::{
    PipelineControl, PipelineRunner, PipelineStatus, RunConfig, RuntimeEvent, RuntimeEventKind,
    RuntimeEventSink, prepare_pipeline, runtime_event_channel,
};
use forge_attractor::events::{PipelineEvent, StageEvent, ParallelEvent, InterviewEvent};
use forge_attractor::graph::Graph;
use serde::Serialize;
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

// Asupersync HTTP server
use asupersync::http::h1::listener::Http1Listener;
use asupersync::http::h1::types::{Request as H1Request, Response as H1Response};
use asupersync::runtime::RuntimeBuilder;

// ---------------------------------------------------------------------------
// Shared application state (uses std::sync — works across runtimes)
// ---------------------------------------------------------------------------

struct AppState {
    graph: Graph,
    dot_source: String,
    logs_root: PathBuf,
    status: RwLock<WatchStatus>,
    control: Arc<PipelineControl>,
    start_trigger: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// Ring buffer of events for SSE long-poll. Clients send `?after=<seq>` to
    /// get events with sequence > seq.
    event_log: Mutex<EventLog>,
    /// Notify waiting SSE clients that new events arrived.
    event_notify: std::sync::Condvar,
    /// Dummy mutex for condvar (condvar needs a mutex guard).
    event_notify_lock: Mutex<()>,
}

struct EventLog {
    events: Vec<(u64, String)>, // (sequence_no, JSON)
    next_seq: u64,
}

impl EventLog {
    fn new() -> Self {
        Self { events: Vec::new(), next_seq: 1 }
    }

    fn push(&mut self, json: String) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.events.push((seq, json));
        // Keep at most 10k events in memory
        if self.events.len() > 10_000 {
            self.events.drain(..5_000);
        }
        seq
    }

    fn since(&self, after_seq: u64) -> Vec<&str> {
        self.events
            .iter()
            .filter(|(seq, _)| *seq > after_seq)
            .map(|(_, json)| json.as_str())
            .collect()
    }

    fn latest_seq(&self) -> u64 {
        self.events.last().map(|(seq, _)| *seq).unwrap_or(0)
    }
}

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum WatchStatus {
    WaitingForStart,
    NotStarted,
    Running {
        active_nodes: HashSet<String>,
        completed_nodes: Vec<String>,
        failed_nodes: Vec<String>,
    },
    Paused {
        active_nodes: HashSet<String>,
        completed_nodes: Vec<String>,
        failed_nodes: Vec<String>,
        paused_at_node: Option<String>,
    },
    Completed {
        run_id: String,
        completed_nodes: Vec<String>,
    },
    Stopped {
        completed_nodes: Vec<String>,
        reason: String,
    },
    Failed {
        reason: String,
    },
}

// ---------------------------------------------------------------------------
// Entry point — runs on asupersync runtime
// ---------------------------------------------------------------------------

pub fn run_sync(args: WatchArgs) -> Result<ExitCode, String> {
    let source = load_dot_source(args.dot_file.as_deref(), args.dot_source.as_deref())?;
    let (graph, diagnostics) =
        prepare_pipeline(&source, &[], &[]).map_err(|e| e.to_string())?;
    for diag in &diagnostics {
        eprintln!("warning: {}", diag.message);
    }

    let cxdb = cxdb_host_config_from_env()?;
    let (storage, artifacts) = build_runtime_persistence(&cxdb)?;

    // Extract server config before consuming args
    let bind_addr = args.bind.clone();
    let bind_port = args.port;
    let open_browser = args.open;
    let wait_for_start = args.wait_for_start;

    let logs_root = args.logs_root.clone().unwrap_or_else(|| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        PathBuf::from(format!(".forge-watch-{ts}"))
    });

    // Pipeline event channel (tokio mpsc — used by forge-attractor internally)
    let (mpsc_tx, mpsc_rx) = runtime_event_channel();

    // Deferred start: if --wait-for-start, create a std::sync gate
    let (start_tx, start_rx) = if wait_for_start {
        let (tx, rx) = std::sync::mpsc::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let initial_status = if wait_for_start {
        WatchStatus::WaitingForStart
    } else {
        WatchStatus::NotStarted
    };

    // Pipeline control handle (runtime-agnostic — uses AtomicBool + tokio::sync::Notify)
    let control = Arc::new(PipelineControl::new());

    let state = Arc::new(AppState {
        graph: graph.clone(),
        dot_source: source.clone(),
        logs_root: logs_root.clone(),
        status: RwLock::new(initial_status),
        control: control.clone(),
        start_trigger: Mutex::new(start_tx),
        event_log: Mutex::new(EventLog::new()),
        event_notify: std::sync::Condvar::new(),
        event_notify_lock: Mutex::new(()),
    });

    // Bridge thread: reads events from tokio mpsc → pushes to event_log + updates status
    let bridge_state = state.clone();
    let bridge_handle = std::thread::Builder::new()
        .name("forge-watch-bridge".into())
        .spawn(move || {
            // Build a tiny tokio runtime just for the mpsc receiver
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build bridge tokio runtime");
            rt.block_on(bridge_loop(bridge_state, mpsc_rx));
        })
        .map_err(|e| format!("failed to spawn bridge thread: {e}"))?;

    // Pipeline thread: runs on its own tokio runtime
    let pipeline_state = state.clone();
    let pipeline_control = control.clone();
    let pipeline_handle = std::thread::Builder::new()
        .name("forge-watch-pipeline".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build pipeline tokio runtime");
            rt.block_on(pipeline_loop(
                args, graph, logs_root, mpsc_tx, cxdb, storage, artifacts,
                pipeline_state, pipeline_control, start_rx,
            ));
        })
        .map_err(|e| format!("failed to spawn pipeline thread: {e}"))?;

    // Asupersync HTTP server
    let addr = format!("{bind_addr}:{bind_port}");
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| format!("failed to build asupersync runtime: {e}"))?;

    let handle = runtime.handle();
    runtime.block_on(async {
        let handler_state = state.clone();
        let listener = Http1Listener::bind(addr.clone(), move |req: H1Request| {
            let st = Arc::clone(&handler_state);
            async move { dispatch(req, &st) }
        })
        .await
        .map_err(|e| format!("failed to bind {addr}: {e}"))?;

        eprintln!("Pipeline visualizer: http://{}", listener.local_addr().map_err(|e| e.to_string())?);

        if open_browser {
            let url = format!("http://{addr}");
            let _ = std::process::Command::new("xdg-open")
                .arg(&url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }

        // Install Ctrl+C handler (asupersync's signal::ctrl_c is Phase 0 stub)
        let shutdown = listener.shutdown_signal();
        let _ = std::thread::Builder::new()
            .name("forge-watch-signal".into())
            .spawn(move || {
                // Use raw libc for SIGINT — simple and dependency-free
                #[cfg(unix)]
                {
                    use std::sync::atomic::{AtomicBool, Ordering};
                    static SIGNALED: AtomicBool = AtomicBool::new(false);
                    unsafe {
                        libc::signal(libc::SIGINT, sigint_handler as *const () as libc::sighandler_t);
                    }
                    extern "C" fn sigint_handler(_: libc::c_int) {
                        SIGNALED.store(true, std::sync::atomic::Ordering::Release);
                    }
                    while !SIGNALED.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    let _ = shutdown.begin_drain(Duration::from_secs(2));
                }
            });

        listener.run(&handle).await.map_err(|e| format!("server error: {e}"))?;

        // Wait for pipeline and bridge to finish
        let _ = pipeline_handle.join();
        let _ = bridge_handle.join();

        Ok::<ExitCode, String>(ExitCode::SUCCESS)
    })
}

// ---------------------------------------------------------------------------
// Pipeline loop (runs in its own tokio runtime thread)
// ---------------------------------------------------------------------------

async fn pipeline_loop(
    args: WatchArgs,
    graph: Graph,
    logs_root: PathBuf,
    mpsc_tx: forge_attractor::RuntimeEventSender,
    cxdb: crate::CxdbHostConfig,
    storage: Option<forge_attractor::SharedAttractorStorageWriter>,
    artifacts: Option<Arc<dyn forge_attractor::AttractorArtifactWriter>>,
    state: Arc<AppState>,
    control: Arc<PipelineControl>,
    start_rx: Option<std::sync::mpsc::Receiver<()>>,
) {
    // Wait for start signal if deferred
    if let Some(rx) = start_rx {
        if rx.recv().is_err() {
            eprintln!("Server shut down before pipeline started");
            return;
        }
    }

    {
        let mut status = state.status.write().unwrap();
        *status = WatchStatus::Running {
            active_nodes: HashSet::new(),
            completed_nodes: Vec::new(),
            failed_nodes: Vec::new(),
        };
    }

    let executor = match build_executor(
        args.interviewer,
        args.backend,
        args.human_answers,
        &cxdb,
        storage.clone(),
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to build executor: {e}");
            let mut status = state.status.write().unwrap();
            *status = WatchStatus::Failed { reason: e };
            return;
        }
    };

    let result = PipelineRunner
        .run(
            &graph,
            RunConfig {
                run_id: args.run_id,
                logs_root: Some(logs_root),
                events: RuntimeEventSink::with_sender(mpsc_tx),
                executor,
                storage,
                artifacts,
                cxdb_persistence: cxdb.persistence,
                control: Some(control),
                ..RunConfig::default()
            },
        )
        .await;

    match result {
        Ok(ref r) => {
            let new_status = match r.status {
                PipelineStatus::Success => WatchStatus::Completed {
                    run_id: r.run_id.clone(),
                    completed_nodes: r.completed_nodes.clone(),
                },
                PipelineStatus::Fail => {
                    let reason = r.failure_reason.clone()
                        .unwrap_or_else(|| "pipeline failed".to_string());
                    if reason.contains("stopped by user") {
                        WatchStatus::Stopped {
                            completed_nodes: r.completed_nodes.clone(),
                            reason: "Stopped by user".to_string(),
                        }
                    } else {
                        WatchStatus::Failed { reason }
                    }
                }
            };
            let mut status = state.status.write().unwrap();
            *status = new_status;
            eprintln!("Pipeline finished: {:?}. Server still running — press Ctrl+C to exit.", r.status);
        }
        Err(ref e) => {
            let mut status = state.status.write().unwrap();
            *status = WatchStatus::Failed { reason: e.to_string() };
            eprintln!("Pipeline failed: {e}. Server still running — press Ctrl+C to exit.");
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge loop: tokio mpsc → event_log + status updates
// ---------------------------------------------------------------------------

async fn bridge_loop(state: Arc<AppState>, mut rx: forge_attractor::RuntimeEventReceiver) {
    while let Some(event) = rx.recv().await {
        // Serialize event to JSON
        let json = serde_json::to_string(&event).unwrap_or_default();

        // Update status
        update_status(&state, &event);

        // Push to event log and notify waiting SSE clients
        {
            let mut log = state.event_log.lock().unwrap();
            log.push(json);
        }
        // Wake up any threads blocked in the SSE long-poll
        state.event_notify.notify_all();
    }
}

// ---------------------------------------------------------------------------
// Status updates from events
// ---------------------------------------------------------------------------

fn update_status(state: &AppState, event: &RuntimeEvent) {
    let mut status = state.status.write().unwrap();

    // Handle pipeline-level control events
    if let RuntimeEventKind::Pipeline(ref pe) = event.kind {
        match pe {
            PipelineEvent::Paused { node_id, .. } => {
                if let WatchStatus::Running {
                    ref active_nodes, ref completed_nodes, ref failed_nodes,
                } | WatchStatus::Paused {
                    ref active_nodes, ref completed_nodes, ref failed_nodes, ..
                } = *status {
                    *status = WatchStatus::Paused {
                        active_nodes: active_nodes.clone(),
                        completed_nodes: completed_nodes.clone(),
                        failed_nodes: failed_nodes.clone(),
                        paused_at_node: Some(node_id.clone()),
                    };
                }
                return;
            }
            PipelineEvent::Stopped { reason, .. } => {
                let completed = match &*status {
                    WatchStatus::Running { completed_nodes, .. }
                    | WatchStatus::Paused { completed_nodes, .. } => completed_nodes.clone(),
                    _ => Vec::new(),
                };
                *status = WatchStatus::Stopped {
                    completed_nodes: completed,
                    reason: reason.clone(),
                };
                return;
            }
            PipelineEvent::Started { .. } | PipelineEvent::Resumed { .. } => {
                if let WatchStatus::Paused {
                    ref active_nodes, ref completed_nodes, ref failed_nodes, ..
                } = *status {
                    *status = WatchStatus::Running {
                        active_nodes: active_nodes.clone(),
                        completed_nodes: completed_nodes.clone(),
                        failed_nodes: failed_nodes.clone(),
                    };
                }
                return;
            }
            _ => {}
        }
    }

    // Handle node-level events
    let (active_nodes, completed_nodes, failed_nodes) = match *status {
        WatchStatus::Running {
            ref mut active_nodes,
            ref mut completed_nodes,
            ref mut failed_nodes,
        } => (active_nodes, completed_nodes, failed_nodes),
        WatchStatus::Paused {
            ref mut active_nodes,
            ref mut completed_nodes,
            ref mut failed_nodes,
            ..
        } => (active_nodes, completed_nodes, failed_nodes),
        _ => return,
    };

    match &event.kind {
        RuntimeEventKind::Stage(stage) => match stage {
            StageEvent::Started { node_id, .. } => {
                active_nodes.insert(node_id.clone());
            }
            StageEvent::Completed { node_id, .. } => {
                active_nodes.remove(node_id);
                if !completed_nodes.contains(node_id) {
                    completed_nodes.push(node_id.clone());
                }
            }
            StageEvent::Failed { node_id, will_retry, .. } => {
                if !will_retry {
                    active_nodes.remove(node_id);
                    failed_nodes.push(node_id.clone());
                }
            }
            StageEvent::Retrying { .. } => {}
        },
        RuntimeEventKind::Parallel(par) => match par {
            ParallelEvent::Started { node_id, .. } => {
                active_nodes.insert(node_id.clone());
            }
            ParallelEvent::BranchStarted { target_node, .. } => {
                active_nodes.insert(target_node.clone());
            }
            ParallelEvent::BranchCompleted { target_node, .. } => {
                active_nodes.remove(target_node);
                if !completed_nodes.contains(target_node) {
                    completed_nodes.push(target_node.clone());
                }
            }
            ParallelEvent::Completed { node_id, .. } => {
                active_nodes.remove(node_id);
                if !completed_nodes.contains(node_id) {
                    completed_nodes.push(node_id.clone());
                }
            }
        },
        RuntimeEventKind::Interview(interview) => match interview {
            InterviewEvent::Started { node_id, .. } => {
                active_nodes.insert(node_id.clone());
            }
            InterviewEvent::Completed { node_id, .. }
            | InterviewEvent::Timeout { node_id, .. } => {
                active_nodes.remove(node_id);
                if !completed_nodes.contains(node_id) {
                    completed_nodes.push(node_id.clone());
                }
            }
        },
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// HTTP request dispatch (bridges h1 types ↔ synchronous web::Router logic)
// ---------------------------------------------------------------------------

fn dispatch(req: H1Request, state: &AppState) -> H1Response {
    let path = req.uri.split('?').next().unwrap_or(&req.uri);
    let query_string = req.uri.split_once('?').map(|(_, q)| q);
    let method = req.method.as_str();

    // Manual route matching (direct and fast — no web::Router overhead for a handful of routes)
    match (method, path) {
        ("GET", "/") => serve_html(),
        ("GET", "/api/graph") => serve_json(&state.graph),
        ("GET", "/api/dot") => plain_text(&state.dot_source),
        ("GET", "/api/status") => {
            let status = state.status.read().unwrap().clone();
            serve_json(&status)
        }
        ("GET", "/api/events") => serve_events(state, query_string),
        ("GET", p) if p.starts_with("/api/artifacts/") => serve_artifact(state, p),
        ("POST", "/api/control/start") => control_start(state),
        ("POST", "/api/control/pause") => control_pause(state),
        ("POST", "/api/control/resume") => control_resume(state),
        ("POST", "/api/control/stop") => control_stop(state),
        _ => H1Response::new(404, "Not Found", b"Not Found".to_vec()),
    }
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

fn serve_html() -> H1Response {
    let mut resp = H1Response::new(200, "OK", include_bytes!("watch.html").to_vec());
    resp.headers.push(("Content-Type".into(), "text/html; charset=utf-8".into()));
    resp
}

fn serve_json<T: Serialize>(value: &T) -> H1Response {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let mut resp = H1Response::new(200, "OK", body);
    resp.headers.push(("Content-Type".into(), "application/json".into()));
    resp.headers.push(("Access-Control-Allow-Origin".into(), "*".into()));
    resp
}

fn plain_text(text: &str) -> H1Response {
    let mut resp = H1Response::new(200, "OK", text.as_bytes().to_vec());
    resp.headers.push(("Content-Type".into(), "text/plain; charset=utf-8".into()));
    resp
}

/// SSE long-poll: returns all events since `?after=<seq>`, blocking briefly if none available.
fn serve_events(state: &AppState, query: Option<&str>) -> H1Response {
    let after_seq: u64 = query
        .and_then(|q| {
            q.split('&')
                .find_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    if k == "after" { v.parse().ok() } else { None }
                })
        })
        .unwrap_or(0);

    // Check for events already available
    {
        let log = state.event_log.lock().unwrap();
        let events = log.since(after_seq);
        if !events.is_empty() {
            return sse_response(&events, log.latest_seq());
        }
    }

    // Block for up to 15 seconds waiting for new events
    let guard = state.event_notify_lock.lock().unwrap();
    let _guard = state.event_notify.wait_timeout(guard, Duration::from_secs(15)).unwrap();

    // Check again after waking up
    let log = state.event_log.lock().unwrap();
    let events = log.since(after_seq);
    sse_response(&events, log.latest_seq())
}

fn sse_response(events: &[&str], latest_seq: u64) -> H1Response {
    let mut body = String::new();
    for json in events {
        body.push_str("data: ");
        body.push_str(json);
        body.push_str("\n\n");
    }
    // If no events, send a comment as keepalive
    if events.is_empty() {
        body.push_str(": keepalive\n\n");
    }

    let mut resp = H1Response::new(200, "OK", body.into_bytes());
    resp.headers.push(("Content-Type".into(), "text/event-stream".into()));
    resp.headers.push(("Cache-Control".into(), "no-cache".into()));
    resp.headers.push(("X-Last-Seq".into(), latest_seq.to_string()));
    resp.headers.push(("Access-Control-Allow-Origin".into(), "*".into()));
    resp
}

fn serve_artifact(state: &AppState, path: &str) -> H1Response {
    // path = "/api/artifacts/{node_id}/{filename}"
    let rest = match path.strip_prefix("/api/artifacts/") {
        Some(r) => r,
        None => return H1Response::new(404, "Not Found", Vec::new()),
    };

    let (node_id, filename) = match rest.split_once('/') {
        Some(pair) => pair,
        None => return H1Response::new(400, "Bad Request", b"missing filename".to_vec()),
    };

    const ALLOWED: &[&str] = &["prompt.md", "response.md", "status.json"];
    if !ALLOWED.contains(&filename) {
        return H1Response::new(400, "Bad Request", b"invalid filename".to_vec());
    }

    if node_id.contains("..") || node_id.contains('/') || node_id.contains('\\') {
        return H1Response::new(400, "Bad Request", b"invalid node_id".to_vec());
    }

    let file_path = state.logs_root.join(node_id).join(filename);
    match std::fs::read_to_string(&file_path) {
        Ok(content) => {
            let ct = if filename.ends_with(".json") {
                "application/json"
            } else {
                "text/plain; charset=utf-8"
            };
            let mut resp = H1Response::new(200, "OK", content.into_bytes());
            resp.headers.push(("Content-Type".into(), ct.into()));
            resp
        }
        Err(_) => H1Response::new(404, "Not Found", Vec::new()),
    }
}

// ---------------------------------------------------------------------------
// Control endpoints
// ---------------------------------------------------------------------------

fn control_start(state: &AppState) -> H1Response {
    let mut trigger = state.start_trigger.lock().unwrap();
    if let Some(tx) = trigger.take() {
        let _ = tx.send(());
        H1Response::new(200, "OK", Vec::new())
    } else {
        H1Response::new(409, "Conflict", b"already started".to_vec())
    }
}

fn control_pause(state: &AppState) -> H1Response {
    state.control.pause();
    H1Response::new(200, "OK", Vec::new())
}

fn control_resume(state: &AppState) -> H1Response {
    state.control.resume();
    // Immediate status update for responsive UI
    let mut status = state.status.write().unwrap();
    if let WatchStatus::Paused {
        ref active_nodes, ref completed_nodes, ref failed_nodes, ..
    } = *status {
        *status = WatchStatus::Running {
            active_nodes: active_nodes.clone(),
            completed_nodes: completed_nodes.clone(),
            failed_nodes: failed_nodes.clone(),
        };
    }
    H1Response::new(200, "OK", Vec::new())
}

fn control_stop(state: &AppState) -> H1Response {
    state.control.cancel();
    // Also trigger start if pending, so the pipeline thread can exit
    let mut trigger = state.start_trigger.lock().unwrap();
    if let Some(tx) = trigger.take() {
        let _ = tx.send(());
    }
    H1Response::new(200, "OK", Vec::new())
}
