//! `/setup`: copy-paste blocks that point Claude Code and Codex at
//! this lotel, with this instance's OTLP endpoint and ingest token filled in.
//!
//! Server-rendered, no JS: the three panes are `?tab=` links over one page.
//! The host is the one the browser used to reach the UI (the `Host` header,
//! validated), falling back to the OTLP listen host; the port is always the
//! OTLP listener's. The blocks never name the agents' content gates
//! (`OTEL_LOG_USER_PROMPTS`, `OTEL_LOG_TOOL_DETAILS`, …) — not even to say
//! "leave these unset", since that advertises the knob.

use topcoat::{
    Result,
    context::{Cx, context},
    router::{
        page,
        request::{headers, uri},
    },
    view::{View, component, view},
};

use crate::{config::Config, filters::Bar, ingest::Ingest, ui::{content_marker, content_policy}};

/// The panes, in tab order: (id, label).
const TABS: [(&str, &str); 3] = [
    ("claude", "Claude Code"),
    ("codex", "Codex: config file"),
    ("codex-run", "Codex: one run"),
];

/// `otlp:host:port` (as the extension registers it) → (host, port).
fn listen_host_port(listen_uri: &str) -> (String, String) {
    let hp = listen_uri.trim_start_matches("otlp:");
    match hp.rsplit_once(':') {
        Some((h, p)) => (h.trim_matches(['[', ']']).to_string(), p.to_string()),
        None => (hp.to_string(), "4318".to_string()),
    }
}

/// The hostname from a `Host` header value, when it is a plain hostname or
/// address (anything else — a header someone crafted — is ignored).
fn host_from_header(value: &str) -> Option<String> {
    let host = match value.strip_prefix('[') {
        // [v6]:port
        Some(rest) => rest.split_once(']').map(|(h, _)| h)?,
        None => value.rsplit_once(':').map(|(h, _)| h).unwrap_or(value),
    };
    let ok = !host.is_empty()
        && host.len() <= 253
        && host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
    ok.then(|| host.to_string())
}

