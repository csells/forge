# Spec 07 — Asupersync Runtime Migration

**Status:** In Progress — Phase 5b (watch server WebSocket) complete; Phases 1–4 pending
**Scope:** Full workspace — forge-cli, forge-attractor, forge-agent, forge-llm, forge-cxdb-runtime
**Goal:** Replace tokio with asupersync as the sole async runtime across the entire Forge stack.

## 1. Motivation

Forge currently uses tokio as its async runtime across all crates. This creates several problems:

1. **Silent cancellation data loss.** Tokio drops futures on cancellation with no drain phase. A cancelled `tx.send()` silently loses the message. asupersync's two-phase reserve/commit pattern makes this structurally impossible.

2. **Orphaned tasks.** `tokio::spawn` creates detached tasks with no ownership. If the spawner dies, the task runs forever. asupersync's region model guarantees that when a scope closes, all child tasks reach terminal state.

3. **Ambient authority.** Any code can call `tokio::spawn()` or `tokio::time::sleep()` without explicit capability. asupersync requires an explicit `Cx` token for privileged operations, making authority flow visible and auditable.

4. **No structured concurrency.** Tokio's task model is flat — there's no parent-child relationship between tasks. asupersync enforces hierarchical region ownership where parent close cascades to children.

5. **Mixed runtime complexity.** The `forge-cli watch` command currently runs asupersync for TCP/WebSocket serving alongside two separate tokio threads (pipeline runner and event bridge) — requiring `std::sync` primitives and thread-boundary coordination. A single runtime eliminates this architectural complexity. **Note:** The watch server was already migrated to use asupersync for TCP/WS (Phase 5b complete), but the pipeline and bridge threads still use tokio internally pending Phases 1–4.

6. **WebSocket-native event streaming.** The previous long-poll SSE implementation was a hack forced by the inability to upgrade HTTP connections within `Http1Listener`. This has been replaced with native WebSocket (Phase 5b complete). The remaining goal is to use WebSocket for all real-time communication — including control commands and artifact requests — once the full single-runtime unification is complete.

**Note on event delivery semantics:** The cancellation data-loss protection in point 1 refers specifically to asupersync's two-phase reserve/commit pattern for channel operations — a reserved send cannot be lost on cancellation. This is distinct from the backpressure-induced `try_send` event drops in §3.8, which are a deliberate best-effort design choice for the event observation path. Pipeline data integrity uses the cancel-safe patterns; event observation uses lossy best-effort delivery. These are different categories.

## 2. Target Architecture

After migration, Forge runs on a single asupersync runtime. No tokio dependency anywhere in the workspace.

```
                    ┌─────────────────────────────────────┐
                    │        asupersync runtime            │
                    │   RuntimeBuilder::current_thread()   │
                    │   .blocking_threads(2, 16)           │
                    │                                      │
                    │  ┌─────────┐  ┌──────────┐          │
                    │  │ HTTP/WS │  │ Pipeline │          │
                    │  │ Server  │  │ Engine   │          │
                    │  └────┬────┘  └────┬─────┘          │
                    │       │            │                 │
                    │       │  broadcast │                 │
                    │       │◄───channel─┘                 │
                    │       │                              │
                    │  ┌────┴────┐                         │
                    │  │   WS    │  (per-client tasks)     │
                    │  │ Clients │                         │
                    │  └─────────┘                         │
                    └─────────────────────────────────────┘
```

**Key properties:**
- Single process, single runtime, single thread (current_thread flavor) with a blocking thread pool (min 2, max 16).
- Filesystem I/O, synchronous subprocess waits, and heavy JSON serialization run on the blocking pool via `spawn_blocking` — never on the reactor thread.
- Event flow: pipeline → bounded mpsc (try_send) → bridge task → broadcast channel → WebSocket client tasks. No condvars, no bridge threads.
- HTTP only serves the initial HTML page. Everything else flows over WebSocket.
- The `Cx` capability token is threaded explicitly through all async call stacks (see §3).

**Blocking pool sizing rationale:** A parallel pipeline can run up to 3 concurrent CLI agents (Claude Code, Codex, Gemini), each consuming a blocking thread for `child.wait()`. Additionally, filesystem I/O (`asupersync::fs::*`) and JSON serialization use the blocking pool. With max 16 threads, the pool can handle 3 CLI waits + 9 concurrent fs operations + 4 headroom. Min 2 ensures the pool is always warm enough for immediate fs operations even when no CLI agents are running. If the pool reaches capacity, additional `spawn_blocking` calls will queue rather than stall the reactor — but this should be logged as a warning via a pool-capacity metric.

## 3. Cx Propagation Strategy

The most pervasive API difference between tokio and asupersync is the `Cx` capability token. In tokio, `spawn`, `sleep`, channel `send`/`recv`, and `Mutex::lock` are all callable without any explicit token. In asupersync, most of these require `&Cx`. This isn't a cosmetic change — it ripples through function signatures across crate boundaries.

### 3.1 Where `Cx` is NOT Required

These asupersync APIs work without `&Cx` (verified against v0.2.6 source):
- `Notify::notified()`, `notify_one()`, `notify_waiters()` — pure waker operations
- `channel::mpsc::Sender::try_send(val)` — non-blocking, synchronous
- `channel::mpsc::Sender::try_reserve()` — non-blocking, synchronous
- `channel::mpsc::Receiver::try_recv()` — non-blocking, returns `Result<T, RecvError>` (variant `Empty` when no message)
- `asupersync::time::wall_now()` — reads wall clock (always real time, even under LabRuntime)
- `asupersync::fs::*` — uses internal `spawn_blocking`, no `Cx` parameter
- `asupersync::process::Command::spawn()` — synchronous spawn
- `asupersync::process::Child::wait()` — synchronous (blocking) wait
- `asupersync::process::Child::kill()` — synchronous SIGKILL

### 3.2 Where `Cx` IS Required

These APIs require `&Cx` as a parameter:
- `channel::mpsc::Sender::send(&cx, val)` and `reserve(&cx)` — async
- `channel::mpsc::Receiver::recv(&cx)` — async, returns `Result<T, RecvError>` (variants: `Disconnected`, `Cancelled`)
- `channel::broadcast::Sender::send(&cx, msg)` — **sync but requires &Cx** (reserves slot internally). Returns `Result<usize, SendError<T>>`.
- `channel::broadcast::Receiver::recv(&cx)` — async, returns `Result<T, broadcast::RecvError>` (variants: `Lagged(u64)`, `Closed`, `Cancelled`)
- `sync::Mutex::lock(&cx)` — returns `LockFuture` which resolves to `MutexGuard`. **Note:** If `Cx` is unavailable (e.g., synchronous callback), use `std::sync::Mutex` instead.
- `sync::RwLock::read(&cx)` / `write(&cx)` — async
- `sync::Semaphore::acquire(&cx, n)` — async
- `scope.spawn(state, cx, |cx| async move { ... })` — creates child task with child Cx (see §3.5)
- `scope.race(cx, h1, h2)` — races two handles (same output type required), returns `Result<T, JoinError>`
- `WebSocketAcceptor::accept(&cx, request_bytes, stream)` — async
- `ws.send(&cx, msg)` / `ws.recv(&cx)` — async

### 3.3 Cancellation Checkpoints

The cancellation checkpoint API takes `&self` (verified, cx.rs:1090):

```rust
pub fn checkpoint(&self) -> Result<(), Cancelled>
```

This is NOT `&mut self`. Functions that use checkpoints take `&Cx`, same as everything else:

```rust
async fn worker(cx: &Cx) -> Result<(), Error> {
    loop {
        cx.checkpoint()?;  // Cancellation check — &self, not &mut
        // do work...
    }
}
```

**Rule:** Long-running loops (pipeline runner main loop, agent tool loops) MUST insert `cx.checkpoint()?` at each iteration boundary. This cooperatively checks whether the `Cx` has been cancelled and returns `Err(Cancelled)` if so. Since it takes `&self`, it composes naturally with all other `&Cx` operations.

### 3.4 Root Cx Acquisition and Scope Creation

**`RuntimeBuilder::block_on` does NOT provide a Cx.** Its signature (verified, builder.rs:771):
```rust
pub fn block_on<F: Future>(&self, future: F) -> F::Output
```

Root `Cx` must be obtained explicitly. The scope is obtained from the `Cx`:

**Production entry point:**
```rust
let runtime = RuntimeBuilder::current_thread()
    .blocking_threads(2, 16)
    .build()?;

// Save a handle for unstructured spawning (e.g., per-client WS tasks)
let handle = runtime.handle();

runtime.block_on(async {
    let cx = Cx::for_request();  // Creates root Cx with unique IDs + infinite budget
    let scope = cx.scope();       // Get root scope from Cx
    let mut state = RuntimeState::new();

    // Create a child region for the application
    scope.region(&mut state, &cx, FailFast, |app_scope, state| async move {
        run_application(&cx, app_scope, state, config).await
    }).await
});
```

**Test entry point — two options (verified, test_utils.rs:116-129):**
```rust
// Option 1: run_test — does NOT pass Cx to closure
#[test]
fn my_test() {
    asupersync::test_utils::run_test(|| async {
        let cx = Cx::for_testing();  // Create Cx explicitly
        do_stuff(&cx).await;
    });
}

// Option 2: run_test_with_cx — passes owned Cx to closure (preferred)
#[test]
fn my_test_with_cx() {
    asupersync::test_utils::run_test_with_cx(|cx| async move {
        do_stuff(&cx).await;  // Cx provided by the test harness
    });
}
```

**When to use which:** Use `run_test_with_cx` for tests that exercise `Cx`-dependent APIs (channels, spawn, Mutex). Use `run_test` + `Cx::for_testing()` for tests that need simpler setup or multiple independent `Cx` instances.

### 3.5 Scoped Spawn and Region APIs (Full Signatures)

**`scope.spawn` (verified, scope.rs:287):**
```rust
pub fn spawn<F, Fut, Caps>(
    &self,
    state: &mut RuntimeState,
    cx: &Cx<Caps>,
    f: F,
) -> Result<(TaskHandle<Fut::Output>, StoredTask), SpawnError>
```

**`scope.region` — method on `Scope`, NOT a static function (verified, scope.rs:775):**
```rust
pub async fn region<P2, F, Fut, T, Caps>(
    &self,
    state: &mut RuntimeState,
    cx: &Cx<Caps>,
    policy: P2,       // e.g., FailFast (default policy)
    f: F,
) -> Result<Outcome<T, P2::Error>, RegionCreateError>
where
    P2: Policy,
    F: FnOnce(Scope<'_, P2>, &mut RuntimeState) -> Fut,
    Fut: Future<Output = Outcome<T, P2::Error>>,
```

**Key insight: the closure receives BOTH the child `Scope` AND `&mut RuntimeState`.** This solves the `&mut RuntimeState` propagation problem — inner code can spawn via the child scope using the state it received. No need to thread state through separate function parameters:

```rust
let scope = cx.scope();
scope.region(&mut state, &cx, FailFast, |child_scope, state| async move {
    // Inside the region, we have the child scope AND mutable state
    let (handle, _) = child_scope.spawn(state, &cx, |child_cx| async move {
        // IMPORTANT: child_cx is a CHILD Cx — different from the outer &cx.
        // The |child_cx| closure parameter receives a new Cx with its own cancellation
        // token. Always use the child Cx inside spawn closures. Name-shadowing
        // (|cx| instead of |child_cx|) is common and intentional to prevent
        // accidentally using the parent Cx.
        do_work(&child_cx).await;
        Outcome::Ok(42)
    })?;

    let result = handle.join(&cx).await;
    // handle.join returns Result<T, JoinError> — T is 42 here
    Outcome::Ok(result)
}).await
```

