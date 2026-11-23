//! Registers the DuckDB extensions linked into the binary, when DuckDB comes
//! from its prebuilt static libraries rather than a source build.
//!
//! A source build (`bundled-duckdb`) compiles core_functions, json and parquet
//! in and registers them itself. DuckDB's prebuilt `libduckdb_static.a` does
//! not: the application calls `duckdb_register_static_extension` with each
//! linked extension's describe function before it opens a database. That
//! includes `loadable_extensions`, the piece that lets `LOAD` read an
//! extension file (capped, otlp, fts); without it every LOAD fails with
//! "this build does not link the loadable_extensions library".

#[cfg(lotel_prebuilt_duckdb)]
mod prebuilt {
    use std::ffi::c_void;
    use std::sync::OnceLock;

    type Describe = unsafe extern "C" fn(*mut c_void) -> i32;

    unsafe extern "C" {
        fn duckdb_register_static_extension(describe: Describe) -> i32;
        fn duckdb_extension_loadable_extensions_describe(d: *mut c_void) -> i32;
        fn duckdb_extension_core_functions_describe(d: *mut c_void) -> i32;
        fn duckdb_extension_json_describe(d: *mut c_void) -> i32;
        fn duckdb_extension_parquet_describe(d: *mut c_void) -> i32;
    }

    pub fn register() -> Result<(), String> {
        static DONE: OnceLock<Result<(), String>> = OnceLock::new();
        DONE.get_or_init(|| {
            let all: [(&str, Describe); 4] = [
                ("loadable_extensions", duckdb_extension_loadable_extensions_describe),
                ("core_functions", duckdb_extension_core_functions_describe),
                ("json", duckdb_extension_json_describe),
                ("parquet", duckdb_extension_parquet_describe),
            ];
            for (name, describe) in all {
                // SAFETY: each describe function has the signature DuckDB's
                // duckdb_static_extension.h declares, and registration runs
                // once, before any database is opened.
                let rc = unsafe { duckdb_register_static_extension(describe) };
                if rc != 0 {
                    return Err(format!("registering DuckDB's {name} extension failed ({rc})"));
                }
            }
            Ok(())
        })
        .clone()
    }
}

/// Call before opening any DuckDB database. A no-op for source builds.
pub fn register_linked_extensions() -> anyhow::Result<()> {
    #[cfg(lotel_prebuilt_duckdb)]
    prebuilt::register().map_err(anyhow::Error::msg)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use duckdb::Connection;

    /// Both ways of building DuckDB must give lotel the same engine: the same
    /// extensions compiled in (and nothing extra, e.g. ICU would change how
    /// timestamps with time zones behave), and LOAD of extension files working.
    #[test]
    fn linked_extensions_match_a_source_build() {
        super::register_linked_extensions().unwrap();
        let config = duckdb::Config::default()
            .with("autoinstall_known_extensions", "false")
            .unwrap()
            .with("autoload_known_extensions", "false")
            .unwrap();
        let db = Connection::open_in_memory_with_flags(config).unwrap();
        let mut st = db
            .prepare(
                "SELECT extension_name FROM duckdb_extensions() \
                 WHERE install_mode = 'STATICALLY_LINKED' ORDER BY 1",
            )
            .unwrap();
        let linked: Vec<String> = st.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
        assert_eq!(linked, ["core_functions", "json", "parquet"], "{linked:?}");
        for ext in ["core_functions", "json", "parquet"] {
            db.execute_batch(&format!("LOAD {ext};")).unwrap();
        }
        let n: i64 = db.query_row("SELECT json_array_length('[1,2,3]')", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
        // An extension file that does not exist: the error must be about the
        // file, not "this build does not link the loadable_extensions library".
        let err = db.execute_batch("LOAD '/nonexistent/x.duckdb_extension';").unwrap_err().to_string();
        assert!(!err.contains("loadable_extensions"), "{err}");
    }
}
