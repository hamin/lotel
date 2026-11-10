//! Embedded DuckDB: loads the bundled `capped` extension, attaches the store.
//!
//! DuckDB runs in-process via duckdb-rs (statically linked, version-pinned to
//! match the bundled extensions). A single `Connection` is shared behind a
//! mutex; `ATTACH` and `LOAD` are instance-wide so every reader sees them.

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

/// SQL string literal with single quotes escaped.
pub(crate) fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub struct Db {
    conn: Arc<Mutex<Connection>>,
    pub data_dir: PathBuf,
    /// Where the loadable extension files live (data dir or the dev override).
    pub ext_dir: PathBuf,
    pub capped_version: String,
}

impl Clone for Db {
    fn clone(&self) -> Self {
        Self {
            conn: self.conn.clone(),
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
        let capped_version: String = conn.query_row("SELECT capped_version()", [], |r| r.get(0))?;

        let store = cfg.data_dir.join("store");
        conn.execute_batch(&format!(
            "ATTACH {} AS otel (TYPE capped);",
            quote(&store.to_string_lossy())
        ))?;

                Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            data_dir: cfg.data_dir.clone(),
            ext_dir,
            capped_version,
        })
    }

    /// Run a closure against the shared connection. Tolerates mutex poisoning:
    /// a panic in one request must not take the whole UI down.
    pub(crate) fn with_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        f(&conn)
    }

    /// Write the embedded extension files next to the data dir (or use the dev
    /// override directory as-is).
    fn materialize_extensions(cfg: &Config) -> anyhow::Result<PathBuf> {
        if let Some(dir) = &cfg.extension_dir {
            let path = dir.join("capped.duckdb_extension");
            if !path.exists() {
                anyhow::bail!(
                    "LOTEL_EXTENSION_DIR set but no capped.duckdb_extension in {}",
                    dir.display()
                );
            }
            return Ok(dir.clone());
        }
        let dir = cfg.data_dir.join("extensions");
        fs::create_dir_all(&dir)?;
        // Rewrite every boot: cheap (tens of MB) and always in sync with the binary.
        fs::write(dir.join("capped.duckdb_extension"), CAPPED_EXTENSION)?;
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let wrapped = format!("SELECT COLUMNS(*)::VARCHAR FROM ({sql})");

        // duckdb-rs only exposes column metadata after execution: probe once
        // (an empty result is fine — it still executed), then read the values.
        let columns: Vec<String> = {
            let mut probe = conn.prepare(&wrapped)?;
            probe.query_row(params, |_| Ok(())).ok();
            (0..probe.column_count())
                .map(|i| {
                    probe
                        .column_name(i)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|_| format!("col{i}"))
                })
                .collect()
        };

        let mut stmt = conn.prepare(&wrapped)?;
        let rows = stmt
            .query_map(params, |row| {
                (0..columns.len())
                    .map(|i| row.get::<_, Option<String>>(i).map(|v| v.unwrap_or_default()))
                    .collect::<Result<Vec<_>, _>>()
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok((columns, rows))
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