**`Outcome<T, E>` — four-way result type (verified, outcome.rs:207):**
```rust
pub enum Outcome<T, E> {
    Ok(T),
    Err(E),
    Cancelled(CancelReason),
    Panicked(PanicPayload),
}
```

Region closures MUST return `Outcome<T, E>`, not `Result<T, E>`. This is a fundamental difference from tokio — cancellation and panic are explicit outcomes, not hidden.

**Getting a Scope:** Use `cx.scope()` (verified, cx.rs:2087) which returns `Scope<'static>` with the Cx's budget. There is no `Scope::region(...)` static function — `region` is always called on an existing `Scope` instance.

**`RuntimeHandle` for unstructured spawn (verified, builder.rs:848):**
```rust
// Get handle before block_on (RuntimeHandle is Send + Clone)
let handle = runtime.handle();

// Later, in any async context:
let join_handle = handle.spawn(async { /* fire-and-forget */ });
```

**Note:** There is NO `RuntimeHandle::current()` method. You must get the handle via `runtime.handle()` at initialization and pass it through (e.g., store in shared state or pass as a function argument).

### 3.6 RuntimeState Propagation

`scope.spawn` and `scope.region` require `&mut RuntimeState`. This is NOT threaded through every function signature like `&Cx`. Instead, the architecture concentrates spawning at orchestration boundaries:

**Strategy: Orchestration boundaries own `RuntimeState`, inner code is spawn-free.**

| Layer | Has `&mut RuntimeState`? | Can spawn? |
|-------|--------------------------|------------|
| `main()` / entry point | Yes | Yes — root scope |
| `Runner::run()` (pipeline orchestrator) | Yes — received from region closure | Yes — spawns node handlers |
| `Session::submit()` (agent orchestrator) | Yes — received from region closure | Yes — spawns tool tasks |
| `AgentProvider::run_to_completion()` | **No** | No — uses `handle.spawn` (unstructured) or `spawn_blocking` for CLI subprocess I/O. Cannot use `scope.spawn` without RuntimeState. |
| `NodeHandler::execute()` | No | No — returns result, caller spawns |
| Tool implementations | No | No — uses `spawn_blocking` (free function, no state needed) |
| `ProviderAdapter::complete/stream` | No | No — pure async I/O |

**CLI adapter spawning note:** The CLI adapter pattern in §4.5 shows `scope.spawn` for the stdout reader, stderr drainer, and child wait tasks. In practice, since `AgentProvider::run_to_completion()` does NOT receive `RuntimeState`, the CLI adapters must use `handle.spawn(fut)` (unstructured) instead of `scope.spawn(state, &cx, ...)`. This means CLI subprocess tasks are fire-and-forget — they are not structurally owned by a scope. This is acceptable because: (1) CLI subprocesses have their own lifecycle via `child.wait()`, and (2) the process group timeout (§4.5) provides a hard kill as a safety valve. The alternative — threading `RuntimeState` through `AgentProvider` — would require changes to the trait signature across all providers, which is deferred.

**This means most trait signatures only gain `cx: &Cx`, NOT `state: &mut RuntimeState`.** Only the top-level orchestrators (Runner, Session) receive state from their parent region closures. Inner code that needs blocking I/O uses the free function `asupersync::runtime::spawn_blocking(f)` which does NOT require state.

### 3.7 Propagation Patterns

**Pattern 1: Function parameters carry `&Cx` explicitly.**
```rust
// Before (tokio)
async fn process_node(node: &Node) -> Result<()> { ... }

// After (asupersync)
async fn process_node(cx: &Cx, node: &Node) -> Result<()> { ... }
```

**Pattern 2: Trait methods gain a `cx` parameter.**
```rust
// Before
#[async_trait]
trait NodeHandler {
    async fn execute(&self, ctx: &RuntimeContext) -> Result<NodeOutcome>;
}

// After
#[async_trait]
trait NodeHandler {
    async fn execute(&self, cx: &Cx, ctx: &RuntimeContext) -> Result<NodeOutcome>;
}
```

**Note on `async_trait` + `&Cx` + Send bounds:** `Cx` is `Send + Sync + Clone` (verified — uses `Arc<RwLock>` internally, manual `Clone` impl at cx.rs:151 clones the inner `Arc`s without requiring `Caps: Clone`). Adding `cx: &Cx` to `#[async_trait]` methods works without issues because `&Cx` is `Send`. This spec does NOT replace `async_trait` with native `async fn in traits` — that is a separate refactor explicitly deferred to a follow-up effort. The boxing overhead of `async_trait` is a known cost that can be optimized post-migration.

**Note on `Cx<Caps>` generic:** `Cx` is generic over a `Caps` type parameter for capability subsetting in advanced scenarios. Forge does not currently use capability subsetting — all `Cx` instances use the default `Cx<()>`. The generic parameter can be ignored for the purposes of this migration; it does not affect function signatures beyond what is shown in this spec.

### 3.8 Impact by Crate

| Crate | Trait signatures affected | Function signatures affected |
|-------|--------------------------|------------------------------|
| forge-attractor | `NodeHandler::execute`, `AgentSubmitter::submit_with_result` | `Runner::run`, `Runner::execute_node`, handler dispatch |
| forge-agent | `ExecutionEnvironment` methods, `ToolRegistry` methods | `Session::submit`, `Session::run_tool_loop`, tool implementations |
| forge-llm | `ProviderAdapter::complete`/`stream`, `AgentProvider::run_to_completion` | `LlmClient::complete`, stream parser tasks |
| forge-cxdb-runtime | `CxdbHttpClient` methods (async, currently use reqwest internally) | Test helpers only |
| forge-cli | None (entry point only) | `run_pipeline`, `run_command`, `resume_command`, `watch::run` |

**Note:** `RuntimeEventSink::emit` does NOT gain `&Cx` — see §3.9 for why.

### 3.9 RuntimeEventSink::emit — Sync Preservation via try_send

**This is the single most invasive potential change in the migration — and we avoid it.**

The current `RuntimeEventSink::emit()` (events.rs:206) is synchronous:
```rust
pub fn emit(&self, event: RuntimeEvent) {
    if let Some(observer) = self.observer.as_ref() {
        observer.on_event(&event);
    }
    if let Some(sender) = self.sender.as_ref() {
        let _ = sender.send(event);  // tokio unbounded — non-blocking
    }
}
```

It is called from 22 sites in `runner.rs` via the `emit_runtime_event()` helper function — all in async contexts, but through a **synchronous wrapper**. Making `emit()` async would require:
1. Making `emit_runtime_event()` async
2. Adding `.await` at all 22 call sites
3. Threading `&Cx` through every function that emits events

**We avoid this entirely by using `try_send()`.** asupersync's `mpsc::Sender::try_send(val)` is synchronous and does NOT require `&Cx` (verified, mpsc.rs:241). With a bounded channel of 1024:

```rust
pub fn emit(&self, event: RuntimeEvent) {
    if let Some(observer) = self.observer.as_ref() {
        observer.on_event(&event);
    }
    if let Some(sender) = self.sender.as_ref() {
        match sender.try_send(event) {
            Ok(()) => {}
            Err(SendError::Full(_)) => {
                // Channel full — transient backpressure. Increment counter, move on.
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
            }
            Err(SendError::Disconnected(_)) => {
                // Receiver dropped — event subsystem is broken. Log at WARN level.
                // This shouldn't happen during normal operation.
                tracing::warn!("event channel receiver dropped — events will be lost");
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
            }
            Err(SendError::Cancelled(_)) => {
                // Cx cancelled — runtime is shutting down. Expected during shutdown.
                self.dropped_events.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
```

**Tradeoff:** Events can be dropped if the channel is full. This is acceptable because:
- Events are observation-path data (UI updates), not pipeline-critical data
- 1024 slots provides ~minutes of headroom for small JSON events
- The alternative (async emit + Cx threading) touches 22+ call sites for zero practical benefit
- The `dropped_events` counter is exposed via the watch UI for observability
- The observer callback (`on_event`) still fires synchronously — only the channel send can drop

**Event delivery SLA:** Best-effort. The pipeline never blocks on event delivery. There are two categories of event loss:
1. **Pre-bridge drops** (at `try_send` in `emit()`): Counted by `dropped_events` atomic counter. These drops happen BEFORE `seq` assignment, so clients CANNOT detect them via sequence number gaps. The `dropped_events` metric is the ONLY way to observe pre-bridge loss.
2. **Post-bridge drops** (broadcast `Lagged`): The bridge assigns `seq` before broadcasting, so clients CAN detect these via `RecvError::Lagged(n)`. The client receives a `lag` message with the count of missed events.

This is explicitly NOT "durable delivery" — that would require the pipeline to block on a full channel, which defeats the purpose.

**forge-agent unbounded channel:** `forge-agent/src/events.rs` uses `futures::channel::mpsc::unbounded()` (NOT `tokio::sync::mpsc`). Apply the same bounded + `try_send()` pattern: `mpsc::channel(512)`. Agent events are less frequent than pipeline events, so 512 capacity is generous.

## 4. API Mapping

This section maps every tokio API used in Forge to its asupersync equivalent, with accurate return types and semantic differences verified against v0.2.6 source code.

### 4.1 Channels

#### MPSC (bounded)

| Tokio | Asupersync | Semantic difference |
|-------|------------|---------------------|
| `mpsc::channel::<T>(cap)` | `mpsc::channel::<T>(cap)` | Same |
| `tx.send(val).await` → `Result<(), SendError<T>>` | `tx.send(&cx, val).await` → `Result<(), SendError<T>>` | Requires `&Cx` |
| `tx.try_send(val)` → `Result<(), TrySendError<T>>` | `tx.try_send(val)` → `Result<(), SendError<T>>` | Same semantics, slightly different error type |
| `rx.recv().await` → `Option<T>` | `rx.recv(&cx).await` → `Result<T, RecvError>` | **Different return type.** Handle `RecvError::Disconnected` (all senders dropped) and `RecvError::Cancelled` (Cx cancelled) instead of `None`. |
| — | `rx.try_recv()` → `Result<T, RecvError>` | Non-blocking. `RecvError::Empty` when no message, `::Disconnected` when closed. |
| — | `tx.reserve(&cx).await` → `permit.send(val)` | Two-phase reserve/commit (cancel-safe) — new capability |
| — | `tx.try_reserve()` → `Result<SendPermit, SendError>` | Non-blocking reserve, no `&Cx` |

**mpsc RecvError variants (verified, mpsc.rs:57):**
- `Disconnected` — all senders dropped
- `Cancelled` — Cx was cancelled
- `Empty` — channel empty (for `try_recv` only)

#### MPSC (unbounded → bounded conversion)

**Critical:** Forge uses unbounded channels in several places:
- `forge-attractor/src/events.rs` — `tokio::sync::mpsc::unbounded_channel()` for pipeline events
- `forge-agent/src/events.rs` — `futures::channel::mpsc::unbounded()` for agent events
- `forge-llm/src/openai.rs`, `anthropic.rs`, `high_level.rs` — `futures::channel::mpsc::unbounded()` for stream parser channels

Asupersync does **not** provide unbounded channels by design — unbounded channels violate backpressure principles.

**Migration strategy:** Convert all unbounded channels to bounded channels with appropriate capacity:
- `forge-attractor` event channel: `mpsc::channel(1024)` — pipeline events are small JSON; 1024 provides ample headroom. Uses `try_send()` to preserve sync `emit()` signature (see §3.9).
- `forge-agent` event channel: `mpsc::channel(512)` — agent events are less frequent. Same `try_send()` pattern.
- `forge-llm` stream parser channels: `mpsc::channel(256)` — SSE events arrive sequentially from a single HTTP stream; 256 is generous.

#### Broadcast (for WebSocket event fan-out)

