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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// Asupersync
use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncWriteExt};
use asupersync::net::TcpListener;
use asupersync::net::TcpStream;
use asupersync::net::websocket::{WebSocketAcceptor, ServerWebSocket, Message};
use asupersync::runtime::{RuntimeBuilder, RuntimeHandle};
use asupersync::sync::Notify;

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
    /// Ring buffer of events. WS clients track their own position.
    event_log: Mutex<EventLog>,
    /// Wakes WS client tasks when new events arrive.
    event_notify: Arc<Notify>,
    /// Signal for server shutdown.
    shutdown: AtomicBool,
    /// Handle for spawning WS client tasks.
    handle: Mutex<Option<RuntimeHandle>>,
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

    fn since(&self, after_seq: u64) -> Vec<(u64, &str)> {
        self.events
            .iter()
            .filter(|(seq, _)| *seq > after_seq)
            .map(|(seq, json)| (*seq, json.as_str()))
            .collect()
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
        event_notify: Arc::new(Notify::new()),
        shutdown: AtomicBool::new(false),
        handle: Mutex::new(None),
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

    // Asupersync runtime — TCP listener + WebSocket server
    let addr = format!("{bind_addr}:{bind_port}");
    let runtime = RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| format!("failed to build asupersync runtime: {e}"))?;

    let handle = runtime.handle();
    // Store handle so WS client tasks can be spawned
    *state.handle.lock().unwrap() = Some(handle.clone());

    runtime.block_on(async {
        let cx = Cx::for_testing();

        let listener = TcpListener::bind(addr.clone()).await
            .map_err(|e| format!("failed to bind {addr}: {e}"))?;
        let local_addr = listener.local_addr().map_err(|e| e.to_string())?;
        eprintln!("Pipeline visualizer: http://{local_addr}");

        if open_browser {
            let url = format!("http://{addr}");
            let _ = std::process::Command::new("xdg-open")
                .arg(&url)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }

        // Install Ctrl+C handler
        let shutdown_state = state.clone();
        let _ = std::thread::Builder::new()
            .name("forge-watch-signal".into())
            .spawn(move || {
                #[cfg(unix)]
                {
                    static SIGNALED: AtomicBool = AtomicBool::new(false);
                    unsafe {
                        libc::signal(libc::SIGINT, sigint_handler as *const () as libc::sighandler_t);
                    }
                    extern "C" fn sigint_handler(_: libc::c_int) {
                        SIGNALED.store(true, Ordering::Release);
                    }
                    while !SIGNALED.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    shutdown_state.shutdown.store(true, Ordering::Release);
                    // Wake any waiting WS clients so they notice shutdown
                    shutdown_state.event_notify.notify_waiters();
                }
            });

        // Accept loop
        let acceptor = WebSocketAcceptor::new();
        loop {
            if state.shutdown.load(Ordering::Acquire) {
                break;
            }

            let accept_result = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(1),
                listener.accept(),
            ).await;

            let (stream, _peer) = match accept_result {
                Ok(Ok(pair)) => pair,
                Ok(Err(e)) => {
                    eprintln!("accept error: {e}");
                    continue;
                }
                Err(_) => continue, // timeout — loop back to check shutdown
            };

            let conn_state = state.clone();
            let conn_acceptor = acceptor.clone();
            let conn_cx = cx.clone();
            let _ = handle.try_spawn(async move {
                if let Err(e) = handle_connection(conn_cx, stream, &conn_state, &conn_acceptor).await {
                    // Connection errors are expected (client disconnects, etc.)
                    let _ = e;
                }
            });
        }

        // Wait for pipeline and bridge to finish
        let _ = pipeline_handle.join();
        let _ = bridge_handle.join();

        Ok::<ExitCode, String>(ExitCode::SUCCESS)
    })
}

// ---------------------------------------------------------------------------
// Connection handler: parse HTTP, dispatch to WS upgrade or HTTP response
// ---------------------------------------------------------------------------

