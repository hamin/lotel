//! Embedded DuckDB: loads the bundled `capped` extension, attaches the store.
//!
//! DuckDB runs in-process via duckdb-rs (statically linked, version-pinned to
//! match the bundled extensions). Reads come from a small pool of connections
//! over one shared instance — DuckDB's concurrency model is "many connections,
//! one database", and serializing every page behind a single mutexed conn made
//! the stress script's readers queue behind each other (p95 seconds, not ms).
//! Peers come from `Connection::try_clone` (duckdb_connect on the same
//! handle). Re-opening the same *path* is not the way: it builds a separate
//! instance that never saw the `ATTACH otel` — every query there binder-errors
//! on the missing catalog, which is exactly how the first pool broke.
//! `LOAD` and `ATTACH` live on the instance, so every clone sees them.

use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use duckdb::Connection;

use crate::config::Config;

/// Materialized at compile time so one binary ships the storage layer.
/// `just fetch-extensions` refreshes these from the capped shell build.
const CAPPED_EXTENSION: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/extensions/capped.duckdb_extension"));

/// BM25 full-text indexes over sealed segments (capped_fts). Embedded so
/// search works on a machine with no DuckDB extension cache; capped's own
/// sidecar build LOADs fts and finds this one already loaded.
const FTS_EXTENSION: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/extensions/fts.duckdb_extension"));

/// SQL string literal with single quotes escaped.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Connections kept warm for concurrent readers. Pages issue several grouped
/// queries per render; a handful of peers lets that fan out instead of queue.
const MAX_CONNS: usize = 8;

pub struct Db {
    /// The connection that opened the instance and ran LOAD/ATTACH. Never
    /// used for queries — it is the seed every pooled peer is cloned from.
    seed: Arc<Mutex<Connection>>,
    pool: Arc<Mutex<Vec<Connection>>>,
    pub data_dir: PathBuf,
    /// Where the loadable extension files live (data dir or the dev override).
    pub ext_dir: PathBuf,
    pub capped_version: String,
}

impl Clone for Db {
    fn clone(&self) -> Self {
        Self {
            seed: self.seed.clone(),
            pool: self.pool.clone(),
            data_dir: self.data_dir.clone(),
            ext_dir: self.ext_dir.clone(),
            capped_version: self.capped_version.clone(),
        }
    }
}

impl Db {
    /// Open (creating if needed) the embedded database and attach the store.
    pub fn open(cfg: &Config) -> anyhow::Result<Self> {
        fs::create_dir_all(&cfg.data_dir)?;
        let ext_dir = Self::materialize_extensions(cfg)?;

        let conn = Connection::open_with_flags(
            cfg.data_dir.join("lotel.duckdb"),
            duckdb::Config::default().allow_unsigned_extensions()?,
        )?;

        let capped_path = ext_dir.join("capped.duckdb_extension");
        conn.execute_batch(&format!("LOAD {};", quote(&capped_path.to_string_lossy())))
            .map_err(|e| {
                anyhow::anyhow!("LOAD capped from {} failed: {e}", capped_path.display())
            })?;
        let fts_path = ext_dir.join("fts.duckdb_extension");
        conn.execute_batch(&format!("LOAD {};", quote(&fts_path.to_string_lossy())))
            .map_err(|e| {
                anyhow::anyhow!("LOAD fts from {} failed: {e}", fts_path.display())
            })?;
        // DuckDB defaults to 80% of RAM and one thread per core. For a tool
        // that should stay invisible on a laptop, the buffer pool gets a
        // leash. (This caps DuckDB's own accounting, not third-party
        // extension allocations — a warmup flood inside the otlp extension
        // can still push RSS well past this, though it frees again.)
        let limit = std::env::var("LOTEL_MEMORY_LIMIT").unwrap_or_else(|_| "1GB".into());
        conn.execute_batch(&format!("SET memory_limit = {};", quote(&limit)))
            .map_err(|e| anyhow::anyhow!("SET memory_limit='{limit}' failed: {e}"))?;
        if let Ok(threads) = std::env::var("LOTEL_THREADS") {
            conn.execute_batch(&format!("SET threads = {};", quote(&threads)))
                .map_err(|e| anyhow::anyhow!("SET threads='{threads}' failed: {e}"))?;
        }
        let capped_version: String = conn.query_row("SELECT capped_version()", [], |r| r.get(0))?;

        let store = cfg.data_dir.join("store");
        conn.execute_batch(&format!(
            "ATTACH {} AS otel (TYPE capped);",
            quote(&store.to_string_lossy())
        ))?;

        Ok(Self {
            seed: Arc::new(Mutex::new(conn)),
            pool: Arc::new(Mutex::new(Vec::new())),
            data_dir: cfg.data_dir.clone(),
            ext_dir,
            capped_version,
        })
    }

