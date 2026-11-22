# Quickstart

This page gets lotel running and shows you what your coding agents did. To get the binary, see [Install](install.md).

## Run lotel

Start it from any directory:

```sh
lotel
```

lotel creates a `lotel-data` directory in the current directory and prints a banner like this:

```
lotel 0.1.0 (<git sha> · duckdb 2.0.0 · capped 0.1.0)
lotel: store <data dir>
lotel: OTLP on otlp:127.0.0.1:4318 (bearer token: <token>)
lotel: UI on http://127.0.0.1:8080
```

Two ports matter:

| Port | What it serves |
| --- | --- |
| `8080` | The UI, at http://127.0.0.1:8080 |
| `4318` | The OTLP/HTTP receiver your agents send to |

Copy the bearer token from the banner. Every request to port 4318 needs it.

Both ports bind to `127.0.0.1` by default, so only your machine can reach them. To change addresses, see [Configuration](configuration.md).

## Keep the token stable

If you don't set a token, lotel makes a new random one on every start. Agents you configured with the old token then get `401` responses. To keep one token across restarts, set your own. It must be at least 16 characters:

```sh
export LOTEL_INGEST_TOKEN=$(openssl rand -hex 16)
lotel
```

## Point your agents at it

Open http://127.0.0.1:8080/setup. The Setup page prints copy-paste blocks with this instance's endpoint and token already filled in. The blocks below show the same settings with placeholders.

### Claude Code

Paste this into the shell you start `claude` from:

```sh
export CLAUDE_CODE_ENABLE_TELEMETRY=1
export CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1
export OTEL_METRICS_EXPORTER=otlp
export OTEL_LOGS_EXPORTER=otlp
export OTEL_TRACES_EXPORTER=otlp
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer <token>'
export OTEL_METRIC_EXPORT_INTERVAL=10000
export OTEL_LOGS_EXPORT_INTERVAL=5000
```

You can put the same variables, without `export`, in the `env` map of `~/.claude/settings.json`.

### Codex

Add this to your user-level `~/.codex/config.toml`. Codex ignores `[otel]` in project configs.

```toml
[otel]
environment = "local"
log_user_prompt = false
metrics_exporter = "none"

[otel.exporter.otlp-http]
endpoint = "http://127.0.0.1:4318/v1/logs"
protocol = "binary"
headers = { "Authorization" = "Bearer <token>" }

[otel.trace_exporter.otlp-http]
endpoint = "http://127.0.0.1:4318/v1/traces"
protocol = "binary"
headers = { "Authorization" = "Bearer <token>" }
```

To try it on a single run without touching your config, pass the same settings as `-c` overrides:

```sh
codex exec \
  -c 'otel.log_user_prompt=false' \
  -c 'otel.exporter={otlp-http={endpoint="http://127.0.0.1:4318/v1/logs",protocol="binary",headers={Authorization="Bearer <token>"}}}' \
  -c 'otel.trace_exporter={otlp-http={endpoint="http://127.0.0.1:4318/v1/traces",protocol="binary",headers={Authorization="Bearer <token>"}}}' \
  "your task"
```

### opencode

The Setup page has no opencode tab yet. lotel reads opencode's logs and traces when they arrive: point opencode's OpenTelemetry exporter at `http://127.0.0.1:4318` with the header `Authorization: Bearer <token>`. Check opencode's own documentation for how to turn its telemetry on.

Note that opencode logs its full command line, prompt included. lotel hides it in the UI. See [Privacy](privacy.md).

### Any other app

Any OpenTelemetry SDK can send to lotel. See [Sending telemetry](sending-telemetry.md).

## What you see first

On the first start, while the log table is empty, lotel adds three demo log rows from services called `checkout` and `cart`. They give the UI something to show and age out like any other row.

Then run your agent and give it a prompt. Within a few seconds:

- **Sessions** (`/sessions`) shows one row for the session, with its event and span counts, cost and tokens when the agent reports them, and when it was last seen. Click it for the session timeline.
- **Logs** (`/logs`) shows the agent's events. `/logs?range=all&f=event.name%3Duser_prompt` lists the prompt events.
- **Spend** (`/spend`) and **Tools** (`/tools`) fill in as the agent reports cost metrics and tool calls.
- **Dashboard** (`/`) shows row counts per signal, ingest stats and the store's tables.

If a list is empty under a short time range, the page offers a link to the same view with `range=all`.

For a tour of every page, see [The UI](ui.md).

## Next steps

- [Retention](retention.md): how long lotel keeps data and how to change it.
- [Privacy](privacy.md): what lotel stores and what it shows.
- [Configuration](configuration.md): every environment variable.