| API | Signature | Notes |
|-----|-----------|-------|
| `broadcast::channel::<T>(cap)` | Returns `(Sender<T>, Receiver<T>)` | **Size must be >= ring buffer capacity** (4096) to avoid spurious Lagged errors |
| `tx.send(&cx, msg)` | `Result<usize, SendError<T>>` — **sync but requires &Cx** | Reserves slot via internal reserve/commit |
| `rx.recv(&cx).await` | `Result<T, broadcast::RecvError>` — may return `RecvError::Lagged(n)` | |
| `tx.subscribe()` | Creates new receiver (late joiners) | |
| — | No `try_send` on broadcast::Sender | Must have `&Cx` available |

**Broadcast RecvError variants (verified, broadcast.rs:49):**
- `Lagged(u64)` — receiver fell behind by `n` messages, some were evicted from the ring
- `Closed` — all senders dropped (**NOT `Disconnected`** — broadcast uses different variant name than mpsc)
- `Cancelled` — the `Cx` was cancelled

**Note on broadcast send requiring &Cx:** The bridge task (pipeline mpsc → broadcast) runs in an async context with `Cx` available, so this is not a problem. The bridge task is the only sender to the broadcast channel.

**Broadcast send semantics (verified, broadcast.rs:155-181):** `broadcast::Sender::send(&cx, msg)` is **truly synchronous** — it calls `reserve(&cx)` which checks for active receivers and returns immediately (no `.await`), then `permit.send(msg)` which appends to the ring buffer. If the ring buffer is full, the **oldest message is evicted** (overwritten) — broadcast channels never block senders. The `&Cx` parameter is used only for tracing cancellation state, not for suspension. Returns `Result<usize, SendError<T>>` where `usize` is the number of receivers that will see the message, and `SendError::Closed(msg)` if no receivers exist.

### 4.2 Synchronization Primitives

| Tokio | Asupersync | Notes |
|-------|------------|-------|
| `tokio::sync::Mutex` | `asupersync::sync::Mutex` | `lock(&cx)` returns `LockFuture` → `MutexGuard`. **Use `std::sync::Mutex` in sync contexts where &Cx is unavailable.** |
| `tokio::sync::RwLock` | `asupersync::sync::RwLock` | `read(&cx)` / `write(&cx)` — async |
| `tokio::sync::Notify` | `asupersync::sync::Notify` | `notified().await` / `notify_one()` / `notify_waiters()` — does NOT require `&Cx` |
| `tokio::sync::Semaphore` | `asupersync::sync::Semaphore` | `acquire(&cx, n)` — async |

**`MutexGuard` IS `Send` for `T: Send` (verified, mutex.rs:320).** However, holding any lock guard across `.await` points in a single-threaded runtime creates deadlock risk — if the same task (or any other task on the sole thread) tries to re-acquire the lock while the future is suspended, it blocks forever. Avoid holding guards across yields:

```rust
// WRONG — deadlock risk in single-threaded runtime
let guard = mutex.lock(&cx).await;
do_something_async(&guard).await;  // guard held across yield

// CORRECT — extract data, drop guard, then await
let data = {
    let guard = mutex.lock(&cx).await;
    guard.clone()  // or extract what you need
};  // guard dropped here
do_something_async(&data).await;
```

**Mutex audit requirement:** Before migrating each crate, audit all `tokio::sync::Mutex` usages. If the lock is acquired in a synchronous callback or a context without `Cx`, replace with `std::sync::Mutex` instead. The pipeline control fields (`PipelineControl`) currently use `AtomicBool` + `Notify` — these do not require `Cx` and are unaffected.

**Used in:** `forge-agent/src/session/mod.rs` (Notify for abort signaling), `forge-attractor/src/runtime.rs` (PipelineControl uses Notify).

### 4.3 Time

| Tokio | Asupersync | Notes |
|-------|------------|-------|
| `tokio::time::sleep(dur)` | `sleep(wall_now(), dur).await` | Requires current time; `wall_now()` for production |
| `tokio::time::timeout(dur, fut)` | `timeout(wall_now(), dur, fut).await` | Same — requires current time |
| `tokio::time::Instant::now()` | `asupersync::time::wall_now()` | Returns `Time` (nanosecond-precision, elapsed since module clock baseline) |
| `tokio::time::Duration` | `std::time::Duration` | Standard library type in both |

**On `wall_now()` and testability (verified against source):** `wall_now()` (sleep.rs:94) ALWAYS returns real wall-clock time — it uses `std::time::Instant` internally and is NOT intercepted by `LabRuntime` or `VirtualClock`. Code that needs deterministic time must use `cx.timer_driver().map_or_else(wall_now, |d| d.now())` — the timer driver IS virtualized under `LabRuntime`.

**On `sleep` and authority:** `asupersync::time::sleep(time, dur)` is a **free function** that does NOT require `&Cx`. It relies on the runtime's internal timer driver (thread-local). This is a pragmatic exception to asupersync's explicit-authority model — the `Cx` is required for *cancellable* operations (channels, locks, spawn), while sleep is always fire-and-forget. For cancellable delays, use `timeout(wall_now(), dur, cx.checkpoint())` instead.

**Ergonomic helper (revised for testability):**
```rust
/// Sleep for a duration. Uses virtual time under LabRuntime, wall time in production.
pub async fn sleep_for(cx: &Cx, dur: Duration) {
    let now = cx.timer_driver()
        .map_or_else(asupersync::time::wall_now, |d| d.now());
    asupersync::time::sleep(now, dur).await;
}
```

### 4.4 Task Spawning

| Tokio | Asupersync | Notes |
|-------|------------|-------|
| `tokio::spawn(fut)` | `scope.spawn(state, &cx, \|cx\| async move { ... })` | **Structured:** task owned by scope; requires `&mut RuntimeState` + `&Cx`; closure receives child `Cx` |
| `tokio::spawn(fut)` (detached) | `handle.spawn(fut)` | Via `RuntimeHandle` (obtained from `runtime.handle()`); returns `JoinHandle<T>` — use for fire-and-forget tasks like WS client handlers |
| `tokio::task::yield_now()` | `asupersync::runtime::yield_now()` | Same semantics |
| `tokio::task::JoinHandle<T>` | `JoinHandle<T>` or `TaskHandle<T>` | `JoinHandle` from `handle.spawn()`, `TaskHandle` from `scope.spawn()` |
| `tokio::task::spawn_blocking(f)` | `asupersync::runtime::spawn_blocking(f).await` | **Free function**, does NOT require RuntimeState or Cx |
| `tokio::task::spawn_blocking(f)` | `scope.spawn_blocking(state, cx, f)` | **Scope-owned** variant, returns `Result<(TaskHandle<R>, StoredTask), SpawnError>` |

**`select!` migration** — This is NOT mechanical. `tokio::select!` is a macro with N branches, implicit cancellation safety, and pin projection. **asupersync has NO `select!` macro** (verified). Two alternatives:

| Pattern | When to use |
|---------|-------------|
| `Select::new(f1, f2).await` → `Either::Left(v) \| Either::Right(v)` | Low-level, 2 futures only, **must manually handle loser** — the dropped future may have partial state. `Select` requires pinned futures (`pin!` or `Box::pin`). |
| `scope.race(cx, h1, h2).await` → `Result<T, JoinError>` (verified, scope.rs:906) | **Preferred.** Auto-cancels and drains the loser via 3-phase protocol. Requires `TaskHandle` (spawned tasks). Both handles must have **same output type `T`**. The winner's inner `T` is returned directly — NOT wrapped in `Outcome`. If the winner panicked/cancelled, `Err(JoinError)` is returned. |
| `scope.race_all(cx, handles).await` → `Result<(T, usize), JoinError>` (verified, scope.rs:1036) | For N > 2 handles. Returns winner value + index. **All handles must have same output type.** |

**Actual `tokio::select!` sites in the workspace (verified via grep):**

| Crate | Count | Location |
|-------|-------|----------|
| forge-agent | 1 | `session/mod.rs:568` — abort watchdog racing |
| forge-attractor | 0 | None |
| forge-llm | 0 | None |
| forge-cxdb-runtime | 0 | None |
| forge-cli | 0 | None |
| **Total** | **1** | |

**Before/after example (the one `select!` site — abort watchdog in session):**
```rust
// BEFORE (tokio) — forge-agent/src/session/mod.rs:568
tokio::select! {
    result = session.submit(msg) => { handle_result(result) }
    _ = abort_notify.notified() => { handle_abort() }
}

// AFTER (asupersync) — scope.race with spawned handles
// Both handles must return the same type, so we unify on Result<SubmitResult, AgentError>
let scope = cx.scope();
scope.region(&mut state, &cx, FailFast, |child_scope, state| async move {
    let (submit_handle, _) = child_scope.spawn(state, &cx, |cx| async move {
        match session.submit(&cx, msg).await {
            Ok(r) => Outcome::Ok(Ok(r)),
            Err(e) => Outcome::Ok(Err(e)),
        }
    })?;
    let (abort_handle, _) = child_scope.spawn(state, &cx, |_cx| async move {
        abort_notify.notified().await;
        Outcome::Ok(Err(AgentError::Aborted))
    })?;
    // race auto-cancels and drains the loser — returns Result<T, JoinError>
    match child_scope.race(&cx, submit_handle, abort_handle).await {
        Ok(result) => Outcome::Ok(result),
        Err(e) => Outcome::Err(e.into()),
    }
}).await
```

### 4.5 Process Spawning

| Tokio | Asupersync | Semantic difference |
|-------|------------|---------------------|
| `tokio::process::Command` | `asupersync::process::Command` | Same builder API |
| `cmd.spawn()` | `cmd.spawn()` | Returns `Child` handle |
| `child.stdout` (field, `Option<ChildStdout>`) | `child.stdout()` → `Option<ChildStdout>` (method, takes ownership internally via `.take()`) | **Field vs method.** `child.stdout()` already performs an internal `.take()` — calling it twice returns `None`. Do NOT write `child.stdout().take()`. |
| `child.wait().await` (async) | `child.wait()` (**synchronous, blocking**) | **Critical difference:** asupersync's `wait()` is `pub fn wait(&mut self) -> Result<ExitStatus>`, NOT async. Must run via `spawn_blocking` to avoid stalling the reactor. |
| `child.kill()` | `child.kill()` | Sends SIGKILL |
| `child.id()` | `child.id()` → **`Option<u32>`** | Returns `None` if process already waited on |
| `tokio::io::BufReader` | `asupersync::io::BufReader` | 8KB default buffer |
| `AsyncBufReadExt::lines()` | `Lines::new(buf_reader)` | Returns `Stream<Item = io::Result<String>>` |

**Process wait pattern (required):**
```rust
// WRONG — blocks the reactor thread
let status = child.wait()?;

// CORRECT — offload to blocking pool
// spawn_blocking returns T directly (verified, spawn_blocking.rs:158): async fn spawn_blocking<F, T>(f: F) -> T
// NOT Result<T, JoinError>. Panics inside the closure are re-raised when awaited.
// child.wait() returns Result<ExitStatus, ProcessError>, so only one ? is needed:
let status = asupersync::runtime::spawn_blocking(move || child.wait()).await?;
```

**CLI adapter full lifecycle pattern (correct ownership transfer):**

The CLI adapters (claude_code.rs, codex.rs, gemini.rs) need to read stdout while waiting for the child process. Since `child.wait()` takes `&mut self`, stdout must be extracted first:

