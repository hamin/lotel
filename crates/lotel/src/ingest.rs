//! OTLP ingest: the community `otlp` extension runs the OTLP server inside
//! the embedded database and appends rows straight into the capped tables
//! (`catalog := 'otel'`). Retention is then entirely capped's job.

use std::{fs, io::Read};

use crate::config::Config;
use crate::db::Db;

/// Materialized next to the data dir on boot (embedded → binary is
/// self-contained; `LOTEL_EXTENSION_DIR` overrides for development).
const OTLP_EXTENSION: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/extensions/otlp.duckdb_extension"));

/// The canonical duckdb-otlp table layout (v0.5 schema) with capped policies.
/// Kept in sync with the bundled otlp extension; `just fetch-extensions`
/// documents how the snapshot was taken. `{ttl_*}` and `{max_bytes}` are
/// filled from the environment.
const TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS otel.main."otlp_logs"(time_unix_nano TIMESTAMP, observed_time_unix_nano TIMESTAMP, trace_id VARCHAR, span_id VARCHAR, service_name VARCHAR, service_namespace VARCHAR, service_instance_id VARCHAR, severity_number INTEGER, severity_text VARCHAR, event_name VARCHAR, body VARCHAR, resource_attributes VARCHAR, scope_name VARCHAR, scope_version VARCHAR, scope_attributes VARCHAR, log_attributes VARCHAR, dropped_attributes_count INTEGER, flags INTEGER) WITH (time_col = 'time_unix_nano', ttl = '{ttl_logs}', max_bytes = '{max_bytes}');
CREATE TABLE IF NOT EXISTS otel.main."otlp_traces"(start_time_unix_nano TIMESTAMP, duration_time_unix_nano BIGINT, trace_id VARCHAR, span_id VARCHAR, parent_span_id VARCHAR, trace_state VARCHAR, service_name VARCHAR, service_namespace VARCHAR, service_instance_id VARCHAR, "name" VARCHAR, kind INTEGER, status_code INTEGER, status_status_message VARCHAR, resource_attributes VARCHAR, scope_name VARCHAR, scope_version VARCHAR, scope_attributes VARCHAR, span_attributes VARCHAR, events_json VARCHAR, links_json VARCHAR, dropped_attributes_count INTEGER, dropped_events_count INTEGER, dropped_links_count INTEGER, flags INTEGER) WITH (time_col = 'start_time_unix_nano', ttl = '{ttl_traces}', max_bytes = '{max_bytes}');
CREATE TABLE IF NOT EXISTS otel.main."otlp_metrics_gauge"(time_unix_nano TIMESTAMP, start_time_unix_nano TIMESTAMP, "name" VARCHAR, description VARCHAR, unit VARCHAR, int_value BIGINT, double_value DOUBLE, service_name VARCHAR, service_namespace VARCHAR, service_instance_id VARCHAR, resource_attributes VARCHAR, scope_name VARCHAR, scope_version VARCHAR, scope_attributes VARCHAR, metric_attributes VARCHAR, flags INTEGER, exemplars_json VARCHAR) WITH (time_col = 'time_unix_nano', ttl = '{ttl_metrics}', max_bytes = '{max_bytes}');
CREATE TABLE IF NOT EXISTS otel.main."otlp_metrics_sum"(time_unix_nano TIMESTAMP, start_time_unix_nano TIMESTAMP, "name" VARCHAR, description VARCHAR, unit VARCHAR, int_value BIGINT, double_value DOUBLE, service_name VARCHAR, service_namespace VARCHAR, service_instance_id VARCHAR, resource_attributes VARCHAR, scope_name VARCHAR, scope_version VARCHAR, scope_attributes VARCHAR, metric_attributes VARCHAR, flags INTEGER, exemplars_json VARCHAR, aggregation_temporality INTEGER, is_monotonic BOOLEAN) WITH (time_col = 'time_unix_nano', ttl = '{ttl_metrics}', max_bytes = '{max_bytes}');
CREATE TABLE IF NOT EXISTS otel.main."otlp_metrics_histogram"(time_unix_nano TIMESTAMP, start_time_unix_nano TIMESTAMP, "name" VARCHAR, description VARCHAR, unit VARCHAR, count BIGINT, sum DOUBLE, min DOUBLE, max DOUBLE, bucket_counts BIGINT[], explicit_bounds DOUBLE[], service_name VARCHAR, service_namespace VARCHAR, service_instance_id VARCHAR, resource_attributes VARCHAR, scope_name VARCHAR, scope_version VARCHAR, scope_attributes VARCHAR, metric_attributes VARCHAR, flags INTEGER, exemplars_json VARCHAR, aggregation_temporality INTEGER) WITH (time_col = 'time_unix_nano', ttl = '{ttl_metrics}', max_bytes = '{max_bytes}');
CREATE TABLE IF NOT EXISTS otel.main."otlp_metrics_exp_histogram"(time_unix_nano TIMESTAMP, start_time_unix_nano TIMESTAMP, "name" VARCHAR, description VARCHAR, unit VARCHAR, count BIGINT, sum DOUBLE, min DOUBLE, max DOUBLE, scale INTEGER, zero_count BIGINT, zero_threshold DOUBLE, positive_offset INTEGER, positive_bucket_counts BIGINT[], negative_offset INTEGER, negative_bucket_counts BIGINT[], service_name VARCHAR, service_namespace VARCHAR, service_instance_id VARCHAR, resource_attributes VARCHAR, scope_name VARCHAR, scope_version VARCHAR, scope_attributes VARCHAR, metric_attributes VARCHAR, flags INTEGER, exemplars_json VARCHAR, aggregation_temporality INTEGER) WITH (time_col = 'time_unix_nano', ttl = '{ttl_metrics}', max_bytes = '{max_bytes}');
"#;