async fn handle_connection(
    cx: Cx,
    mut stream: TcpStream,
    state: &AppState,
    acceptor: &WebSocketAcceptor,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Read raw HTTP request
    let mut buf = vec![0u8; 8192];
    let mut total = 0;

    let request_end = loop {
        if total >= buf.len() {
            return write_http_error(&mut stream, 413, "Request Entity Too Large").await;
        }
        let n = read_some(&mut stream, &mut buf[total..]).await?;
        if n == 0 {
            return Ok(()); // client disconnected
        }
        total += n;
        // Check for end of HTTP headers
        if let Some(pos) = find_header_end(&buf[..total]) {
            break pos;
        }
    };

    // Parse with httparse
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(&buf[..request_end]) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return write_http_error(&mut stream, 400, "Bad Request").await,
    }

    let method = req.method.unwrap_or("");
    let full_path = req.path.unwrap_or("/");
    let path = full_path.split('?').next().unwrap_or(full_path);

    // Check for WebSocket upgrade
    let is_ws_upgrade = req.headers.iter().any(|h| {
        h.name.eq_ignore_ascii_case("Upgrade")
            && std::str::from_utf8(h.value).unwrap_or("").eq_ignore_ascii_case("websocket")
    });

    if method == "GET" && path == "/ws" && is_ws_upgrade {
        // WebSocket upgrade — hand off raw bytes + stream to acceptor
        let mut ws = acceptor.accept(&cx, &buf[..request_end], stream).await
            .map_err(|e| format!("ws accept: {e}"))?;
        ws_client_loop(&cx, &mut ws, state).await;
        return Ok(());
    }

    // Regular HTTP — dispatch and respond
    let response = dispatch_http(method, path, full_path, state);
    stream.write_all(&response).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// WebSocket client loop
// ---------------------------------------------------------------------------