```rust
let mut child = Command::new("claude")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

// 1. Extract stdout AND stderr BEFORE moving child into wait
//    child.stdout() performs internal .take() — returns Option<ChildStdout>
let stdout = child.stdout().expect("stdout configured");
let stderr = child.stderr().expect("stderr configured");
let reader = BufReader::new(stdout);
let framed = FramedRead::new(reader, LinesCodec::new());

// 2. Spawn tasks using handle.spawn (unstructured) — AgentProvider does NOT have RuntimeState
//    handle.spawn returns JoinHandle<T> which implements Future (can be .await'd)
let reader_handle = handle.spawn(async move {
    let mut events = Vec::new();
    let mut framed = framed;  // ensure mutability for .next()
    while let Some(line) = framed.next().await {
        let event: Event = serde_json::from_str(&line?)?;
        events.push(event);
    }
    Ok::<_, Error>(events)
});

// 3. Spawn a task to drain stderr (prevents pipe buffer deadlock)
let stderr_handle = handle.spawn(async move {
    let mut stderr_reader = BufReader::new(stderr);
    let mut buf = String::new();
    stderr_reader.read_to_string(&mut buf).await?;
    Ok::<_, Error>(buf)
});

// 4. Wait for child in blocking pool (child moved here — stdout/stderr already extracted)
let wait_handle = handle.spawn(async move {
    Ok::<_, Error>(asupersync::runtime::spawn_blocking(move || child.wait()).await?)
});

// 5. Await all three — JoinHandle<T> implements Future, .await returns T directly
let events = reader_handle.await?;   // Result<Vec<Event>, Error>
let stderr_out = stderr_handle.await?; // Result<String, Error>
let status = wait_handle.await?;       // Result<ExitStatus, Error>
```

**Timeout for runaway processes:**

When a subprocess exceeds a timeout, we need to kill it. Since `child` is moved into `spawn_blocking`, we use a oneshot channel to send the `Child` back after `wait()` completes, or kill it from outside using `child.kill()` BEFORE moving it:

```rust
// Option A: Use timeout on the spawn_blocking future (simplest)
let status = asupersync::time::timeout(
    asupersync::time::wall_now(),
    timeout_dur,
    asupersync::runtime::spawn_blocking(move || child.wait()),
).await;

match status {
    Ok(exit_status) => Ok(exit_status?),
    Err(_timeout) => {
        // The spawn_blocking future was cancelled, but the blocking thread
        // will still run to completion. The child process is orphaned on
        // the blocking thread — it will be reaped when wait() returns.
        // For aggressive cleanup: use process groups (setpgid + killpg).
        Err(Error::Timeout)
    }
}
```

**For aggressive timeout cleanup**, use process groups. **Note:** `asupersync::process::Command` does NOT expose `pre_exec()` or the `CommandExt` trait. Use `std::process::Command` to set up the process group, then convert or manage the child directly:
```rust
use std::process::Command as StdCommand;
use std::os::unix::process::CommandExt;

let mut std_cmd = StdCommand::new("claude");
std_cmd.stdout(std::process::Stdio::piped())
       .stderr(std::process::Stdio::piped());
unsafe { std_cmd.pre_exec(|| { libc::setpgid(0, 0); Ok(()) }); }
let child = std_cmd.spawn()?;  // std::process::Child
let pgid = child.id();  // u32 (std Child returns u32, not Option<u32>)

// On timeout: kill entire process group
unsafe { libc::killpg(pgid as i32, libc::SIGKILL); }
// Then reap: spawn_blocking(move || child.wait())
```

**Alternative without process groups:** Use `child.id()` (returns `Option<u32>` on asupersync Child) to send SIGKILL directly, then reap with a final `wait()`:
```rust
if let Some(pid) = child.id() {
    unsafe { libc::kill(pid as i32, libc::SIGKILL); }
}
```

**Used in:** `forge-llm/src/cli_adapters/` (claude_code.rs, codex.rs, gemini.rs — spawn CLI subprocess and read JSONL from stdout), `forge-agent/src/execution.rs` (shell tool, file operations).

### 4.6 Filesystem I/O

| Tokio | Asupersync | Notes |
|-------|------------|-------|
| `tokio::fs::read(path)` | `asupersync::fs::read(path)` | Returns `Vec<u8>` |
| `tokio::fs::read_to_string(path)` | `asupersync::fs::read_to_string(path)` | Returns `String` |
| `tokio::fs::write(path, data)` | `asupersync::fs::write(path, data)` | **Not atomic** — uses `std::fs::write` internally |
| `tokio::fs::create_dir_all(path)` | `asupersync::fs::create_dir_all(path)` | Recursive mkdir |
| `tokio::fs::remove_file(path)` | `asupersync::fs::remove_file(path)` | Delete file |
| `tokio::fs::rename(from, to)` | `asupersync::fs::rename(from, to)` | Atomic rename |
| `tokio::fs::metadata(path)` | `asupersync::fs::metadata(path)` | File metadata |

| `tokio::fs::File::open/create` | `asupersync::fs::File::open(path)` / `File::create(path)` | Async file handle (uses `spawn_blocking_io` internally) |
| — | `file.sync_all()` / `file.sync_data()` | Async fsync — required for crash-safe writes |

Both implementations use `spawn_blocking` under the hood. The API surface is virtually identical.

**Note on atomicity:** For checkpoint files and artifacts that require crash-safe writes, use the standard temp-file + fsync + rename pattern:
```rust
let tmp_path = path.with_extension("tmp");
let mut file = asupersync::fs::File::create(&tmp_path).await?;
file.write_all(data).await?;
file.sync_all().await?;  // fsync to disk
drop(file);
asupersync::fs::rename(&tmp_path, &path).await?;  // atomic rename
```

**Used in:** `forge-agent/src/execution.rs` (9 call sites — read, write, create_dir_all, remove_file, rename, metadata).

### 4.7 HTTP Client (`reqwest` → shared `forge-http` adapter)

**Shared adapter:** Phase 1 (forge-cxdb-runtime) and Phase 2a (forge-llm) both need a custom HTTP/1.1 adapter. To avoid duplicate code, extract a shared internal crate `forge-http` that sits BELOW both `forge-cxdb-runtime` and `forge-llm` in the dependency graph. **The adapter CANNOT live in `forge-llm`** — `forge-cxdb-runtime` is below `forge-llm` in the dep graph, so having `forge-cxdb-runtime` depend on `forge-llm` would create a cycle. The `forge-http` crate handles:

| reqwest | asupersync adapter | Notes |
|---------|-------------------|-------|
| `client.post(url).json(&body).send()` | `adapter.post_json(url, headers, body)` | Must serialize body and set Content-Type manually |
| `resp.bytes_stream()` | `adapter.post_streaming(url, headers, body)` → SSE stream | Streaming body via line parser |
| `resp.json::<T>()` | Manual: `serde_json::from_slice(&resp.body)` | No built-in JSON deserialization |

**HTTPS/TLS (v0.2.6) — ALPN bug confirmed in source:**

The `HttpClient` internally uses `TlsConnectorBuilder::new().alpn_http()` (http_client.rs:591) which calls `.alpn_protocols(vec![b"h2".to_vec(), b"http/1.1".to_vec()])` (connector.rs:333-334). This advertises HTTP/2 as preferred during TLS negotiation, but the client only implements an HTTP/1.1 codec. Servers that select h2 will cause connection failures.

**The asupersync skill reference (`api.md`) incorrectly claims this is fixed.** The v0.2.6 source code proves otherwise.

**Workaround (required):** Manual TLS with HTTP/1.1-only ALPN:
```rust
let tls = TlsConnectorBuilder::new()
    .alpn_protocols(vec![b"http/1.1".to_vec()])  // NOT .alpn_http()
    .with_webpki_roots()
    .build()?;
let tcp = TcpStream::connect(addr).await?;
let stream = tls.connect(domain, tcp).await?;
// Write raw HTTP/1.1 request, read response
```

**HTTP response parsing:** Use the `httparse` crate for zero-copy HTTP/1.x response parsing (status line, headers). `httparse` only parses headers — chunked transfer encoding must be handled separately. Use a simple chunked-decode state machine or the `chunked_transfer` crate for this.

```rust
// Parse HTTP response headers with httparse
let mut headers = [httparse::EMPTY_HEADER; 64];
let mut response = httparse::Response::new(&mut headers);
let status = response.parse(&buf)?;
match status {
    httparse::Status::Complete(body_offset) => { /* headers parsed, body starts at offset */ }
    httparse::Status::Partial => { /* need more data — read more into buf */ }
}
```

**HTTP adapter risk assessment:**

| Concern | OpenAI behavior | Anthropic behavior | Required handling |
|---------|----------------|-------------------|-------------------|
| Response framing | `Transfer-Encoding: chunked` for streaming, `Content-Length` for buffered | Same | Parse both chunked and content-length responses |
| SSE streaming | `data:` lines with `\n\n` boundaries | Same format | Line-by-line parser with event boundary detection |
| Error responses | JSON with `error.message` | JSON with `error.message` | Parse error JSON from non-2xx responses |
| Rate limiting | `429` + `Retry-After` header | `429` + `Retry-After` | Extract and honor Retry-After |
| Connection reuse | HTTP/1.1 keep-alive | HTTP/1.1 keep-alive | Connection pool keyed by host:port |
| Timeouts | Connect + read timeouts | Same | Configurable per-request |

**Estimated size:** 400-600 lines.

**Required test coverage:**
1. Happy path: buffered JSON POST → JSON response
2. Happy path: streaming POST → SSE events
3. Error path: non-2xx response → parsed error
4. Error path: 429 + Retry-After → correct backoff
5. Error path: connection reset mid-stream → clean error
6. Error path: partial chunked response → timeout/error
7. Connection reuse: two sequential requests reuse same TLS connection
8. Live provider test gate (OpenAI + Anthropic) — must pass before merge

**Fallback option:** If the custom adapter proves unreliable, consider `ureq` (sync HTTP client) wrapped in `spawn_blocking`. This avoids the ALPN bug entirely since ureq manages its own TLS stack.

**Used in:** `forge-llm/src/openai.rs` and `forge-llm/src/anthropic.rs` (complete + stream API calls), `forge-cxdb-runtime` (CXDB HTTP client).

### 4.8 Codec / Framing (new capability)

asupersync provides `LinesCodec` + `FramedRead` for JSONL parsing:

```rust
let stdout = child.stdout().expect("stdout configured");
let reader = BufReader::new(stdout);
let framed = FramedRead::new(reader, LinesCodec::new());
while let Some(line) = framed.next().await {
    let event: Event = serde_json::from_str(&line?)?;
    // process
}
```

**Note on `futures`/`futures-util` dependency:** The `futures` channel crate is removed (replaced by asupersync channels). However, **`futures-util` is retained** in `forge-llm` because the CLI adapter pattern above requires `StreamExt::next()` on `FramedRead` streams. asupersync does not re-export `StreamExt`. Per-crate summary:
- `forge-llm`: **retain `futures-util`** — needed for `StreamExt::next()` on `FramedRead` (CLI adapters) and `BoxFuture` in trait returns.
- `forge-agent`: remove `futures` channel; retain `futures-util` only if `StreamExt` or `BoxFuture` is used (check during Phase 3).
- `forge-attractor`: remove `futures` entirely if only channel was used.
- `forge-cxdb-runtime`: remove `futures` entirely.

### 4.9 Test Infrastructure

| Tokio | Asupersync | Notes |
|-------|------------|-------|
| `#[tokio::test(flavor = "current_thread")]` | `#[test]` + `run_test_with_cx(\|cx\| async move { ... })` | **Preferred** — Cx provided by test harness |
| `#[tokio::test(flavor = "current_thread")]` | `#[test]` + `run_test(\|\| async { ... })` | For Cx-independent tests; create via `Cx::for_testing()` if needed |
| `tokio::time::sleep` (in tests) | `sleep(cx.timer_driver().map_or_else(wall_now, \|d\| d.now()), dur)` | Use timer driver for virtual time |
| — | `LabRuntime` with chaos injection | New: reproducible concurrency bug detection |
| — | `Cx::for_testing()` | Creates test `Cx` with infinite budget |

