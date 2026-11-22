use std::{env, path::PathBuf};

/// Runtime configuration, environment-only (Fanout-style: read once at boot).
#[derive(Debug, Clone)]
pub struct Config {
    /// UI/API bind address.
    pub host: String,
    pub port: u16,
    /// Data directory: app catalog + attached capped stores + materialized extensions.
    pub data_dir: PathBuf,
    /// Directory holding prebuilt `.duckdb_extension` files (capped, otlp).
    /// Overrides the copy embedded in the binary.
    pub extension_dir: Option<PathBuf>,
    /// Per-signal retention, passed straight into the table `WITH (...)` policies.
    pub ttl_logs: String,
    pub ttl_traces: String,
    pub ttl_metrics: String,
    /// Size cap per signal table (kept generous; the point is age-first retention).
    pub max_bytes: String,
    /// OTLP/HTTP listen address (same server also answers /v1/traces, /v1/metrics).
    pub otlp_addr: String,
    /// Ingest bearer token; generated at boot when unset.
    pub ingest_token: Option<String>,
    /// Show agent content — prompt text, tool input/output — verbatim instead
    /// of masking it at render. Off by default:
    /// what an agent sent stays hidden unless the operator opts in, and then
    /// the banner and the page markers say so.
    pub agent_show_content: bool,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let addr = env::var("LOTEL_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
        let (host, port) = addr
            .rsplit_once(':')
            .ok_or_else(|| anyhow::anyhow!("LOTEL_ADDR must be host:port, got {addr}"))?;
        let port = port
            .parse()
            .map_err(|_| anyhow::anyhow!("LOTEL_ADDR port is not a number: {port}"))?;

        Ok(Self {
            host: host.to_owned(),
            port,
            data_dir: env::var("LOTEL_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("lotel-data")),
            extension_dir: env::var("LOTEL_EXTENSION_DIR").ok().map(PathBuf::from),
            ttl_logs: env::var("LOTEL_TTL_LOGS").unwrap_or_else(|_| "7 days".into()),
            ttl_traces: env::var("LOTEL_TTL_TRACES").unwrap_or_else(|_| "3 days".into()),
            ttl_metrics: env::var("LOTEL_TTL_METRICS").unwrap_or_else(|_| "30 days".into()),
            max_bytes: env::var("LOTEL_MAX_BYTES").unwrap_or_else(|_| "2GB".into()),
            otlp_addr: env::var("LOTEL_OTLP_ADDR").unwrap_or_else(|_| "127.0.0.1:4318".into()),
            ingest_token: env::var("LOTEL_INGEST_TOKEN").ok().filter(|s| !s.is_empty()),
            agent_show_content: show_content(env::var("LOTEL_AGENT_SHOW_CONTENT").ok().as_deref()),
        })
    }

    /// Stored text as a page may display it: through the shared render
    /// redactor ([`crate::agents::redact_cell`]) unless the operator
    /// opted in with `LOTEL_AGENT_SHOW_CONTENT`. Nothing here touches the
    /// store — masking is a display-time decision, and the rows keep whatever
    /// the agent sent until the retention TTL forgets them.
    pub(crate) fn display(&self, s: &str) -> String {
        if self.agent_show_content {
            s.to_owned()
        } else {
            crate::agents::redact_cell(s)
        }
    }
}

/// `LOTEL_AGENT_SHOW_CONTENT` is an opt-in, so only an explicit yes counts:
/// `1`, `true`, `on`, `yes` (any case, blanks trimmed) turn the redactor off;
/// unset, empty, `0` and junk all keep content masked.
fn show_content(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|s| s.trim().to_ascii_lowercase()),
        Some(v) if matches!(v.as_str(), "1" | "true" | "on" | "yes")
    )
}

#[cfg(test)]
mod tests {
    use super::show_content;

    #[test]
    fn show_content_is_off_unless_told_otherwise() {
        assert!(!show_content(None), "unset is the privacy default");
        assert!(!show_content(Some("")));
        assert!(!show_content(Some("0")));
        assert!(!show_content(Some("false")));
        assert!(!show_content(Some("off")));
        assert!(!show_content(Some("no")));
        assert!(!show_content(Some("banana")));
    }

    #[test]
    fn show_content_accepts_the_usual_yes_spellings() {
        for v in ["1", "true", "TRUE", "on", "On", "yes", " Yes ", "  1"] {
            assert!(show_content(Some(v)), "{v} must opt in");
        }
    }
}
