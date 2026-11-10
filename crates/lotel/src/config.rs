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
        })
    }
}