```rust
// Preferred for most migrated tests:
#[test]
fn test_channel_send_recv() {
    asupersync::test_utils::run_test_with_cx(|cx| async move {
        let (tx, mut rx) = mpsc::channel::<i32>(16);
        tx.send(&cx, 42).await.unwrap();
        assert_eq!(rx.recv(&cx).await.unwrap(), 42);
    });
}
```

**Actual scale (verified counts):**

| Crate | `#[tokio::test]` macros | Total `tokio::` call sites |
|-------|-------------------------|----------------------------|
| forge-attractor (src + tests) | 142 | 156 |
| forge-agent (src + tests) | 80 | 115 |
| forge-llm (src + tests) | 52 | 82 |
| forge-cxdb-runtime (dev) | 8 | 8 |
| forge-cli (tests) | 0 | 7 |
| **Total** | **282** | **368** |

### 4.10 Error Mapping

asupersync introduces new error types that must integrate with Forge's `thiserror` hierarchy:

| asupersync error | Maps to | Context |
|-----------------|---------|---------|
| `mpsc::RecvError::Disconnected` | Existing `ChannelClosed` / `InternalError` | All senders dropped |
| `mpsc::RecvError::Cancelled` | New `Cancelled` variant | `Cx` was cancelled |
| `broadcast::RecvError::Closed` | Existing `ChannelClosed` | All broadcast senders dropped |
| `broadcast::RecvError::Cancelled` | New `Cancelled` variant | `Cx` was cancelled |
| `SendError<T>` | Existing `ChannelClosed` | Channel receiver dropped |
| `JoinError::Cancelled(reason)` | New `TaskCancelled(reason)` variant | Task was cancelled — preserve cancel reason |
| `JoinError::Panicked(payload)` | New `TaskPanicked(payload)` variant | Task panicked — preserve panic info |
| `SpawnError` | Existing `InternalError` | Scope at capacity |
| `broadcast::RecvError::Lagged(n)` | New `EventLag(n)` variant in watch | Consumer fell behind |
| `WsAcceptError` | New `WebSocketError` variant in watch | WebSocket upgrade failed |
| `RegionCreateError` | New `RegionError` variant | Scope/region creation failed |

**Important: Preserve `JoinError` semantics.** Do NOT collapse all `JoinError` into `InternalError`. The distinction between `Cancelled` and `Panicked` is critical for control-plane behavior (graceful shutdown vs fault recovery).

## 5. WebSocket Architecture (forge-cli watch)

**Implementation status:** Phase 5b is complete. The watch server now uses asupersync's `TcpListener` + `httparse` + `WebSocketAcceptor` for real-time event delivery. The pipeline and event bridge still run on separate tokio threads (pending Phases 1–4). See §5.1 for the as-built architecture and §5.6 for the target single-runtime architecture (post-Phases 1–4).

**Migration from SSE:** The previous watch implementation used long-poll SSE (`EventSource` in the browser, tokio `Http1Listener` + SSE sink on the server). This has been replaced with native WebSocket.

### 5.1 Server Architecture (As-Built)

The current implementation uses a hybrid architecture: asupersync handles TCP/WebSocket, while the pipeline engine and event bridge still run on separate tokio threads. This is acceptable as a transitional state — the threads communicate via `std::sync` primitives and `Notify`.

```
                    ┌─────────────────────────────────────────────┐
                    │  std::thread: "forge-watch-bridge"           │
                    │  (tokio current_thread runtime)              │
                    │  tokio mpsc rx → EventLog + Notify.notify() │
                    └────────────────────┬────────────────────────┘
                                         │ std::sync
                    ┌────────────────────▼────────────────────────┐
                    │  std::thread: "forge-watch-pipeline"         │
                    │  (tokio current_thread runtime)              │
                    │  PipelineRunner.run() → RuntimeEventSink     │
                    │  tokio mpsc tx → bridge thread               │
                    └─────────────────────────────────────────────┘

                    ┌─────────────────────────────────────────────┐
                    │  asupersync runtime (main thread)            │
                    │  RuntimeBuilder::current_thread()            │
                    │                                              │
                    │  TcpListener (single port)                   │
                    │      │                                       │
                    │      ├── GET /         → Serve HTML          │
                    │      ├── GET /api/*    → REST endpoints       │
                    │      ├── POST /api/*   → Control endpoints    │
                    │      └── GET /ws       → WebSocket upgrade    │
                    │                │                             │
                    │  handle.try_spawn(per-client WS task)        │
                    │      │                                       │
                    │      └── ws_client_loop:                     │
                    │           Notify.notified() OR               │
                    │           timeout(15s) + ping keepalive       │
                    └─────────────────────────────────────────────┘
```

**Shared state (AppState):** All three threads/runtimes share a single `Arc<AppState>` using `std::sync` primitives (`RwLock`, `Mutex`, `AtomicBool`). The asupersync `Notify` is used for WS client wakeup — it works correctly across runtimes because `Notify::notify_waiters()` does not require `&Cx`.

**HTTP request parsing (use `httparse`):**

**HTTP request parsing (use `httparse`):**

Since we use a raw `TcpListener`, HTTP requests must be parsed before WebSocket upgrade. Use `httparse` for zero-copy request parsing with an incremental read loop:

```rust
let mut buf = vec![0u8; 8192];
let mut total = 0;

// Incremental read loop — handles TCP fragmentation
let parsed = loop {
    let timeout_result = asupersync::time::timeout(
        asupersync::time::wall_now(),
        Duration::from_secs(5),
        stream.read(&mut buf[total..]),
    ).await;

    match timeout_result {
        Ok(Ok(0)) => break Err(Error::ConnectionClosed),  // EOF before complete request
        Ok(Ok(n)) => {
            total += n;
            let mut headers = [httparse::EMPTY_HEADER; 32];
            let mut req = httparse::Request::new(&mut headers);
            match req.parse(&buf[..total]) {
                Ok(httparse::Status::Complete(body_offset)) => {
                    break Ok((req.method.unwrap_or(""), req.path.unwrap_or(""), body_offset));
                }
                Ok(httparse::Status::Partial) => {
                    if total >= 8192 { break Err(Error::RequestTooLarge); }
                    continue;  // Read more data
                }
                Err(_) => break Err(Error::MalformedRequest),
            }
        }
        Ok(Err(e)) => break Err(e.into()),
        Err(_) => break Err(Error::ReadTimeout),  // 5-second timeout
    }
};
```

**Enforced limits:**

| Limit | Value | Rationale |
|-------|-------|-----------|
| Max request buffer size | 8 KB | Prevents memory exhaustion from malformed requests |
| Max header count | 32 | Standard limit for `httparse` header array |
| Request read timeout | 5 seconds | Prevents slowloris-style connection exhaustion |
| Allowed methods | GET only | Only GET / and GET /ws are valid |
| Allowed paths | `/`, `/ws` | Reject all other paths with 404 |

**Required malformed-request tests:**
1. Request exceeding 8 KB buffer → 400 Bad Request
2. Missing Host header → 400
3. POST to /ws → 405 Method Not Allowed
4. GET /api/anything → 404
5. Incomplete request (timeout) → connection dropped
6. Binary garbage → connection dropped
7. Fragmented valid request (multi-read) → succeeds

### 5.2 Connection Protocol (As-Built)

**On WebSocket connect, server immediately sends:**
```json
{ "type": "init", "graph": {...}, "status": {...} }
```

The `graph` field is the full Forge `Graph` JSON (nodes, edges, attrs). The `status` field is the current `WatchStatus` enum serialized with the `state` tag field.

Immediately after the `init` message, the server sends all buffered events from the `EventLog`:
```json
{ "type": "event", "seq": 1, "data": { "timestamp": "...", "kind": {...} } }
```
Events are sent one message per event, each with a monotonically increasing `seq` assigned by the `EventLog`. After all buffered events are sent, the connection transitions to live mode — new events are pushed as they arrive.

**Pipeline events (server → client):**
```json
{ "type": "event", "seq": 42, "data": { "timestamp": "...", "kind": {...} } }
```

The `seq` is a u64 assigned by the `EventLog::push()` method. The `data` field is the full `RuntimeEvent` JSON (timestamp + kind). There is no `run_id` field in the current implementation.

**Event ordering guarantee:** Events are assigned `seq` inside the `EventLog::push()` call, which is guarded by `std::sync::Mutex`. The lock is held for the duration of push. The `Notify::notify_waiters()` call follows the lock release, so clients are woken after the event is committed to the log.

**EventLog ring buffer:** The `EventLog` stores events in a `Vec<(u64, String)>` (sequence number + JSON string). Capacity is bounded to 10,000 events; when it exceeds this, the oldest 5,000 are drained. On reconnect, the WS client loop calls `EventLog::since(0)` to replay all buffered events.

**Control actions:** Control buttons in the HTML page (`start`, `pause`, `resume`, `stop`) send requests via HTTP `fetch()` to the REST endpoints (see §5.5). They are NOT sent over the WebSocket in the current implementation.

**Keepalive:** If no new events arrive within 15 seconds, the server sends a WebSocket `ping` frame. The WS client loop re-checks after each Notify wakeup or 15-second timeout.

**Target protocol extensions (deferred to post-Phases 1–4):**
- `run_id` field in events for cross-session resumability
- `resume` message from client (last_seq + run_id) for catch-up after reconnect
- `dropped_events` counter in `init` message for observability
- Control commands via WebSocket (replacing REST fetch)
- Per-run auth token (embedded in HTML, validated on WS upgrade)

### 5.3 Security Constraints (As-Built)