    /// A connection from the pool: reuse a warm peer or clone a new one off
    /// the seed — same instance, same attached catalogs.
    fn checkout(&self) -> anyhow::Result<Connection> {
        if let Some(conn) = self.pool.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            return Ok(conn);
        }
        let seed = self.seed.lock().unwrap_or_else(|e| e.into_inner());
        Ok(seed.try_clone()?)
    }

    /// Return a connection to the pool. One that just hit an error stays out:
    /// cheap insurance against handing a half-poisoned statement to the next
    /// page, since duckdb-rs connections are cheap to recreate. Tolerates
    /// mutex poisoning: a panic in one request must not take the UI down.
    fn give_back(&self, conn: Connection, healthy: bool) {
        if !healthy {
            return;
        }
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < MAX_CONNS {
            pool.push(conn);
        }
    }

    /// Run a closure against a pooled connection.
    pub(crate) fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let conn = self.checkout()?;
        let out = f(&conn);
        self.give_back(conn, out.is_ok());
        out
    }

    /// Write the embedded extension files next to the data dir (or use the dev
    /// override directory as-is).
    fn materialize_extensions(cfg: &Config) -> anyhow::Result<PathBuf> {
        if let Some(dir) = &cfg.extension_dir {
            for name in ["capped.duckdb_extension", "fts.duckdb_extension"] {
                if !dir.join(name).exists() {
                    anyhow::bail!(
                        "LOTEL_EXTENSION_DIR set but no {name} in {}",
                        dir.display()
                    );
                }
            }
            return Ok(dir.clone());
        }
        let dir = cfg.data_dir.join("extensions");
        fs::create_dir_all(&dir)?;
        // Rewrite every boot: cheap (tens of MB) and always in sync with the binary.
        fs::write(dir.join("capped.duckdb_extension"), CAPPED_EXTENSION)?;
        fs::write(dir.join("fts.duckdb_extension"), FTS_EXTENSION)?;
        Ok(dir)
    }

    /// Run a query and hand back column names + all values rendered as text.
    /// Used by the M0/M1 status pages; signal views get typed queries in M2+.
    pub fn query_rows(&self, sql: &str) -> anyhow::Result<(Vec<String>, Vec<Vec<String>>)> {
        self.query_rows_with(sql, &[])
    }

    /// Parameterized variant: values are bound, never interpolated.
    pub fn query_rows_with(
        &self,
        sql: &str,
        params: &[&dyn duckdb::ToSql],
    ) -> anyhow::Result<(Vec<String>, Vec<Vec<String>>)> {
        let conn = self.checkout()?;
        let out = self.run_rows(&conn, sql, params);
        self.give_back(conn, out.is_ok());
        out
    }

    fn run_rows(
        &self,
        conn: &Connection,
        sql: &str,
        params: &[&dyn duckdb::ToSql],
    ) -> anyhow::Result<(Vec<String>, Vec<Vec<String>>)> {
        let wrapped = format!("SELECT COLUMNS(*)::VARCHAR FROM ({sql})");

        // duckdb-rs only exposes column metadata after a *successful* execution:
        // probe once (an empty result is fine — it still executed), then read the
        // values. A real execution error must propagate here: reading column
        // names of a statement that never ran panics inside duckdb-rs
        // ("The statement was not executed yet"), and a panicking request would
        // poison the shared connection for every page behind it.
        let mut probe = conn.prepare(&wrapped).map_err(|e| {
            eprintln!("lotel: prepare failed: {e}\n  sql: {wrapped}");
            e
        })?;
        match probe.query_row(params, |_| Ok(())) {
            Ok(()) | Err(duckdb::Error::QueryReturnedNoRows) => {}
            Err(e) => {
                eprintln!("lotel: query failed: {e}\n  sql: {wrapped}");
                return Err(e.into());
            }
        }
        let columns: Vec<String> = (0..probe.column_count())
            .map(|i| {
                probe
                    .column_name(i)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|_| format!("col{i}"))
            })
            .collect();
        drop(probe);

        let mut stmt = conn.prepare(&wrapped).map_err(|e| {
            eprintln!("lotel: re-prepare failed: {e}\n  sql: {wrapped}");
            e
        })?;
        let rows = stmt.query_map(params, |row| {
            (0..columns.len())
                .map(|i| row.get::<_, Option<String>>(i).map(|v| v.unwrap_or_default()))
                .collect::<Result<Vec<_>, _>>()
        })?;
        // Drain every row even when one fails to convert, so the statement is
        // fully consumed before it drops; keep the first error to report.
        let mut out: Vec<Vec<String>> = Vec::new();
        let mut first_err: Option<duckdb::Error> = None;
        for r in rows {
            match r {
                Ok(v) => out.push(v),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }
        if let Some(e) = first_err {
            eprintln!("lotel: row fetch failed: {e}\n  sql: {wrapped}");
            return Err(e.into());
        }
        Ok((columns, out))
    }

    pub fn duckdb_version(&self) -> anyhow::Result<String> {
        self.with_conn(|conn| {
            Ok(conn.query_row("SELECT version()", [], |r| r.get::<_, String>(0))?)
        })
    }

    pub fn data_dir_display(&self) -> String {
        self.data_dir.canonicalize().unwrap_or(self.data_dir.clone()).display().to_string()
    }
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("data_dir", &self.data_dir)
            .field("capped_version", &self.capped_version)
            .finish()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Open a throwaway Db (real DuckDB + capped), one dir per test.
    fn temp_db(tag: &str) -> Db {
        let dir = std::env::temp_dir().join(format!("lotel-db-test-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        let cfg = Config {
            host: "127.0.0.1".into(),
            port: 0,
            data_dir: dir,
            extension_dir: None,
            ttl_logs: "7 days".into(),
            ttl_traces: "3 days".into(),
            ttl_metrics: "30 days".into(),
            max_bytes: "64MB".into(),
            otlp_addr: "127.0.0.1:0".into(),
            ingest_token: None,
        };
        Db::open(&cfg).expect("open temp db")
    }

    /// A failing query must surface as Err — not panic on column metadata for a
    /// statement that never executed, which took down every page behind the
    /// shared connection (the M14 field bug).
    #[test]
    fn failed_query_errors_without_poisoning_the_connection() {
        let db = temp_db("failed-query");
        let bad = db.query_rows("SELECT * FROM no_such_table_xyz");
        assert!(bad.is_err(), "failing query must return Err");
        let (cols, rows) = db
            .query_rows("SELECT 42 AS answer")
            .expect("later queries must still work");
        assert_eq!(cols, ["answer"]);
        assert_eq!(rows, vec![vec!["42".to_string()]]);
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    /// The stress script found this: eight concurrent readers against one
    /// mutexed connection queued behind each other (p95 seconds). Peers must
    /// run at once and all see the attached store — the first pool re-opened
    /// the same *path*, which built separate instances where `otel` was
    /// missing and every real query binder-errored. A literal-only query
    /// cannot see that; this one must read through otel.main.
    #[test]
    fn concurrent_readers_share_the_attached_store() {
        let db = temp_db("concurrent-readers");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let d = db.clone();
                std::thread::spawn(move || {
                    d.query_rows("SELECT count(*) AS n FROM capped_tables('otel')")
                })
            })
            .collect();
        for h in handles {
            let (cols, rows) = h.join().expect("reader thread").expect("reader query");
            assert_eq!(cols, ["n"]);
            assert_eq!(rows.len(), 1, "one count row");
        }
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    /// An executed-but-empty result is not an error: column names still exist.
    #[test]
    fn empty_result_still_reports_columns() {
        let db = temp_db("empty-result");
        let (cols, rows) = db
            .query_rows_with("SELECT 1 AS n WHERE 1 = 0", &[])
            .expect("empty result must not error");
        assert_eq!(cols, ["n"]);
        assert!(rows.is_empty());
        let _ = fs::remove_dir_all(&db.data_dir);
    }
}
