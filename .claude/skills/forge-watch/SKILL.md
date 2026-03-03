---
name: forge-watch
description: >-
  Start the Forge pipeline visualizer and get an accessible HTTPS link. This skill
  handles building the binary, starting the watch server, configuring Caddy reverse
  proxy, and returning a link you can open on any device (phone, tablet, laptop).
  Use this skill when the user says "watch pipeline", "start forge watch", "visualize
  pipeline", "show me the pipeline", "forge watch", "pipeline visualizer", "watch
  the orchestration", "start the visualizer", or wants to see a live view of a Forge
  Attractor pipeline running. Also trigger when the user asks to "stop forge watch",
  "kill the visualizer", or check its status.
---

# Forge Pipeline Visualizer

Start or manage the Forge pipeline visualizer — a web UI that shows a live, interactive
view of an Attractor pipeline as it executes. The left pane renders the pipeline graph
vertically with animated node states; the right pane shows the prompt/response conversation
for each node.

## Quick Reference

| Item | Value |
|------|-------|
| **Binary** | `forge-cli` (built from `/home/ubuntu/Projects/forge`) |
| **Subcommand** | `forge-cli watch` |
| **Local port** | 8384 (default) |
| **Caddy route** | `/forge*` on port 443 (strips `/forge` prefix) |
| **External URL** | `https://144.126.145.59/forge` |
| **DOT examples** | `/home/ubuntu/Projects/forge/examples/*.dot` |

## Starting the Visualizer

### Step 1: Build the binary

```bash
cd /home/ubuntu/Projects/forge && PATH="$HOME/.cargo/bin:$PATH" cargo build -p forge-cli --release
```

Use `--release` for a faster runtime. If already built recently, this is a no-op.

### Step 2: Ensure the Caddy route exists

Check if `/etc/caddy/Caddyfile` already has the `/forge*` handle block inside the
port-443 site. It should look like:

```caddy
    handle /forge* {
        uri strip_prefix /forge
        reverse_proxy localhost:8384
    }
```

If missing, add it inside the `https://144.126.145.59:443 { ... }` block (before the
final catch-all `handle { ... }`), then reload:

```bash
sudo systemctl reload caddy
```

Only do this once — if the block already exists, skip this step.

### Step 3: Kill any existing instance

```bash
pkill -f "forge-cli.*watch" 2>/dev/null; echo "Cleaned up"
```

### Step 4: Start the watch server

```bash
cd /home/ubuntu/Projects/forge && \
FORGE_CXDB_PERSISTENCE=off \
PATH="$HOME/.cargo/bin:$PATH" \
nohup target/release/forge-cli watch \
  --dot-file <DOT_FILE> \
  --backend <BACKEND> \
  --bind 127.0.0.1 \
  --port 8384 \
  > /tmp/forge-watch.log 2>&1 &
echo "PID: $!"
```

**Parameters the user should specify:**
- `<DOT_FILE>` — which pipeline to run (e.g., `examples/01-linear-foundation.dot`)
- `<BACKEND>` — which agent backend: `mock`, `claude-code`, `codex-cli`, `gemini-cli`, or `agent`

**Optional flags:**
- `--interviewer auto` — auto-approve HITL gates (default when not a terminal)
- `--run-id <ID>` — custom run identifier
- `--logs-root <DIR>` — where artifacts are written (auto-generated if omitted)

If the user wants CXDB persistence, remove the `FORGE_CXDB_PERSISTENCE=off` env var
(requires a running CXDB server).

### Step 5: Report the link

After starting, tell the user:

> Pipeline visualizer is running at **https://144.126.145.59/forge**
>
> (Self-signed certificate — accept the browser warning on first visit.)

If they want to tail the logs: `tail -f /tmp/forge-watch.log`

## Stopping the Visualizer

```bash
pkill -f "forge-cli.*watch" 2>/dev/null && echo "Stopped." || echo "Not running."
```

## Checking Status

```bash
pgrep -af "forge-cli.*watch" && echo "--- Log tail ---" && tail -5 /tmp/forge-watch.log
```

## Common Scenarios

### "Watch this pipeline on my phone"

1. Ask which DOT file and backend (suggest `mock` for a quick demo, `claude-code` for real)
2. Follow the start steps above
3. Give them the link: `https://144.126.145.59/forge`

### "Stop the visualizer"

Run the stop command above.

### "It's not loading"

Check:
1. Is the process running? (`pgrep -af forge-cli`)
2. Is Caddy routing? (`curl -sk https://144.126.145.59/forge`)
3. Is the port in use? (`ss -tlnp | grep 8384`)
4. Check logs: `tail -20 /tmp/forge-watch.log`

### "Run a different pipeline"

Stop the current one first, then start with the new DOT file. Only one watch server
runs at a time on port 8384.

## Available Example Pipelines

| File | Description | Nodes |
|------|-------------|-------|
| `examples/01-linear-foundation.dot` | Simple linear: plan → summarize | 4 |
| `examples/02-hitl-review-gate.dot` | HITL review with approve/reject loop | 6 |
| `examples/03-parallel-triage-and-fanin.dot` | Parallel fan-out, merge, decision gate | 10 |