**Path sanitization (implemented):**
- `node_id` is checked for `..`, `/`, and `\` characters. Requests containing path traversal characters return 400.
- `file` must be in the allowlist: `["prompt.md", "response.md", "status.json"]`.
- The resolved path is `logs_root / node_id / filename`. Files are read with `std::fs::read()`.
- No TOCTOU hardening (no `/proc/self/fd` canonicalize verification) in the current implementation.
- Maximum response size: unbounded in current implementation (reads entire file).

**Bind address:** Configurable via `--bind` argument (default: `127.0.0.1`). No `--unsafe-bind` enforcement or non-localhost warnings are implemented.

**Authentication:** No per-run auth token is implemented. The WebSocket endpoint (`/ws`) accepts any connection without authentication. The REST endpoints (`/api/*`) also accept unauthenticated requests. This is acceptable for the current development-tool use case (localhost only by default).

**Rate limiting:** Not implemented.

**Target security hardening (deferred to post-Phase 5 or separate PR):**
- Per-run auth token embedded in HTML, validated on WS upgrade (see original §5.3 target design above)
- `--unsafe-bind` flag for non-localhost with warning
- `node_id` regex validation (`^[a-zA-Z0-9_-]+$`)
- Maximum artifact response size (10 MB)
- Rate limiting (10 concurrent WS connections, 60 control commands/min)
- TOCTOU-safe path validation via `/proc/self/fd` readlink

### 5.4 Event Flow (As-Built)

```
std::thread: "forge-watch-pipeline"
    │  (tokio runtime — PipelineRunner)
    │  RuntimeEventSink::emit() → tokio mpsc tx
    ▼
std::thread: "forge-watch-bridge"
    │  (tokio runtime — bridge_loop)
    │  tokio mpsc rx.recv().await
    │  → serde_json::to_string(event)
    │  → EventLog::push(json)  [std::sync::Mutex]
    │  → Notify::notify_waiters()  [asupersync Notify, cross-runtime safe]
    ▼
asupersync runtime (main thread)
    │
    ├── WS Client Task 1 (handle.try_spawn)
    │     ws_client_loop:
    │       1. send init message (graph + status)
    │       2. send all EventLog events since seq=0
    │       3. loop:
    │            poll EventLog::since(last_seq)
    │            if new events → send each, update last_seq
    │            else wait: Notify.notified() OR timeout(15s) + ping
    │
    ├── WS Client Task 2 ...
    └── WS Client Task N ...
```

**EventLog access pattern:** Each WS client task holds its own `last_seq: u64`. On each loop iteration it acquires `std::sync::Mutex<EventLog>` briefly, collects new events since `last_seq`, releases the lock, then sends the events over the WebSocket. The `Notify::notify_waiters()` in the bridge thread wakes all waiting client tasks simultaneously — zero-polling, Notify-driven delivery.

**Per-client WebSocket task (single owner, no split):**

**Critical:** `ServerWebSocket` does NOT have a `split()` method (verified — only client `WebSocket<IO>` has `split()`). The per-client task must own the WebSocket exclusively.

**`ServerWebSocket` API (verified, server.rs:295-324):**
- `pub async fn send(&mut self, cx: &Cx, msg: Message) -> Result<(), WsError>`
- `pub async fn recv(&mut self, cx: &Cx) -> Result<Option<Message>, WsError>` — returns `None` on clean close

**Note on WS recv in as-built implementation:** The current `ws_client_loop` does NOT call `ws.recv()` — it only sends events from the `EventLog`. Client-to-server messages (control actions) go via the HTTP REST endpoints instead. The client loop uses a Notify-with-timeout pattern rather than the Select-based approach described in the original target design. The target design below (§5.6) describes the Select-based approach for the fully-unified post-Phase-5 implementation.

**As-built `ws_client_loop` pattern:**
```rust
async fn ws_client_loop(cx: &Cx, ws: &mut ServerWebSocket<TcpStream>, state: &AppState) {
    let mut last_seq: u64 = 0;

    // 1. Send init message
    let init = json!({ "type": "init", "graph": &state.graph, "status": ... });
    ws.send(cx, Message::text(init.to_string())).await?;

    // 2. Replay all buffered events
    let buffered = state.event_log.lock().unwrap().since(0).collect::<Vec<_>>();
    for (seq, json) in buffered {
        ws.send(cx, Message::text(format!(r#"{{"type":"event","seq":{seq},"data":{json}}}"#))).await?;
        last_seq = seq;
    }

    // 3. Live event loop
    loop {
        if state.shutdown.load(Ordering::Acquire) { /* close */ return; }

        let new_events = state.event_log.lock().unwrap().since(last_seq).collect::<Vec<_>>();
        for (seq, json) in new_events {
            ws.send(cx, Message::text(format!(r#"{{"type":"event","seq":{seq},"data":{json}}}"#))).await?;
            last_seq = seq;
        }

        if no_new_events {
            match timeout(wall_now(), Duration::from_secs(15), state.event_notify.notified()).await {
                Ok(()) => {} // woken by new event — loop
                Err(_)  => { ws.ping(...).await?; } // 15s timeout — send keepalive ping
            }
        }
    }
}
```

**Target design with Select (deferred to post-Phases 1–4):**

The original target design — a single-owner WS loop using `Select` to race `ws.recv()` against `Notify::notified()` — is still the right end-state. It adds bidirectional messaging over WS (replacing REST fetch for control actions). This is deferred until all crates are on asupersync:

```rust
// Post-unification: bidirectional WS loop (target, not yet implemented)
loop {
    // Drain all pending outbound messages (non-blocking)
    while let Ok(msg) = out_rx.try_recv() {
        ws.send(cx, Message::text(msg)).await?;
    }

    // Wait for either: client message OR outbound queue notification
    // Use Select to race ws.recv against wake_notify
    let ws_recv_fut = ws.recv(cx);
    let wake_fut = wake_notify.notified();

    match Select::new(ws_recv_fut, wake_fut).await {
        Either::Left(Ok(Some(Message::Text(cmd)))) => {
            let resp = handle_client_command(app_state, &cmd);
            ws.send(cx, Message::text(resp)).await?;
        }
        Either::Left(Ok(Some(Message::Close(_)))) | Either::Left(Ok(None)) => break,
        Either::Left(Err(e)) => return Err(e.into()),
        Either::Right(()) => {
            // Notified — loop back to drain outbound queue
            continue;
            }
            _ => {} // ignore binary frames
        }
    }

    // Signal forwarder to exit (JoinHandle has no abort — use Notify)
    forwarder_shutdown.notify_one();
    // Optionally await the forwarder JoinHandle to ensure clean exit:
    // let _ = forwarder.await;
    Ok(())
}
```

This design has **zero polling** — the `Select` blocks until either a client message arrives or the `Notify` fires when outbound events are queued. The `Notify` wakes the loop immediately, so event delivery latency is bounded only by the WS send time.

**Cancel safety of `ws.recv` in Select:** When `wake_fut` wins the Select race, the `ws_recv_fut` is dropped. This is safe because `ServerWebSocket::recv` is **cancel-safe** — partially received frame state is stored in the `ServerWebSocket` struct itself (not in the future), so dropping and re-creating the recv future on the next loop iteration does not lose data. Verified: the WebSocket implementation uses an internal buffer that persists across recv calls (server.rs:220-324). The `Notify::notified()` future is also cancel-safe (documented: "waiter is removed on cancellation", notify.rs:9).

**Note on `Cx` in event forwarder:** The forwarder uses `cx.clone()` (Cx is Clone + Send + Sync, verified at cx.rs:151). When the parent scope closes, the broadcast `recv` will return `Cancelled`, causing the forwarder to exit. This maintains the cancellation tree — no orphaned tasks.

### 5.5 HTTP REST Endpoints (As-Built)

**Note:** The original design called for removing all REST endpoints in favor of WebSocket messages. In the as-built implementation, REST endpoints are **retained**. The HTML page uses `fetch()` for control actions and artifact loading, while only event streaming uses WebSocket. This is an intentional pragmatic tradeoff for the transitional state.

| HTTP Endpoint | Purpose |
|---------------|---------|
| `GET /` | Serve static HTML page (watch.html) |
| `GET /api/graph` | Current graph as JSON |
| `GET /api/dot` | Original DOT source |
| `GET /api/status` | Current `WatchStatus` as JSON |
| `GET /api/artifacts/{node}/{file}` | Serve artifact file (prompt.md, response.md, status.json) |
| `POST /api/control/start` | Trigger deferred pipeline start |
| `POST /api/control/pause` | Pause pipeline via `PipelineControl` |
| `POST /api/control/resume` | Resume pipeline via `PipelineControl` |
| `POST /api/control/stop` | Cancel pipeline via `PipelineControl` |
| `GET /ws` | WebSocket upgrade — real-time event stream |

**HTML uses fetch() for artifacts and controls:** The `fetchArtifact()` function in watch.html calls `GET /api/artifacts/{node}/{file}` via `fetch()`. The control buttons call `POST /api/control/{action}` via `fetch()`. After a control action, the HTML re-fetches `GET /api/status` to refresh the UI.

**Target (deferred):** After full runtime unification (post-Phases 1–4), control commands and artifact requests should migrate to WebSocket messages (`control` and `artifact_request`/`artifact` message types), and the REST endpoints except `GET /` can be removed. The HTML would receive status updates via the existing `init` + `event` messages rather than polling.

### 5.6 Target Single-Runtime Architecture (Post-Phases 1–4)

After all crates are on asupersync, the pipeline and bridge threads are eliminated. Everything runs on the single asupersync `current_thread` runtime:

```
                    ┌─────────────────────────────────────────┐
                    │        asupersync runtime                │
                    │   RuntimeBuilder::current_thread()       │
                    │   .blocking_threads(2, 16)               │
                    │                                          │
                    │  ┌─────────────┐  ┌───────────────────┐ │
                    │  │  TCP/WS     │  │  Pipeline Engine  │ │
                    │  │  Server     │  │  (scope.spawn)    │ │
                    │  └──────┬──────┘  └────────┬──────────┘ │
                    │         │                  │            │
                    │         │   EventLog +     │ emit()     │
                    │         │   Notify         │ try_send   │
                    │         │◄──bridge task────┘            │
                    │         │   (mpsc rx → EventLog push)   │
                    │  ┌──────▼──────┐                        │
                    │  │  WS Clients │  (handle.try_spawn)    │
                    │  │  (N tasks)  │                        │
                    │  └─────────────┘                        │
                    └─────────────────────────────────────────┘
```

In the target architecture:
- Pipeline engine runs as a scoped task on the asupersync runtime via `scope.spawn`.
- The event bridge runs as an inline asupersync task (mpsc `rx.recv(&cx).await` → EventLog push → Notify).
- WS client tasks are spawned via `handle.try_spawn`.
- Control actions arrive via WS message and are dispatched inline in the WS client loop using `Select` (see §5.4 target design).
- Signal handling uses `spawn_blocking` + `signal_hook` (see §6.1).
- No `std::thread::spawn` except the §6.3 shutdown safety valve.

## 6. Graceful Shutdown

When the user presses Ctrl+C or sends SIGINT, the runtime must shut down cleanly.

### 6.1 Signal Handling

```rust
let (signal_handle, _) = scope.spawn(state, &cx, |_cx| async {
    Outcome::Ok(
        asupersync::runtime::spawn_blocking(|| {
            let mut signals = signal_hook::iterator::Signals::new(&[signal_hook::consts::SIGINT])?;
            // Signals::forever() returns an iterator; .next() blocks until first signal
            signals.forever().next();
            Ok::<_, std::io::Error>(())
        }).await
    )
})?;
```

### 6.2 Cancellation Cascade

On SIGINT:
1. The signal task completes, triggering region close
2. Region close initiates three-phase cancellation on all child tasks:
   - **Request phase:** All child tasks receive cancellation notification
   - **Drain phase:** Tasks with `cx.checkpoint()?` in their loops see the cancellation and begin cleanup
   - **Finalize phase:** Deferred cleanup runs
3. The pipeline runner's main loop checks `cx.checkpoint()?` at each node boundary, ensuring clean exit between nodes
4. WebSocket server tasks receive cancellation and send `Close` frames to connected clients
5. Runtime exits after all tasks reach terminal state

### 6.3 Timeout Enforcement

**`RuntimeBuilder` does NOT have a `shutdown_timeout()` method.** (Note: `ServerConfig` has a `shutdown_timeout` field at config.rs:99, but that applies to the HTTP server graceful shutdown — not the runtime itself. Forge uses a raw `TcpListener`, not the asupersync HTTP server framework, so `ServerConfig::shutdown_timeout` is irrelevant.) Implement at the application level:

```rust
// Top-level: race application work against signal
let scope = cx.scope();
scope.region(&mut state, &cx, FailFast, |app_scope, state| async move {
    // Unify return type for both branches
    enum ShutdownResult {
        WorkDone(Result<(), Error>),
        SignalReceived,
    }

    let (work_handle, _) = app_scope.spawn(state, &cx, |cx| async move {
        Outcome::Ok(ShutdownResult::WorkDone(
            run_application(&cx, config).await
        ))
    })?;

    let (signal_handle, _) = app_scope.spawn(state, &cx, |_cx| async move {
        Outcome::Ok(ShutdownResult::SignalReceived)  // signal_hook wait elided for brevity
    })?;

    // race returns Result<ShutdownResult, JoinError> — same T for both handles
    match app_scope.race(&cx, work_handle, signal_handle).await {
        Ok(ShutdownResult::WorkDone(result)) => {
            Outcome::Ok(result)
        }
        Ok(ShutdownResult::SignalReceived) => {
            // Signal received — scope close handles 3-phase cancellation.
            // Hard timeout as safety valve:
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_secs(5));
                eprintln!("WARN: Shutdown timed out after 5s, force exiting");
                std::process::exit(1);
            });
            Outcome::Ok(Ok(()))
        }
        Err(join_err) => Outcome::Err(join_err.into()),
    }
}).await;
```

**Note:** The `std::thread::spawn` + `process::exit` is a last-resort safety valve. In practice, three-phase cancellation should complete well within 5 seconds. The hard timeout prevents hung subprocesses from blocking shutdown indefinitely.

## 7. Migration Plan

The migration proceeds strictly bottom-up through the crate dependency graph. Each phase produces a compiling, fully-tested, tokio-free state for that crate **and all crates that depend on it**.

**Critical constraint: Phase boundary compilation.** When Phase N changes a trait, all reverse-dependent crates must be updated in the **same PR** to maintain workspace compilation.

Each phase is an independent, revertible PR. **Rollback is LIFO-only** — reverting Phase N after Phase N+1 is merged will break compilation. To roll back Phase N, first revert Phase N+1.

### Phase 1: forge-cxdb-runtime + shared HTTP adapter

**Scope:** `crates/forge-cxdb-runtime/` + shared HTTP adapter + downstream usage in `forge-cli`

**Why first:** `forge-cxdb-runtime` depends on `reqwest` which transitively pulls `tokio`. The shared HTTP adapter built here is reused in Phase 2a.

**Changes:**
1. Create shared HTTP adapter as a standalone internal crate `forge-http` (manual TLS + HTTP/1.1 + `httparse`, see §4.7). This crate sits below both `forge-cxdb-runtime` and `forge-llm` in the dependency graph, avoiding layering violations.
2. Replace `reqwest` HTTP client in `CxdbHttpClient` with the shared adapter.
3. Update `CxdbHttpClient` trait — if async methods need `&Cx`, update trait signatures AND implementations.
4. Replace `#[tokio::test]` in dev-dependencies (8 tests) with `run_test_with_cx(|cx| async move { ... })`.
5. Remove `reqwest` and `tokio` from `Cargo.toml`.
6. Add `asupersync` to dependencies.
7. **Update forge-cli:** Replace `CxdbReqwestHttpClient::new()` in `forge-cli/src/main.rs:258` and `forge-cli/tests/e2e_pipeline.rs`.

