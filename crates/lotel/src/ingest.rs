//! OTLP ingest: the community `otlp` extension runs the OTLP server inside
//! the embedded database and appends rows straight into the capped tables
//! (`catalog := 'otel'`). Retention is then entirely capped's job.

use std::{fs, io::Read};

use crate::config::Config;
use crate::db::Db;

/// Materialized next to the data dir on boot (embedded → binary is
/// self-contained; `LOTEL_EXTENSION_DIR` overrides for development).
/// `pub(crate)` so the Fields panel's fixture tests can `LOAD` it and read
/// the OTLP JSON fixtures with `read_otlp_*`.
pub(crate) const OTLP_EXTENSION: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/otlp.duckdb_extension"));

/// The canonical duckdb-otlp table layout (v0.5 schema) with capped policies.
/// Kept in sync with the bundled otlp extension; `just fetch-extensions`
/// documents how the snapshot was taken. `{ttl_*}` and `{max_bytes}` are
/// filled from the environment.
pub(crate) const TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS otel.main."otlp_logs"(time_unix_nano TIMESTAMP, observed_time_unix_nano TIMESTAMP, trace_id VARCHAR, span_id VARCHAR, service_name VARCHAR, service_namespace VARCHAR, service_instance_id VARCHAR, severity_number INTEGER, severity_text VARCHAR, event_name VARCHAR, body VARCHAR, resource_attributes VARCHAR, scope_name VARCHAR, scope_version VARCHAR, scope_attributes VARCHAR, log_attributes VARCHAR, dropped_attributes_count INTEGER, flags INTEGER) WITH (time_col = 'time_unix_nano', ttl = '{ttl_logs}', max_bytes = '{max_bytes}', search_columns = 'time_unix_nano,service_name,severity_text,event_name,body,trace_id');
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
    // The otlp extension refuses to bind anything but a loopback host unless
    // the caller passes allow_other_hostname=true. Containers must publish
    // the port (127.0.0.1 is unreachable from the host), so a non-loopback
    // LOTEL_OTLP_ADDR is explicit operator intent — the ingest token still
    // guards every request. Loopback hosts keep sending today's exact SQL and
    // stay under the extension's default guard. Loopback set mirrors
    // OtlpUri::IsLocal (duckdb-otlp): localhost / ::1 / all of 127.0.0.0/8.
    let host = cfg.otlp_addr.rsplit_once(':').map_or("", |(h, _)| h);
    let loopback = matches!(host.to_lowercase().as_str(), "localhost" | "::1")
        || host.starts_with("127.");
    let allow_other_sql = if loopback { "" } else { ", allow_other_hostname := true" };

    db.with_conn(|conn| {
        conn.execute_batch(&format!("LOAD {};", crate::db::quote(&otlp_path.to_string_lossy())))?;
        conn.execute_batch(&ddl)?;
        // Pre-search stores: otlp_logs created before search_columns exists
        // cannot grow them in place — rebuild it once, and say so loudly.
        let probe = conn.query_row(
            "SELECT * FROM capped_fts('otel', 'otlp_logs', 'lotel-probe', 1)",
            [],
            |_| Ok(()),
        );
        if probe.is_err_and(|e| e.to_string().contains("no full-text search columns")) {
            eprintln!(
                "lotel: rebuilding otel.main.otlp_logs to enable full-text search \
                 (rows in that table are discarded; other signals are untouched)"
            );
            conn.execute_batch("DROP TABLE otel.main.otlp_logs;")?;
            conn.execute_batch(&ddl)?;
        }
        conn.execute_batch(&format!(
            "FROM otlp_serve('{}', token := '{}', catalog := 'otel'{});",
            listen.replace('\'', "''"),
            token.replace('\'', "''"),
            allow_other_sql,
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
    if cfg.extension_dir.is_some() {
        if !path.exists() {
            anyhow::bail!("LOTEL_EXTENSION_DIR set but no otlp.duckdb_extension in {}", ext_dir.display());
        }
    } else {
        // Rewrite every boot, like capped and fts: after an upgrade the data
        // dir must not keep loading the previous binary's otlp build.
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

/// The limits the store actually enforces. capped fixes a table's policy when
/// the table is created, so an existing data directory keeps its original
/// limits whatever `LOTEL_TTL_*` / `LOTEL_MAX_BYTES` say now. This replaces the
/// config's values with the tables' own (every "kept 7d" in the UI reads the
/// config) and returns one warning per limit the environment asked to change.
pub fn adopt_store_retention(db: &Db, cfg: &mut Config) -> anyhow::Result<Vec<String>> {
    let (_, rows) = db.query_rows("SELECT table_name, ttl_us, max_bytes FROM capped_tables('otel')")?;
    let find = |table: &str| rows.iter().find(|r| r.first().map(String::as_str) == Some(table));
    let mut warnings = Vec::new();
    let mut adopt = |what: &str, var: &str, table: &str, slot: &mut String| {
        let Some(us) = find(table).and_then(|r| r.get(1)).and_then(|v| v.parse::<i64>().ok()) else {
            return;
        };
        let actual = ttl_from_us(us);
        if parse_ttl_us(slot).is_some_and(|want| want != us) {
            warnings.push(format!(
                "lotel: warning: {var}={slot} is ignored: this data directory keeps {what} for {actual}, \
                 set when its tables were created. Use a new LOTEL_DATA_DIR to change it."
            ));
        }
        *slot = actual;
    };
    adopt("logs", "LOTEL_TTL_LOGS", "otlp_logs", &mut cfg.ttl_logs);
    adopt("traces", "LOTEL_TTL_TRACES", "otlp_traces", &mut cfg.ttl_traces);
    adopt("metrics", "LOTEL_TTL_METRICS", "otlp_metrics_gauge", &mut cfg.ttl_metrics);
    if let Some(bytes) = find("otlp_logs").and_then(|r| r.get(2)).and_then(|v| v.parse::<u64>().ok()) {
        if parse_size(&cfg.max_bytes).is_some_and(|want| want != bytes) {
            warnings.push(format!(
                "lotel: warning: LOTEL_MAX_BYTES={} is ignored: this data directory caps each table at {bytes} bytes, \
                 set when its tables were created. Use a new LOTEL_DATA_DIR to change it.",
                cfg.max_bytes
            ));
        }
        cfg.max_bytes = format!("{bytes}B");
    }
    Ok(warnings)
}

const US_PER: [(&str, i64); 4] = [("day", 86_400_000_000), ("hour", 3_600_000_000), ("minute", 60_000_000), ("second", 1_000_000)];

/// "7 days" style text for a TTL in microseconds, in the largest whole unit.
fn ttl_from_us(us: i64) -> String {
    for (unit, n) in US_PER {
        if us >= n && us % n == 0 {
            let k = us / n;
            return format!("{k} {unit}{}", if k == 1 { "" } else { "s" });
        }
    }
    format!("{us} microseconds")
}

/// "<n> <unit>" with day/hour/minute/second (singular or plural) to
/// microseconds; anything else is None (no comparison, no warning).
fn parse_ttl_us(s: &str) -> Option<i64> {
    let (n, unit) = s.trim().split_once(' ')?;
    let n: i64 = n.trim().parse().ok()?;
    let unit = unit.trim().trim_end_matches('s');
    US_PER.iter().find(|(u, _)| *u == unit).map(|(_, per)| n * per)
}

/// "2GB" / "64MB" / "512KB" / "100B" (decimal units, as capped reads them).
fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim().to_ascii_uppercase();
    let (num, mult) = [("TB", 1_000_000_000_000u64), ("GB", 1_000_000_000), ("MB", 1_000_000), ("KB", 1_000), ("B", 1)]
        .iter()
        .find_map(|(suf, m)| t.strip_suffix(suf).map(|n| (n.trim().to_string(), *m)))?;
    num.parse::<u64>().ok().map(|n| n * mult)
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn ttl_text_round_trips_through_microseconds() {
        assert_eq!(ttl_from_us(7 * 86_400_000_000), "7 days");
        assert_eq!(ttl_from_us(86_400_000_000), "1 day");
        assert_eq!(ttl_from_us(36 * 3_600_000_000), "36 hours");
        assert_eq!(parse_ttl_us("7 days"), Some(7 * 86_400_000_000));
        assert_eq!(parse_ttl_us("1 day"), Some(86_400_000_000));
        assert_eq!(parse_ttl_us("a week"), None);
        assert_eq!(parse_size("2GB"), Some(2_000_000_000));
        assert_eq!(parse_size("64mb"), Some(64_000_000));
        assert_eq!(parse_size("lots"), None);
    }
}