/// What an agent should send to: `http://host:port`, host-bracketed for v6.
/// A wildcard listen address (0.0.0.0 / ::) is never an address to send to,
/// so it only stands in when the request named no usable host.
pub(crate) fn otlp_base(listen_uri: &str, host_header: Option<&str>) -> String {
    let (listen_host, port) = listen_host_port(listen_uri);
    let host = host_header
        .and_then(host_from_header)
        .unwrap_or_else(|| match listen_host.as_str() {
            "0.0.0.0" | "::" | "" => "127.0.0.1".to_string(),
            h => h.to_string(),
        });
    if host.contains(':') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// Claude Code: the shell `export` block (also valid, minus `export`, as the
/// `env` map in `~/.claude/settings.json`).
pub(crate) fn claude_block(base: &str, token: &str) -> String {
    format!(
        "export CLAUDE_CODE_ENABLE_TELEMETRY=1\n\
         export CLAUDE_CODE_ENHANCED_TELEMETRY_BETA=1\n\
         export OTEL_METRICS_EXPORTER=otlp\n\
         export OTEL_LOGS_EXPORTER=otlp\n\
         export OTEL_TRACES_EXPORTER=otlp\n\
         export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf\n\
         export OTEL_EXPORTER_OTLP_ENDPOINT={base}\n\
         export OTEL_EXPORTER_OTLP_HEADERS='Authorization=Bearer {token}'\n\
         export OTEL_METRIC_EXPORT_INTERVAL=10000\n\
         export OTEL_LOGS_EXPORT_INTERVAL=5000\n"
    )
}

/// Codex: the `[otel]` section for `~/.codex/config.toml`, as standard
/// tables (an inline table split over lines isn't TOML 1.0). Metrics stay
/// off until Codex's metric names are pinned.
pub(crate) fn codex_toml(base: &str, token: &str) -> String {
    format!(
        "[otel]\n\
         environment = \"local\"\n\
         log_user_prompt = false\n\
         metrics_exporter = \"none\"\n\
         \n\
         [otel.exporter.otlp-http]\n\
         endpoint = \"{base}/v1/logs\"\n\
         protocol = \"binary\"\n\
         headers = {{ \"Authorization\" = \"Bearer {token}\" }}\n\
         \n\
         [otel.trace_exporter.otlp-http]\n\
         endpoint = \"{base}/v1/traces\"\n\
         protocol = \"binary\"\n\
         headers = {{ \"Authorization\" = \"Bearer {token}\" }}\n"
    )
}

/// Codex, one run: the same settings as `-c` overrides on `codex exec`.
pub(crate) fn codex_run(base: &str, token: &str) -> String {
    let exporter = |signal: &str| {
        format!(
            "{{otlp-http={{endpoint=\"{base}/v1/{signal}\",protocol=\"binary\",headers={{Authorization=\"Bearer {token}\"}}}}}}"
        )
    };
    format!(
        "codex exec \\\n  -c 'otel.log_user_prompt=false' \\\n  -c 'otel.exporter={}' \\\n  -c 'otel.trace_exporter={}' \\\n  \"your task\"\n",
        exporter("logs"),
        exporter("traces"),
    )
}

#[component]
async fn setup_pane(cx: &Cx, title: String, hint: String, block: String) -> Result<impl View> {
    Ok(view! {
        <div class="card">
            <h2>(title)</h2>
            <p class="muted">(hint)</p>
            <pre class="setup-block">(block)</pre>
            <p>
                "Then run the agent and give it a prompt. Its session shows up under "
                <a href="/sessions">"Sessions"</a>
                " within a few seconds, and its events under "
                <a href="/logs?range=all&amp;f=event.name%3Duser_prompt">"Logs"</a>
                "."
            </p>
        </div>
    })
}

/// `/setup`.
#[page("/setup")]
pub(crate) async fn setup_page(cx: &Cx) -> Result<impl View> {
    let ing = context::<Ingest>(cx);
    let shown = context::<Config>(cx).agent_show_content;
    let policy = format!(
        "On this instance {}, and everything an agent sends is forgotten at the retention line.",
        content_policy(shown)
    );
    let bar = Bar::parse(uri(cx).query().unwrap_or(""));
    let tab = match bar.get("tab") {
        t @ ("codex" | "codex-run") => t.to_string(),
        _ => "claude".to_string(),
    };
    let host = headers(cx).get("host").and_then(|v| v.to_str().ok());
    let base = otlp_base(&ing.listen_uri, host);
    let token = ing.token.clone();
    let (title, hint, block) = match tab.as_str() {
        "codex" => (
            "Codex: ~/.codex/config.toml",
            "Add this to your user-level Codex config (Codex ignores [otel] in project configs). \
             Codex logs and traces then come here for every run.",
            codex_toml(&base, &token),
        ),
        "codex-run" => (
            "Codex: one run",
            "The same settings for a single codex exec, without touching your config.",
            codex_run(&base, &token),
        ),
        _ => (
            "Claude Code",
            "Paste into the shell you start claude from, or put the same variables (without export) \
             in the env map of ~/.claude/settings.json.",
            claude_block(&base, &token),
        ),
    };

    Ok(view! {
        <h1>"Setup"</h1>
        <p class="sub">"Point Claude Code or Codex at this lotel. The endpoint and token below are this instance's."</p>
        <p class="meta">
            (policy)
            content_marker(shown: shown)
        </p>
        <div class="rtabs">
            for (id, label) in TABS {
                <a class=(if id == tab { "rtab on" } else { "rtab" }) href=(format!("/setup?tab={id}"))>(label)</a>
            }
        </div>
        setup_pane(title: title.to_string(), hint: hint.to_string(), block: block)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GATES: [&str; 4] = [
        "OTEL_LOG_USER_PROMPTS",
        "OTEL_LOG_TOOL_DETAILS",
        "OTEL_LOG_TOOL_CONTENT",
        "OTEL_LOG_RAW_API_BODIES",
    ];

    #[test]
    fn host_comes_from_the_request_port_from_the_listener() {
        let l = "otlp:127.0.0.1:14319";
        assert_eq!(otlp_base(l, Some("localhost:18080")), "http://localhost:14319");
        assert_eq!(otlp_base(l, Some("devbox.lan")), "http://devbox.lan:14319");
        assert_eq!(otlp_base(l, None), "http://127.0.0.1:14319");
        assert_eq!(otlp_base(l, Some("[::1]:18080")), "http://[::1]:14319");
        assert_eq!(otlp_base(l, Some("evil.com/<script>")), "http://127.0.0.1:14319", "junk is ignored");
        assert_eq!(otlp_base("otlp:0.0.0.0:4318", None), "http://127.0.0.1:4318", "never send to a wildcard");
    }

    #[test]
    fn blocks_carry_endpoint_and_token_and_never_name_the_content_gates() {
        let base = "http://127.0.0.1:14319";
        let tok = "tok-0123456789";
        let cc = claude_block(base, tok);
        assert!(cc.contains("export CLAUDE_CODE_ENABLE_TELEMETRY=1"));
        assert!(cc.contains("export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:14319\n"));
        assert!(cc.contains("'Authorization=Bearer tok-0123456789'"));
        let toml = codex_toml(base, tok);
        assert!(toml.contains("log_user_prompt = false") && toml.contains("[otel.trace_exporter.otlp-http]"));
        assert!(toml.contains("endpoint = \"http://127.0.0.1:14319/v1/logs\""));
        let run = codex_run(base, tok);
        assert!(run.contains("otel.log_user_prompt=false") && run.contains("/v1/traces"));
        for block in [&cc, &toml, &run] {
            for g in GATES {
                assert!(!block.contains(g), "{g} must never appear on /setup");
            }
        }
    }
}