**Tests:** All forge-cxdb-runtime tests pass. CXDB live tests pass (`--ignored`). `cargo build -p forge-cli` succeeds.

**Rollback:** Revert PR.

### Phase 2a: forge-llm HTTP client migration

**Scope:** `crates/forge-llm/src/` (HTTP adapter integration, reqwest removal)

**Changes:**
1. Integrate shared HTTP adapter from Phase 1 into `openai.rs` and `anthropic.rs`.
2. Remove `reqwest` from `Cargo.toml`.
3. Write adapter test suite (see §4.7 required test coverage).

**Validation gate:** Live provider tests (`--ignored`) must pass against real OpenAI and Anthropic APIs before merging.

**Rollback:** Revert PR.

### Phase 2b: forge-llm process + channel + time migration

**Scope:** `crates/forge-llm/` (remaining tokio usages)

**Changes:**

#### CLI adapter migration (tokio::process → asupersync::process)
1. Replace `tokio::process::Command` with `asupersync::process::Command` in `cli_adapters/claude_code.rs`, `codex.rs`, `gemini.rs`.
2. Replace `tokio::io::BufReader` + `AsyncBufReadExt::lines()` with `asupersync::io::BufReader` + `FramedRead` + `LinesCodec`.
3. **Critical:** Replace `child.wait().await` with `spawn_blocking(move || child.wait()).await`.
4. **Critical:** Extract `child.stdout()` AND `child.stderr()` BEFORE moving `child` into `spawn_blocking` (see §4.5).

#### Channel migration (unbounded → bounded)
1. Replace `futures::channel::mpsc::unbounded()` in `openai.rs`, `anthropic.rs`, `high_level.rs` with `asupersync::channel::mpsc::channel(256)`.
2. Update all `rx.recv()` → `rx.recv(&cx)` patterns, handling `Result<T, RecvError>` (variants: `Disconnected`, `Cancelled`) instead of `Option<T>`.
3. Thread `&Cx` through function signatures that need to send/recv.
4. Remove `futures` channel dependency. Keep `futures-util` if `StreamExt::next()` or `BoxFuture` is still used.

#### Time + spawn migration
1. Replace `tokio::spawn` with `scope.spawn(state, &cx, |cx| ...)` or `handle.spawn(fut)`.
2. Replace `tokio::time::timeout` with `asupersync::time::timeout(wall_now(), dur, fut)`.
3. Replace `tokio::time::sleep` with `sleep(wall_now(), dur)`.
4. Remove tokio from `Cargo.toml`.
5. Replace `#[tokio::test]` macros (52 tests).

#### Trait signature ripple
1. If `ProviderAdapter::complete`/`stream` or `AgentProvider::run_to_completion` gain `&Cx`, update all implementations AND all call sites in `forge-agent` and `forge-attractor`.
2. Stub-compatible: add `cx: &Cx` parameter, pass `Cx::for_request()` at call sites that don't yet have a real `Cx`. **Warning: Cx::for_request() stubs create orphan cancellation trees — they are temporary and MUST be replaced in subsequent phases.**

**Tests:** All forge-llm tests pass. Live provider tests pass. `cargo build` succeeds for entire workspace.

**Rollback:** Revert PR.

### Phase 3: forge-agent (full migration)

**Scope:** `crates/forge-agent/` + downstream trait signature updates in `forge-attractor`, `forge-cli`

This is the largest migration (115 `tokio::` call sites, 80 `#[tokio::test]` macros).

**Changes:**

#### 3a. Execution environment (`src/execution.rs`)
1. Replace all `tokio::fs::*` calls (9 sites) with `asupersync::fs::*`.
2. Replace `tokio::process::Command` with `asupersync::process::Command`.
3. Replace `tokio::spawn` (pipe readers) with `scope.spawn(state, &cx, |cx| ...)`.
4. Replace `tokio::time::timeout` with `asupersync::time::timeout`.
5. **Critical:** Replace `child.wait().await` with `spawn_blocking(move || child.wait()).await`.
6. Extract stdout AND stderr before moving child (see §4.5).

#### 3b. Session state machine (`src/session/`)
1. Replace `tokio::sync::Notify` with `asupersync::sync::Notify`.
2. Replace `tokio::spawn` (abort watchdog) with `scope.spawn(state, &cx, |cx| ...)`.
3. Replace `tokio::select!` (the ONE select! site at line 568) with `scope.region` + `scope.race` — see §4.4 before/after example. Use `scope.region(state, &cx, FailFast, ...)`, NOT `Scope::region(...)` as a static call.
4. Replace `tokio::pin!` with `std::pin::pin!`.
5. Thread `&Cx` through `Session::submit()`, tool execution, and subagent spawning.
6. **Unbounded channel migration:** Replace `futures::channel::mpsc::unbounded()` in `forge-agent/src/events.rs` with `mpsc::channel(512)` + `try_send()` pattern (same as §3.9).

#### 3c. Tool implementations (`src/tools/`)
1. Each tool file (8 files) has 1 `tokio::fs` or `tokio::process` call — mechanical replacement.
2. Thread `&Cx` through `ToolRegistry::execute()` and individual tool functions.

#### 3d. Test migration
1. Replace `#[tokio::test]` macros (80 tests) with `run_test_with_cx(|cx| async move { ... })`.
2. Replace `tokio::time::sleep` in test mocks with `sleep(wall_now(), dur)`.

#### 3e. Trait signature ripple
1. If `ExecutionEnvironment` or `ToolRegistry` methods gain `&Cx`, update call sites in `forge-attractor` handlers.

**Tests:** All forge-agent tests pass. `cargo build` succeeds for entire workspace.

**Rollback:** Revert PR.

### Phase 4: forge-attractor (channel + sync + spawn migration)

**Scope:** `crates/forge-attractor/`

This is the second-largest migration (156 `tokio::` call sites, 142 `#[tokio::test]` macros).

**Changes:**
1. Replace `tokio::sync::mpsc::unbounded_channel()` in `events.rs` with `asupersync::channel::mpsc::channel(1024)`. Use `try_send()` to preserve sync `emit()` signature (see §3.9).
2. Update `RuntimeEventReceiver` type. Handle `Result<T, RecvError>` — use `RecvError::Disconnected` for channel-closed detection.
3. Replace `tokio::sync::Notify` in `runtime.rs` with `asupersync::sync::Notify`.
4. Replace `tokio::sync::Mutex` with `asupersync::sync::Mutex` (where `Cx` available) or `std::sync::Mutex` (where not). **Never hold guard across `.await`.**
5. Replace `tokio::time::sleep` / `tokio::time::timeout` with asupersync equivalents.
6. Replace `tokio::spawn` with `scope.spawn(state, &cx, |cx| ...)` or `handle.spawn(fut)`.
7. Replace `tokio::task::spawn_blocking` in `interviewer.rs` with `asupersync::runtime::spawn_blocking`.
8. Thread `&Cx` through `NodeHandler::execute()`, `Runner::run()`, handler dispatch.
9. Insert `cx.checkpoint()?` in the runner main loop at each node boundary.
10. Remove tokio from `Cargo.toml`.
11. Replace `#[tokio::test]` macros (142 tests) with `run_test_with_cx(|cx| async move { ... })`.

**Note:** ZERO `tokio::select!` sites in forge-attractor (verified). No select migration needed.

**Tests:** All forge-attractor tests pass.

**Rollback:** Revert PR.

### Phase 5: forge-cli (WebSocket + single runtime)

**Scope:** `crates/forge-cli/`

By this phase, ALL downstream crates are on asupersync. No bridge needed.

#### 5b. Watch server rewrite (`src/watch.rs`) — COMPLETE

**Status: Complete.** The watch server has been rewritten. Key changes implemented:
- Replaced `Http1Listener` + SSE sink with raw `TcpListener` + `httparse`-based HTTP parsing + `WebSocketAcceptor`.
- Added `EventLog` ring buffer (Vec-based, capacity 10k, drains to 5k when full).
- WS client loop uses asupersync `Notify` for zero-poll event delivery with 15-second keepalive ping.
- asupersync `RuntimeBuilder::current_thread()` for the TCP/WS server.
- `httparse` added as a dependency.
- Replaced Graphviz WASM + EventSource-based HTML with Cytoscape.js + dagre layout + Split.js + native WebSocket.

**Deviations from original Phase 5b spec:**
- Pipeline and event bridge still run on separate tokio threads (not asupersync tasks). This is because Phases 1–4 (converting forge-attractor, forge-agent, etc.) are not yet complete.
- No per-run auth token (deferred).
- No rate limiting (deferred).
- REST endpoints retained (see §5.5). Control actions and artifact fetching still use HTTP fetch, not WS messages.
- `broadcast::channel` NOT used — WS clients poll `EventLog` on `Notify` wakeup instead.
- `Cx::for_testing()` used instead of `Cx::for_request()` in the asupersync runtime block (acceptable for now; must be fixed in full Phase 5 completion).
- Signal handling uses a `std::thread` + `libc::signal` pattern (not `signal_hook` + `spawn_blocking`).

**Changes:**

#### 5a. Runtime entry point (`src/main.rs`)
1. Replace `tokio::runtime::Builder::new_current_thread()` with `asupersync::runtime::RuntimeBuilder::current_thread()`.
2. Save `runtime.handle()` for passing to subsystems that need unstructured spawn.
3. Replace `runtime.block_on(async { ... })` — add `Cx::for_request()` + scope creation at the top.
4. Restructure `run_command` and `resume_command` to use `scope.region`.

