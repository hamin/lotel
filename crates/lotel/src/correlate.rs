//! Log ↔ trace correlation: the two lookups that decide whether a link
//! between the logs and traces pages leads somewhere.
//!
//! A log row's `trace_id` only earns a "trace" link when that trace is still
//! in the store (traces age out after 3d, logs after 7d, and plenty of
//! senders stamp ids for traces they never export). A trace or span earns a
//! "N logs" link only when logs carrying its id would show on `/logs` —
//! rows with a NULL time (Codex sends `timeUnixNano: 0`) don't, so they
//! aren't counted.

use std::collections::HashSet;

use crate::{db::Db, filters::Bar};

/// The subset of `ids` that have at least one stored span. One query per
/// page, however many rows link out; an error reads as "none stored", so a
/// broken lookup hides links rather than offering dead ones.
pub(crate) fn stored_traces(db: &Db, ids: &[&str]) -> HashSet<String> {
    let mut uniq: Vec<&str> = ids.iter().copied().filter(|s| !s.is_empty()).collect();
    uniq.sort_unstable();
    uniq.dedup();
    if uniq.is_empty() {
        return HashSet::new();
    }
    let marks = vec!["?"; uniq.len()].join(", ");
    let sql = format!(
        "SELECT DISTINCT trace_id FROM otel.main.otlp_traces WHERE trace_id IN ({marks})"
    );
    let params: Vec<&dyn duckdb::ToSql> = uniq.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    db.query_rows_with(&sql, &params)
        .map(|(_, rows)| rows.into_iter().map(|r| r[0].clone()).collect())
        .unwrap_or_default()
}

/// How many listable logs carry `trace`, and how many of those carry `span`
/// too (0 when `span` is empty).
pub(crate) fn log_counts(db: &Db, trace: &str, span: &str) -> (u64, u64) {
    if trace.is_empty() {
        return (0, 0);
    }
    let sql = "SELECT count(*), count(*) FILTER (WHERE ? <> '' AND span_id = ?)
               FROM otel.main.otlp_logs
               WHERE trace_id = ? AND time_unix_nano IS NOT NULL";
    let params: Vec<&dyn duckdb::ToSql> = vec![&span, &span, &trace];
    db.query_rows_with(sql, &params)
        .ok()
        .and_then(|(_, rows)| rows.into_iter().next())
        .map(|r| (r[0].parse().unwrap_or(0), r[1].parse().unwrap_or(0)))
        .unwrap_or((0, 0))
}

/// `/logs` narrowed to a trace (and span), over all time: the logs of a
/// trace are wherever they are, not in the default window.
pub(crate) fn logs_href(trace: &str, span: &str) -> String {
    let mut h = format!("/logs?range=all&trace={}", Bar::enc(trace));
    if !span.is_empty() {
        h.push_str(&format!("&span={}", Bar::enc(span)));
    }
    h
}

/// "1 log" / "12 logs".
pub(crate) fn logs_label(n: u64) -> String {
    if n == 1 { "1 log".to_string() } else { format!("{n} logs") }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use crate::logs::db_tests::temp_db;

    fn seed(db: &Db) {
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE otel.main.otlp_traces(start_time_unix_nano TIMESTAMP, trace_id VARCHAR, span_id VARCHAR)
                   WITH (time_col = 'start_time_unix_nano', ttl = '3 days', max_bytes = '64MB');
                 INSERT INTO otel.main.otlp_traces VALUES (now()::TIMESTAMP, 't1', 's1');
                 CREATE TABLE otel.main.otlp_logs(time_unix_nano TIMESTAMP, trace_id VARCHAR, span_id VARCHAR, body VARCHAR)
                   WITH (time_col = 'time_unix_nano', ttl = '7 days', max_bytes = '64MB');
                 INSERT INTO otel.main.otlp_logs VALUES
                   (now()::TIMESTAMP, 't1', 's1', 'a'),
                   (now()::TIMESTAMP, 't1', 's2', 'b'),
                   (now()::TIMESTAMP, 't1', 's1', 'c'),
                   (NULL, 't1', 's1', 'no time: not listable'),
                   (now()::TIMESTAMP, 't9', 's9', 'trace never stored');",
            )?;
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn only_stored_traces_link() {
        let db = temp_db("correlate-stored");
        seed(&db);
        let got = stored_traces(&db, &["t1", "t9", "", "t1"]);
        assert_eq!(got, HashSet::from(["t1".to_string()]));
        assert!(stored_traces(&db, &[]).is_empty());
        let _ = std::fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn log_counts_follow_what_the_logs_page_lists() {
        let db = temp_db("correlate-counts");
        seed(&db);
        assert_eq!(log_counts(&db, "t1", ""), (3, 0), "NULL-time rows aren't counted");
        assert_eq!(log_counts(&db, "t1", "s1"), (3, 2));
        assert_eq!(log_counts(&db, "nope", "s1"), (0, 0));
        assert_eq!(log_counts(&db, "", ""), (0, 0));
        let _ = std::fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn links_and_labels() {
        assert_eq!(logs_href("t1", ""), "/logs?range=all&trace=t1");
        assert_eq!(logs_href("t1", "s1"), "/logs?range=all&trace=t1&span=s1");
        assert_eq!((logs_label(1).as_str(), logs_label(4).as_str()), ("1 log", "4 logs"));
    }
}