pub struct Ingest {
    /// e.g. "otlp:localhost:4318"
    pub listen_uri: String,
    pub token: String,
}

/// Load the otlp extension from `ext_dir`, create the capped signal tables,
/// and start the OTLP server writing directly into the store.
pub fn start(db: &Db, cfg: &Config, ext_dir: &std::path::Path) -> anyhow::Result<Ingest> {
    let otlp_path = materialize_otlp(cfg, ext_dir)?;

    let ddl = TABLES
        .replace("{ttl_logs}", &cfg.ttl_logs)
        .replace("{ttl_traces}", &cfg.ttl_traces)
        .replace("{ttl_metrics}", &cfg.ttl_metrics)
        .replace("{max_bytes}", &cfg.max_bytes);

    let token = cfg.ingest_token.clone().unwrap_or_else(random_token);
    let listen = format!("otlp:{}", cfg.otlp_addr);

    db.with_conn(|conn| {
        conn.execute_batch(&format!("LOAD {};", crate::db::quote(&otlp_path.to_string_lossy())))?;
        conn.execute_batch(&ddl)?;
        conn.execute_batch(&format!(
            "FROM otlp_serve('{}', token := '{}', catalog := 'otel');",
            listen.replace('\'', "''"),
            token.replace('\'', "''"),
        ))?;
        Ok(())
    })?;

    seed_demo_if_empty(db)?;

    Ok(Ingest { listen_uri: listen, token })
}

/// First run gets a handful of rows so the UI has a heartbeat on boot.
/// Timestamps are generated in Rust: the `bundled` duckdb build carries no
/// ICU, so timezone functions (`now() AT TIME ZONE …`) are off the table.
fn seed_demo_if_empty(db: &Db) -> anyhow::Result<()> {
    let now_utc = chrono::Utc::now().naive_utc().format("%Y-%m-%d %H:%M:%S%.f").to_string();
    db.with_conn(|conn| {
        let n: i64 = conn.query_row("SELECT count(*) FROM otel.main.otlp_logs", [], |r| r.get(0))?;
        if n > 0 {
            return Ok(());
        }
        conn.execute_batch(&format!(
            "
            INSERT INTO otel.main.otlp_logs
                (time_unix_nano, service_name, severity_number, severity_text, body, log_attributes)
            VALUES
                ('{now_utc}', 'checkout', 9,  'INFO',  'payment authorized', '{{}}'),
                ('{now_utc}', 'checkout', 17, 'ERROR', 'gateway timeout after 3 retries', '{{\"code\":\"ETIMEDOUT\"}}'),
                ('{now_utc}', 'cart',     13, 'WARN',  'cache miss ratio above 0.9', '{{}}');
            ",
        ))?;
        Ok(())
    })
}

impl Ingest {
    /// Live server stats (buffered/committed rows, seal counters) for the UI.
    pub fn stats(&self, db: &Db) -> anyhow::Result<(Vec<String>, Vec<Vec<String>>)> {
        db.query_rows(&format!(
            "SELECT listen_url, total_requests, total_rows, buffered_rows, seals_total, committed_rows_total, COALESCE(last_error, '') AS last_error \
             FROM otlp_server_list() WHERE listen_uri = '{}'",
            self.listen_uri.replace('\'', "''")
        ))
    }
}

fn materialize_otlp(cfg: &Config, ext_dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let path = ext_dir.join("otlp.duckdb_extension");
    if !path.exists() {
        if cfg.extension_dir.is_some() {
            anyhow::bail!("LOTEL_EXTENSION_DIR set but no otlp.duckdb_extension in {}", ext_dir.display());
        }
        fs::write(&path, OTLP_EXTENSION)?;
    }
    Ok(path)
}

fn random_token() -> String {
    let mut buf = [0u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .expect("need entropy for the ingest token");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}
