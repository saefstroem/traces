# traces

A lightweight local HTTPS MITM proxy in Rust that records LLM API traffic (prompts, thinking, tool calls, outputs, and errors) and streams them to any S3-compatible object store.

## Features

- **Zero-config or explicit interception** - Intercept traffic via `HTTPS_PROXY` (`CONNECT` on `127.0.0.1:7646`) or transparently via `/etc/hosts`
- **HTTP/1.1, HTTP/2 & gRPC** - Mirrors ALPN (`h2` / `http/1.1`) between client and upstream. WebSocket is not supported; upgrades on provider routes return `426 Upgrade Required`.
- **Multi-provider normalization** - Extracts a uniform stream of `input`, `thinking`, `tool`, `output`, and `error` events across Anthropic, OpenAI, Gemini, Antigravity, Ollama, and compatible vendors.
- **Buffered S3 uploads** - Batches traces in memory per `<provider>/<model>/<session>` prefix and flushes on a time interval, memory limit, or graceful shutdown (`SIGINT` / `SIGTERM`), restoring failed segments in order.
- **System service support** - Built-in `traces install`, `traces start`, and `traces uninstall` commands for macOS (`launchd`) and Linux (`systemd`).

## Supported Providers

| Path Suffix | Provider | Compatible APIs & Clients |
|---|---|---|
| `/v1/messages` | `anthropic` | Anthropic API, Claude Code, GitHub Copilot, DeepSeek, Kimi, Z.ai, MiniMax |
| `:streamRawPredict` | `anthropic` | Claude on Vertex AI |
| `/chat/completions` | `openai_chat` | OpenAI, Azure OpenAI, OpenRouter, Groq, Mistral, DeepSeek, xAI, Together, Fireworks, vLLM, Ollama |
| `/responses` | `openai_responses` | OpenAI Responses API, Codex, GitHub Copilot, xAI, Groq, Azure, Bedrock |
| `/v1/agent` | `openai_responses` | Perplexity Agent API |
| `:streamGenerateContent` | `gemini` | Gemini API, Vertex AI, Gemini CLI, Gemini Code Assist, Antigravity |
| `/interactions` | `gemini_interactions` | Gemini Interactions API |
| `/api/chat`, `/api/generate` | `ollama` | Native Ollama API (NDJSON) |

## Quick Start

### 1. Configure environment

Create a `.env` file in the repository root (shell environment variables take precedence over `.env`):

```env
TRACES_S3_ENDPOINT=http://127.0.0.1:8333
TRACES_S3_REGION=us-east-1
TRACES_S3_BUCKET=traces
TRACES_S3_ACCESS_KEY_ID=your-access-key
TRACES_S3_SECRET_ACCESS_KEY=your-secret-key
TRACES_PURGE_SECONDS=30
TRACES_MAX_MEMORY_MIB=64
```

| Variable | Description |
|---|---|
| `TRACES_S3_ENDPOINT` | S3-compatible endpoint URL (uses path-style addressing) |
| `TRACES_S3_REGION` | Region name (e.g. `auto` for R2, `us-east-1` for SeaweedFS/MinIO) |
| `TRACES_S3_BUCKET` | Target bucket name |
| `TRACES_S3_ACCESS_KEY_ID` | S3 access key ID |
| `TRACES_S3_SECRET_ACCESS_KEY` | S3 secret access key |
| `TRACES_PURGE_SECONDS` | Max seconds traces wait in memory before flushing (`> 0`) |
| `TRACES_MAX_MEMORY_MIB` | Memory threshold in MiB that triggers an immediate flush (`> 0`) |

### 2. Install and run

```bash
cargo install traces
traces
```

Control log verbosity with `RUST_LOG` (defaults to `traces=info`):

```bash
RUST_LOG=traces=debug traces
```

On first run, `traces` generates a local root CA at `~/.traces/ca.crt` and `~/.traces/ca.key` (`0600`).

### 3. Route traffic through `traces`

#### Option A: Explicit `HTTPS_PROXY` (per command)

Point any client at `http://127.0.0.1:7646` and trust `~/.traces/ca.crt`:

```bash
# Node.js / Claude Code / VS Code
HTTPS_PROXY=http://127.0.0.1:7646 \
NODE_USE_ENV_PROXY=1 \
NODE_EXTRA_CA_CERTS=~/.traces/ca.crt \
claude

# Python (requests / httpx)
HTTPS_PROXY=http://127.0.0.1:7646 \
SSL_CERT_FILE=~/.traces/ca.crt \
REQUESTS_CA_BUNDLE=~/.traces/ca.crt \
python app.py

# C-core gRPC (Python grpcio / C++)
GRPC_DEFAULT_SSL_ROOTS_FILE_PATH=~/.traces/ca.crt
```

#### Option B: Transparent system service (`/etc/hosts` + port 443)

Install `traces` as a background service that trusts the local CA system-wide and redirects the LLM hosts in [`hosts`](hosts) to `127.0.0.1:443`:

```bash
# Run as your normal user (do NOT prefix with sudo; traces elevates only the root steps)
traces install
traces start
```

| Command | Action |
|---|---|
| `traces install` | Copies binary to `/usr/local/bin/traces`, installs CA in system trust store, updates `/etc/hosts`, registers launchd/systemd service |
| `traces start` | Starts the background service immediately (runs at boot thereafter) |
| `traces uninstall` | Stops and removes the service, trust store CA, `/etc/hosts` block, and `/usr/local/bin/traces` |

Service logs:
- **macOS (`launchd`)**: `~/.traces/traces.log`
- **Linux (`systemd`)**: `journalctl -u traces -f`

> **Note:** The background service reads `.env` from the working directory where `traces install` was run. Re-run `traces install` if you move the folder or update `hosts`.

## Storage Format

Each flush writes one JSON array object per active prefix:

```text
<provider>/<model>/<conversation>/<unix-ms>-<seq>.json
```

Example:

```text
anthropic/claude-sonnet-4-6/session-1/1795243052854-000001.json
```

- **`<provider>`**: matched route provider (`anthropic`, `gemini`, `openai_responses`, etc.).
- **`<model>`**: extracted from body `model`, URL `/models/<model>`, or `/deployments/<model>` (falls back to `unknown`).
- **`<conversation>`**: extracted from the first matching header in `prefix.headers` or JSON pointer in `prefix.body` in [`config.json`](config.json) (falls back to `unknown`). Side requests matching `prefix.skip` (such as `x-claude-code-request-class: auxiliary`) are logged but not stored.
- **Chronological ordering**: zero-padded millisecond timestamps and sequence counters ensure alphabetical listing matches chronological order.

Each `.json` object contains an array of normalized trace items:

```json
[
  { "input": { "role": "user", "content": "list the files" } },
  { "thinking": "I will run ls." },
  { "tool": { "name": "Bash", "input": "{\"command\": \"ls\"}" } },
  { "output": "The folder has one file." }
]
```

## Security notice

- `~/.traces/ca.key` is very sensitive, anyone who has access to it can sign certificates trusted by your machine. 0600 perms are applied.
- Recorded traces contain full prompts, tool arguments, tool results, and outputs, which may include sensitive data (such as `.env` contents read by an agent).