#### 5b. Watch server rewrite (`src/watch.rs`) — remaining work
1. ~~Replace `Http1Listener` with raw `TcpListener` + `httparse`-based HTTP parsing~~ ✓ Done
2. ~~Remove all long-poll SSE infrastructure~~ ✓ Done
3. ~~Add `EventLog` ring buffer~~ ✓ Done (Vec-based, 10k cap)
4. ~~Each WS client uses single-owner loop with Notify-based wake~~ ✓ Done
5. Migrate pipeline from separate tokio thread to asupersync scope task (requires Phases 1–4 first)
6. Migrate bridge from tokio thread to asupersync bridge task (requires Phases 1–4 first)
7. Implement per-run auth token (§5.3) + rate limiting
8. Replace `Cx::for_testing()` with `Cx::for_request()` at entry point
9. Migrate signal handling to `signal_hook` + `spawn_blocking` pattern (§6.1)
10. Migrate control actions to WS messages (replacing REST fetch)
11. Replace `broadcast::channel` placeholder with actual broadcast fan-out once bridge is async

#### 5c. Watch HTML rewrite (`src/watch.html`) — COMPLETE
1. ~~Replace fetch-based long-poll + Graphviz WASM + EventSource with native WebSocket + Cytoscape.js + dagre~~ ✓ Done
2. Auto-reconnect with exponential backoff on WS close ✓ Done
3. Remaining: token integration, `resume`/`lag` message handling (deferred with auth token)

#### 5d. Signal handling
1. Use `spawn_blocking` with `signal_hook` (see §6.1).
2. Wire into region close for cancellation cascade (§6.2).
3. Implement shutdown timeout via race pattern (§6.3).

#### 5e. Cleanup
1. `forge-cli/Cargo.toml`: Remove tokio (currently retained for pipeline/bridge threads), add `httparse` ✓ Done. `signal_hook` deferred.
2. `forge-cli/tests/e2e_pipeline.rs`: Replace tokio runtime with asupersync.

**Tests:** forge-cli smoke tests pass. Manual E2E: WebSocket + events + artifacts + control actions verified working.

**Rollback:** Revert PR.

### Phase 6: Workspace cleanup

**Scope:** Workspace root, all specs, docs

**Changes:**
1. `cargo tree -p forge-cli --edges normal | grep -v forge-cxdb | grep tokio` → must return nothing.
2. Update specs 01-03 if they reference reqwest or tokio.
3. Add `sleep_for(cx, dur)` helper to a shared utility module.
4. Update the asupersync skill reference (`api.md`) to note the ALPN bug.
5. Verify `default-features = false` for asupersync in all production Cargo.toml.
6. **Audit all `Cx::for_request()` stubs** from Phases 2-4. Each must be replaced with real `Cx` propagation from the root. No stubs should remain after this phase.
7. Performance regression check: mock pipeline execution time before vs after.

**Rollback:** Revert PR.

## 8. Dependency Changes

### Current State (Phase 5b complete)

| Crate | Status |
|-------|--------|
| forge-cli | Added: `asupersync = "0.2.6"`, `httparse = "1"`, `libc = "0.2"`. Retained: `tokio` (for pipeline + bridge threads, pending Phases 1–4). |
| forge-attractor | Unchanged — tokio still primary runtime |
| forge-agent | Unchanged — tokio still primary runtime |
| forge-llm | Unchanged — tokio still primary runtime |
| forge-cxdb-runtime | Unchanged — tokio still in dev-dependencies |

### Target State (all phases complete)

#### Removed
| Crate | Removed Dependency |
|-------|--------------------|
| forge-attractor | `tokio`, `futures` (if only channel was used) |
| forge-agent | `tokio`, `futures` channel |
| forge-llm | `tokio`, `reqwest`, `futures` channel |
| forge-cli | `tokio` |
| forge-cxdb-runtime | `reqwest`, `tokio` (dev), `futures` |

**Note on `futures-util`:** Retained in `forge-llm` (required for `StreamExt::next()` on `FramedRead` streams in CLI adapters, and `BoxFuture` in trait returns). May also be retained in `forge-agent` — check during Phase 3.

**Note on `libc`:** Retained in `forge-cli` (current signal handling) and `forge-llm` (process group management via `setpgid`/`killpg`). After Phase 5 completion, `forge-cli` signal handling migrates to `signal_hook` + `spawn_blocking`; `libc` stays in `forge-llm` for process groups.

### Added / Updated
| Crate | Added Dependency |
|-------|-----------------|
| **forge-http** (NEW) | `asupersync = { version = "0.2.6", default-features = false, features = ["tls-webpki-roots"] }`, `httparse` |
| forge-attractor | `asupersync = { version = "0.2.6", default-features = false }` |
| forge-agent | `asupersync = { version = "0.2.6", default-features = false }` |
| forge-llm | `asupersync = { version = "0.2.6", default-features = false }`, `forge-http`, `libc` |
| forge-cli | `asupersync = { version = "0.2.6", default-features = false }`, `signal_hook`, `httparse` |
| forge-cxdb-runtime | `asupersync = { version = "0.2.6", default-features = false }`, `forge-http` |

**All crates use `default-features = false`** to avoid pulling `test-internals` into production. Dev-dependencies enable it:
```toml
[dev-dependencies]
asupersync = { version = "0.2.6", features = ["test-internals"] }
```

### Transitive impact
- Removing `reqwest` eliminates: `hyper`, `hyper-util`, `hyper-rustls`, `h2`, `http`, `http-body`, `tower`, and their transitive deps.
- Removing `tokio` eliminates: `tokio`, `tokio-util`, `tokio-stream`, `mio`, and their transitive deps.
- Net reduction: ~30+ crates eliminated.

## 9. Risk Assessment

### Critical Risk
- **HTTPS TLS/ALPN bug (Phase 1 + 2a).** Mitigation: Manual TLS with h1-only ALPN (§4.7). Fallback: `ureq` in `spawn_blocking`. **Stop/go gate:** Live provider tests must pass before merging Phase 2a.

### High Risk
- **Cx propagation (Phases 2b-5).** Threading `&Cx` through every async function signature. Mitigation: §3 defines patterns; implement mechanically per crate. Only orchestrators get `&mut RuntimeState` (§3.6).
- **reqwest removal (Phase 1 + 2a).** Shared HTTP adapter must handle OpenAI + Anthropic + CXDB. Mitigation: `httparse` for parsing, detailed test matrix in §4.7. Stop/go: live provider tests.
- **Unbounded → bounded channels (Phases 2b + 3 + 4).** Mitigation: `try_send()` with generous buffers + dropped-event counters.
- **`select!` → `scope.race` (Phase 3b).** Exactly 1 site. Mitigation: exact before/after in §4.4.

### Medium Risk
- **WebSocket protocol (Phase 5b).** `ServerWebSocket` has no `split()` — single-owner + Notify pattern. Mitigation: incremental HTTP parsing with `httparse` (§5.1), Notify-based wake (§5.4).
- **Process `wait()` blocking (Phase 2b + 3a).** Mitigation: `spawn_blocking` pattern in §4.5; extract stdout+stderr before moving child.
- **RuntimeState propagation.** Mitigation: §3.6 strategy — orchestrators own state, inner code is spawn-free.
- **Phase boundary compilation.** Mitigation: each phase includes all reverse-dependent updates.

### Low Risk
- Time migration: straightforward with `wall_now()` + Cx-aware helper.
- Test macro migration: mechanical `#[tokio::test]` → `run_test_with_cx`.
- Filesystem migration: identical API surface.
- `signal_hook`: well-maintained, widely-used crate.

## 10. Non-Goals

- **Migrating forge-cxdb (vendored CXDB client):** Vendored crate using edition 2021. Does not use tokio in production code. Verify via `cargo tree` that it does not pull tokio into the production binary. Leave untouched.
- **Multi-threaded runtime:** Preserves `current_thread` everywhere.
- **io_uring:** Not enabled in this migration.
- **HTTP/2 or HTTP/3:** Not needed.
- **Replacing `async_trait`:** Explicitly deferred. `Cx` is `Send + Sync`, so adding `cx: &Cx` to `#[async_trait]` methods works without bound issues. The boxing overhead is a known cost to optimize post-migration.

## 11. Success Criteria

Progress legend: ✓ = done, ◐ = partial, ○ = not started

1. ○ `cargo build` succeeds with zero direct tokio dependencies in Forge crates (excluding forge-cxdb vendored crate). *(tokio retained in forge-cli and all upstream crates pending Phases 1–4)*
2. ○ `cargo test` — all existing tests pass (282 migrated test macros). *(not yet migrated)*
3. ○ `cargo test --ignored` — all live provider tests pass. *(not yet migrated)*
4. ◐ `forge-cli watch` — WebSocket works: real-time events, control actions, artifact requests, auto-reconnect. **WS events and graph visualization: ✓ done.** Missing: auth token, resume with run_id, WS-native control commands.
5. ○ `forge-cli run` — pipelines execute correctly on asupersync runtime. *(main.rs still uses tokio)*
6. ○ `forge-cli resume` — checkpoint resume works. *(main.rs still uses tokio)*
7. ○ Binary size reduction from dependency elimination (measured, not targeted).
8. ○ No explicit `std::thread::spawn` calls in forge-cli watch except the §6.3 shutdown safety valve. *(two std::thread spawns for pipeline + bridge remain)*
9. ○ `RecvError::Lagged` handled gracefully in all broadcast consumers. *(broadcast not yet used)*
10. ○ All `child.wait()` calls use `spawn_blocking`. *(forge-llm not yet migrated)*
11. ○ Performance: mock pipeline time not regressed >10%.
12. ◐ Graceful shutdown: Ctrl+C causes clean exit within 5 seconds. *(SIGINT handler implemented via libc::signal + AtomicBool; not yet using signal_hook + spawn_blocking)*
13. ○ Security: WebSocket auth token, path validation, rate limiting. *(basic node_id/filename validation done; no token, no rate limiting)*
14. ○ All production Cargo.toml use `default-features = false` for asupersync. *(forge-cli currently uses `asupersync = "0.2.6"` without `default-features = false`)*
15. ○ No `Cx::for_request()` stubs remain — all Cx instances flow from root. *(forge-cli watch uses `Cx::for_testing()`)*
16. ○ Dropped-event counter exposed via watch UI.
17. ○ `JoinError::Cancelled` and `JoinError::Panicked` preserved as distinct error variants.

## 12. Appendix: asupersync Feature Requirements

```toml
# forge-http (NEW — shared HTTP adapter, needs TLS)
[dependencies]
asupersync = { version = "0.2.6", default-features = false, features = ["tls-webpki-roots"] }
httparse = "1"

[dev-dependencies]
asupersync = { version = "0.2.6", features = ["test-internals"] }

# forge-llm (uses forge-http for API calls; needs libc for process groups)
[dependencies]
asupersync = { version = "0.2.6", default-features = false }
forge-http = { path = "../forge-http" }
libc = "0.2"
futures-util = "0.3"  # retained for StreamExt::next() on FramedRead

[dev-dependencies]
asupersync = { version = "0.2.6", features = ["test-internals"] }

# forge-cxdb-runtime (uses forge-http for CXDB HTTP API)
[dependencies]
asupersync = { version = "0.2.6", default-features = false }
forge-http = { path = "../forge-http" }

[dev-dependencies]
asupersync = { version = "0.2.6", features = ["test-internals"] }

# forge-cli (needs signal handling + HTTP parsing for WS server)
[dependencies]
asupersync = { version = "0.2.6", default-features = false }
signal_hook = "0.3"
httparse = "1"

[dev-dependencies]
asupersync = { version = "0.2.6", features = ["test-internals"] }

# All other crates (runtime, channels, sync, time, fs, process)
[dependencies]
asupersync = { version = "0.2.6", default-features = false }

[dev-dependencies]
asupersync = { version = "0.2.6", features = ["test-internals"] }
```

**CI check:**
```bash
# Verify no production dep enables test-internals
cargo tree -p forge-cli --edges normal | grep -i "test-internals" && exit 1 || true
```