async fn ws_client_loop(cx: &Cx, ws: &mut ServerWebSocket<TcpStream>, state: &AppState) {
    let mut last_seq: u64 = 0;

    // Send initial status + graph as first message
    let init = serde_json::json!({
        "type": "init",
        "graph": &state.graph,
        "status": &*state.status.read().unwrap(),
    });
    if ws.send(cx, Message::text(init.to_string())).await.is_err() {
        return;
    }

    // Send any buffered events (collect first to release lock before awaiting)
    let buffered: Vec<(u64, String)> = {
        let log = state.event_log.lock().unwrap();
        log.since(0).into_iter().map(|(seq, json)| (seq, json.to_string())).collect()
    };
    for (seq, json) in &buffered {
        let msg = format!(r#"{{"type":"event","seq":{seq},"data":{json}}}"#);
        if ws.send(cx, Message::text(msg)).await.is_err() {
            return;
        }
        last_seq = *seq;
    }

    loop {
        if state.shutdown.load(Ordering::Acquire) {
            let _ = ws.close(asupersync::net::websocket::CloseReason::going_away()).await;
            return;
        }

        // Check for new events
        let new_events: Vec<(u64, String)> = {
            let log = state.event_log.lock().unwrap();
            log.since(last_seq)
                .into_iter()
                .map(|(seq, json)| (seq, json.to_string()))
                .collect()
        };

        for (seq, json) in &new_events {
            let msg = format!(r#"{{"type":"event","seq":{seq},"data":{json}}}"#);
            if ws.send(cx, Message::text(msg)).await.is_err() {
                return;
            }
            last_seq = *seq;
        }

        if new_events.is_empty() {
            // Wait for new events or client message
            // We use a simple approach: wait on Notify with a timeout,
            // then check for incoming WS messages non-blockingly.
            let wait_result = asupersync::time::timeout(
                asupersync::time::wall_now(),
                Duration::from_secs(15),
                state.event_notify.notified(),
            ).await;

            match wait_result {
                Ok(()) => {} // notified — new events available, loop back
                Err(_) => {
                    // Timeout — send ping as keepalive
                    if ws.ping(asupersync::bytes::Bytes::new()).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP dispatch (non-WebSocket requests)
// ---------------------------------------------------------------------------

fn dispatch_http(method: &str, path: &str, _full_path: &str, state: &AppState) -> Vec<u8> {
    match (method, path) {
        ("GET", "/") => http_response(200, "text/html; charset=utf-8", include_bytes!("watch.html")),
        ("GET", "/api/graph") => json_response(&state.graph),
        ("GET", "/api/dot") => http_response(200, "text/plain; charset=utf-8", state.dot_source.as_bytes()),
        ("GET", "/api/status") => {
            let status = state.status.read().unwrap().clone();
            json_response(&status)
        }
        ("GET", p) if p.starts_with("/api/artifacts/") => serve_artifact(state, p),
        ("POST", "/api/control/start") => control_start(state),
        ("POST", "/api/control/pause") => control_pause(state),
        ("POST", "/api/control/resume") => control_resume(state),
        ("POST", "/api/control/stop") => control_stop(state),
        _ => http_response(404, "text/plain", b"Not Found"),
    }
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Request Entity Too Large",
        _ => "Unknown",
    };
    let mut resp = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Access-Control-Allow-Origin: *\r\n\
         \r\n",
        body.len(),
    ).into_bytes();
    resp.extend_from_slice(body);
    resp
}

fn json_response<T: Serialize>(value: &T) -> Vec<u8> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    http_response(200, "application/json", &body)
}

async fn write_http_error(
    stream: &mut TcpStream,
    status: u16,
    msg: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let resp = http_response(status, "text/plain", msg.as_bytes());
    stream.write_all(&resp).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Artifact serving
// ---------------------------------------------------------------------------

fn serve_artifact(state: &AppState, path: &str) -> Vec<u8> {
    let rest = match path.strip_prefix("/api/artifacts/") {
        Some(r) => r,
        None => return http_response(404, "text/plain", b"Not Found"),
    };

    let (node_id, filename) = match rest.split_once('/') {
        Some(pair) => pair,
        None => return http_response(400, "text/plain", b"missing filename"),
    };

    const ALLOWED: &[&str] = &["prompt.md", "response.md", "status.json"];
    if !ALLOWED.contains(&filename) {
        return http_response(400, "text/plain", b"invalid filename");
    }

    if node_id.contains("..") || node_id.contains('/') || node_id.contains('\\') {
        return http_response(400, "text/plain", b"invalid node_id");
    }

    let file_path = state.logs_root.join(node_id).join(filename);
    match std::fs::read(&file_path) {
        Ok(content) => {
            let ct = if filename.ends_with(".json") {
                "application/json"
            } else {
                "text/plain; charset=utf-8"
            };
            http_response(200, ct, &content)
        }
        Err(_) => http_response(404, "text/plain", b"Not Found"),
    }
}

// ---------------------------------------------------------------------------
// Control endpoints
// ---------------------------------------------------------------------------

fn control_start(state: &AppState) -> Vec<u8> {
    let mut trigger = state.start_trigger.lock().unwrap();
    if let Some(tx) = trigger.take() {
        let _ = tx.send(());
        http_response(200, "text/plain", b"")
    } else {
        http_response(409, "text/plain", b"already started")
    }
}

fn control_pause(state: &AppState) -> Vec<u8> {
    state.control.pause();
    http_response(200, "text/plain", b"")
}

fn control_resume(state: &AppState) -> Vec<u8> {
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
    http_response(200, "text/plain", b"")
}

fn control_stop(state: &AppState) -> Vec<u8> {
    state.control.cancel();
    // Also trigger start if pending, so the pipeline thread can exit
    let mut trigger = state.start_trigger.lock().unwrap();
    if let Some(tx) = trigger.take() {
        let _ = tx.send(());
    }
    http_response(200, "text/plain", b"")
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
// Bridge loop: tokio mpsc → event_log + notify WS clients
// ---------------------------------------------------------------------------

async fn bridge_loop(state: Arc<AppState>, mut rx: forge_attractor::RuntimeEventReceiver) {
    while let Some(event) = rx.recv().await {
        let json = serde_json::to_string(&event).unwrap_or_default();
        update_status(&state, &event);
        {
            let mut log = state.event_log.lock().unwrap();
            log.push(json);
        }
        // Wake all WS client tasks
        state.event_notify.notify_waiters();
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
// Low-level helpers
// ---------------------------------------------------------------------------

/// Find the end of HTTP headers (double CRLF).
fn find_header_end(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 4);
        }
    }
    None
}

/// Read some bytes from a TcpStream (async).
async fn read_some(stream: &mut TcpStream, buf: &mut [u8]) -> Result<usize, std::io::Error> {
    use std::future::poll_fn;
    use std::pin::Pin;
    use asupersync::io::ReadBuf;

    poll_fn(|cx| {
        let mut read_buf = ReadBuf::new(buf);
        match Pin::new(&mut *stream).poll_read(cx, &mut read_buf) {
            std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(read_buf.filled().len())),
            std::task::Poll::Ready(Err(e)) => std::task::Poll::Ready(Err(e)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }).await
}
