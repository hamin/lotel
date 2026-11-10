//! Query engine behind the Explore page: a Honeycomb-style builder and a
//! read-only SQL console over the six signal tables.
//!
//! Safety model: the builder never interpolates user text into SQL. Column,
//! measure, operator and bucket tokens are resolved through this module's
//! whitelists; user *values* are always bound parameters. The SQL console is
//! gated by [`is_read_only_sql`] before it ever reaches DuckDB.

use serde::{Deserialize, Serialize};
use serde_json::json;

use topcoat::{
    Result as TcResult,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{BoxView, Unescaped, ViewExt, view},
};

use crate::db::{Db, quote};

/// A queryable column: its stable URL id, a friendly label, its kind, whether
/// it is a JSON attribute bag (needs a key + `json_extract_string`), and the
/// fixed SQL expression that reads it.
#[derive(Clone, Copy, Serialize)]
pub struct ColDef {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub is_attr: bool,
    pub expr: &'static str,
}

#[derive(Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Text,
    Num,
}

#[derive(Clone, Copy, Serialize)]
pub struct Dataset {
    pub id: &'static str,
    pub label: &'static str,
    pub signal: &'static str, // logs | traces | metrics — for grouping the picker
    pub table: &'static str,
    pub time: &'static str,
    pub cols: &'static [ColDef],
}

/// Text columns shared by every signal for quick filtering/grouping.
const fn c(key: &'static str, label: &'static str, kind: Kind, expr: &'static str) -> ColDef {
    ColDef {
        key,
        label,
        kind,
        is_attr: false,
        expr,
    }
}
const fn a(key: &'static str, label: &'static str, expr: &'static str) -> ColDef {
    ColDef {
        key,
        label,
        kind: Kind::Text,
        is_attr: true,
        expr,
    }
}

pub const DATASETS: &[Dataset] = &[
    Dataset {
        id: "logs",
        label: "Logs",
        signal: "logs",
        table: "otlp_logs",
        time: "time_unix_nano",
        cols: &[
            c("service_name", "Service", Kind::Text, "service_name"),
            c("severity_text", "Severity", Kind::Text, "severity_text"),
            c(
                "severity_number",
                "Severity #",
                Kind::Num,
                "severity_number",
            ),
            c("event_name", "Event", Kind::Text, "event_name"),
            c("scope_name", "Scope", Kind::Text, "scope_name"),
            c("body", "Body", Kind::Text, "body"),
            c("trace_id", "Trace id", Kind::Text, "trace_id"),
            a("log_attributes", "Log attribute", "log_attributes"),
            a(
                "resource_attributes",
                "Resource attribute",
                "resource_attributes",
            ),
            a("scope_attributes", "Scope attribute", "scope_attributes"),
        ],
    },
    Dataset {
        id: "traces",
        label: "Spans",
        signal: "traces",
        table: "otlp_traces",
        time: "start_time_unix_nano",
        cols: &[
            c("service_name", "Service", Kind::Text, "service_name"),
            c("name", "Span name", Kind::Text, "name"),
            c(
                "duration_ms",
                "Duration (ms)",
                Kind::Num,
                "duration_time_unix_nano / 1000000.0",
            ),
            c("kind", "Kind", Kind::Text, "CAST(kind AS VARCHAR)"),
            c("status_code", "Status code", Kind::Num, "status_code"),
            c(
                "status_message",
                "Status message",
                Kind::Text,
                "status_status_message",
            ),
            c("trace_id", "Trace id", Kind::Text, "trace_id"),
            a("span_attributes", "Span attribute", "span_attributes"),
            a(
                "resource_attributes",
                "Resource attribute",
                "resource_attributes",
            ),
            a("scope_attributes", "Scope attribute", "scope_attributes"),
        ],
    },
    Dataset {
        id: "gauge",
        label: "Metrics · gauge",
        signal: "metrics",
        table: "otlp_metrics_gauge",
        time: "time_unix_nano",
        cols: &[
            c("name", "Metric", Kind::Text, "\"name\""),
            c("service_name", "Service", Kind::Text, "service_name"),
            c("unit", "Unit", Kind::Text, "unit"),
            c("scope_name", "Scope", Kind::Text, "scope_name"),
            c(
                "value",
                "Value",
                Kind::Num,
                "COALESCE(double_value, CAST(int_value AS DOUBLE))",
            ),
            a("metric_attributes", "Metric attribute", "metric_attributes"),
            a(
                "resource_attributes",
                "Resource attribute",
                "resource_attributes",
            ),
        ],
    },
    Dataset {
        id: "sum",
        label: "Metrics · sum",
        signal: "metrics",
        table: "otlp_metrics_sum",
        time: "time_unix_nano",
        cols: &[
            c("name", "Metric", Kind::Text, "\"name\""),
            c("service_name", "Service", Kind::Text, "service_name"),
            c("unit", "Unit", Kind::Text, "unit"),
            c("scope_name", "Scope", Kind::Text, "scope_name"),
            c(
                "value",
                "Value",
                Kind::Num,
                "COALESCE(double_value, CAST(int_value AS DOUBLE))",
            ),
            a("metric_attributes", "Metric attribute", "metric_attributes"),
            a(
                "resource_attributes",
                "Resource attribute",
                "resource_attributes",
            ),
        ],
    },
    Dataset {
        id: "histogram",
        label: "Metrics · histogram",
        signal: "metrics",
        table: "otlp_metrics_histogram",
        time: "time_unix_nano",
        cols: &[
            c("name", "Metric", Kind::Text, "\"name\""),
            c("service_name", "Service", Kind::Text, "service_name"),
            c("unit", "Unit", Kind::Text, "unit"),
            c("count", "Count", Kind::Num, "\"count\""),
            c("sum", "Sum", Kind::Num, "\"sum\""),
            c("min", "Min", Kind::Num, "\"min\""),
            c("max", "Max", Kind::Num, "\"max\""),
            c("mean", "Mean", Kind::Num, "\"sum\" / NULLIF(\"count\", 0)"),
            a("metric_attributes", "Metric attribute", "metric_attributes"),
            a(
                "resource_attributes",
                "Resource attribute",
                "resource_attributes",
            ),
        ],
    },
    Dataset {
        id: "exp_histogram",
        label: "Metrics · exp histogram",
        signal: "metrics",
        table: "otlp_metrics_exp_histogram",
        time: "time_unix_nano",
        cols: &[
            c("name", "Metric", Kind::Text, "\"name\""),
            c("service_name", "Service", Kind::Text, "service_name"),
            c("unit", "Unit", Kind::Text, "unit"),
            c("count", "Count", Kind::Num, "\"count\""),
            c("sum", "Sum", Kind::Num, "\"sum\""),
            c("zero_count", "Zero count", Kind::Num, "zero_count"),
            c("mean", "Mean", Kind::Num, "\"sum\" / NULLIF(\"count\", 0)"),
            a("metric_attributes", "Metric attribute", "metric_attributes"),
            a(
                "resource_attributes",
                "Resource attribute",
                "resource_attributes",
            ),
        ],
    },
];

pub fn dataset(id: &str) -> Option<&'static Dataset> {
    DATASETS.iter().find(|d| d.id == id)
}

/// Aggregation functions offered by the builder. `needs_col` ones require a
/// column; `needs_num` ones require a numeric column.
pub const MEASURE_FNS: &[(&str, &str, bool, bool)] = &[
    ("count", "Count", false, false),
    ("countd", "Count distinct", true, false),
    ("avg", "Average", true, true),
    ("sum", "Sum", true, true),
    ("min", "Min", true, true),
    ("max", "Max", true, true),
    ("p50", "p50", true, true),
    ("p90", "p90", true, true),
    ("p95", "p95", true, true),
    ("p99", "p99", true, true),
];

/// Filter operators: (token, label).
pub const OPERATORS: &[(&str, &str)] = &[
    ("=", "equals"),
    ("!=", "not equal"),
    ("~", "contains"),
    ("!~", "not contains"),
    (">", "greater than"),
    (">=", "at least"),
    ("<", "less than"),
    ("<=", "at most"),
    ("exists", "is set"),
    ("nexists", "is not set"),
];

/// A single `column OP value` predicate.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Filter {
    pub col: String,
    #[serde(default)]
    pub op: String,
    #[serde(default)]
    pub val: String,
}

/// The whole query, straight from the URL query string. Complex parts travel
/// as JSON in single params so arbitrary values survive round-tripping.
#[derive(Debug, Default, Deserialize)]
pub struct QueryState {
    #[serde(default)]
    pub dataset: String,
    #[serde(default)]
    pub mode: String, // "" | builder | sql
    #[serde(default)]
    pub range: String,
    #[serde(default)]
    pub bucket: String,
    #[serde(default)]
    pub filters: String, // JSON [Filter]
    #[serde(default)]
    pub measures: String, // JSON [String]
    #[serde(default)]
    pub group: String, // JSON [String]
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub sql: String,
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Vec<T> {
    if s.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(s).unwrap_or_default()
    }
}

impl QueryState {
    pub fn filters(&self) -> Vec<Filter> {
        from_json(&self.filters)
    }
    pub fn measures(&self) -> Vec<String> {
        from_json(&self.measures)
    }
    pub fn groups(&self) -> Vec<String> {
        from_json(&self.group)
    }
    pub fn limit(&self) -> usize {
        self.limit.unwrap_or(50).clamp(1, 5000)
    }
    fn is_sql(&self) -> bool {
        self.mode == "sql"
    }
}

/// Lower cutoff timestamp for the selected range, or None for all time.
fn cutoff_for(range: &str) -> Option<String> {
    let secs: Option<i64> = match range {
        "15m" => Some(900),
        "1h" => Some(3600),
        "6h" => Some(6 * 3600),
        "24h" => Some(24 * 3600),
        "7d" => Some(7 * 24 * 3600),
        "30d" => Some(30 * 24 * 3600),
        _ => None,
    };
    secs.map(|s| {
        let now = chrono::Utc::now().timestamp() - s;
        chrono::DateTime::<chrono::Utc>::from_timestamp(now, 0)
            .unwrap()
            .naive_utc()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    })
}

/// Total span covered by the range, in seconds (for auto bucketing).
fn range_secs(range: &str) -> i64 {
    match range {
        "15m" => 900,
        "1h" => 3600,
        "6h" => 6 * 3600,
        "24h" => 24 * 3600,
        "7d" => 7 * 24 * 3600,
        "30d" => 30 * 24 * 3600,
        _ => 24 * 3600,
    }
}

/// Pick a human-friendly bucket that yields ~40 columns, from a fixed safe set
/// so the token is never free-form text. Returns an `INTERVAL` literal.
fn auto_bucket(range: &str) -> String {
    const STEPS: &[(i64, &str)] = &[
        (1, "1 second"),
        (5, "5 seconds"),
        (10, "10 seconds"),
        (30, "30 seconds"),
        (60, "1 minute"),
        (300, "5 minutes"),
        (900, "15 minutes"),
        (1800, "30 minutes"),
        (3600, "1 hour"),
        (10800, "3 hours"),
        (21600, "6 hours"),
        (43200, "12 hours"),
        (86400, "1 day"),
        (604800, "7 days"),
    ];
    let target = (range_secs(range) / 40).max(1);
    let mut pick = STEPS[0];
    for &s in STEPS {
        if s.0 <= target {
            pick = s;
        } else {
            break;
        }
    }
    pick.1.to_string()
}

/// Validate a user-supplied bucket override (`15m`, `2h`, …) against a strict
/// pattern; anything else falls back to the auto bucket.
fn bucket_for(state: &QueryState) -> String {
    let b = state.bucket.trim();
    if !b.is_empty() && b.len() <= 4 && b[..b.len() - 1].chars().all(|ch| ch.is_ascii_digit()) {
        let n = &b[..b.len() - 1];
        let unit = match b.as_bytes()[b.len() - 1] {
            b's' => "second",
            b'm' => "minute",
            b'h' => "hour",
            b'd' => "day",
            _ => "",
        };
        if !unit.is_empty() && !n.is_empty() {
            let plural = if n == "1" { "" } else { "s" };
            return format!("{n} {unit}{plural}");
        }
    }
    auto_bucket(&state.range)
}

/// A column reference resolved to a safe SQL expression + kind.
struct Resolved {
    expr: String,
    kind: Kind,
    label: String,
}

/// Resolve a builder column reference. Plain ids map to a registry column;
/// `@col:key` reads a JSON attribute via `json_extract_string`.
fn resolve_col(ds: &Dataset, col: &str) -> Option<Resolved> {
    if let Some(rest) = col.strip_prefix('@') {
        let (src, key) = rest.split_once(':')?;
        let cd = ds.cols.iter().find(|c| c.key == src && c.is_attr)?;
        let safe_key = key.replace('"', "\\\"");
        return Some(Resolved {
            expr: format!("json_extract_string({}, '$.\"{}\"')", cd.expr, safe_key),
            kind: Kind::Text,
            label: format!("{} · {}", cd.label, key),
        });
    }
    let cd = ds.cols.iter().find(|c| c.key == col && !c.is_attr)?;
    Some(Resolved {
        expr: cd.expr.to_string(),
        kind: cd.kind,
        label: cd.label.to_string(),
    })
}

/// Resolve a measure token (`count`, `avg:value`, `p95:duration_ms`).
fn resolve_measure(ds: &Dataset, m: &str) -> Option<(String, String)> {
    if m == "count" {
        return Some(("count(*)".to_string(), "count".to_string()));
    }
    let (func, col) = m.split_once(':')?;
    let spec = MEASURE_FNS.iter().find(|(f, _, _, _)| *f == func)?;
    let r = resolve_col(ds, col)?;
    if spec.3 && r.kind != Kind::Num {
        return None; // numeric-only aggregate on a non-numeric column
    }
    let expr = match func {
        "countd" => format!("count(DISTINCT {})", r.expr),
        "avg" => format!("avg({})", r.expr),
        "sum" => format!("sum({})", r.expr),
        "min" => format!("min({})", r.expr),
        "max" => format!("max({})", r.expr),
        "p50" | "p90" | "p95" | "p99" => {
            let q = func[1..].parse::<f64>().ok()? / 100.0;
            format!("quantile_cont(CAST({} AS DOUBLE), {})", r.expr, q)
        }
        _ => return None,
    };
    Some((expr, format!("{func}({})", r.label)))
}

/// The compiled plan a page or chart endpoint executes.
pub struct Plan {
    pub error: Option<String>,
    pub count_sql: String,
    pub count_params: Vec<String>,
    pub table_sql: String,
    pub table_params: Vec<String>,
    pub chart_sql: String,
    pub chart_params: Vec<String>,
    pub measure_labels: Vec<String>,
    pub group_keys: Vec<String>,
    pub display: String,
    pub stacked: bool,
}

fn inline(sql: &str, params: &[String]) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut parts = sql.split('?');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    for p in params.iter() {
        out.push_str(&quote(p));
        if let Some(seg) = parts.next() {
            out.push_str(seg);
        }
    }
    out
}

/// Compile a builder query into count/table/chart SQL.
pub fn compile(state: &QueryState) -> Plan {
    let ds = match dataset(&state.dataset) {
        Some(d) => d,
        None => return fail("unknown dataset"),
    };

    // WHERE + shared params.
    let mut conds: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    if let Some(cut) = cutoff_for(&state.range) {
        conds.push(format!("{} >= CAST(? AS TIMESTAMP)", ds.time));
        params.push(cut);
    }
    for f in state.filters() {
        if f.col.is_empty() {
            continue;
        }
        let r = match resolve_col(ds, &f.col) {
            Some(r) => r,
            None => return fail(&format!("unknown column '{}'", f.col)),
        };
        let op = f.op.as_str();
        let e = &r.expr;
        match op {
            "=" | "!=" => {
                let cmp = if op == "=" { "=" } else { "<>" };
                if r.kind == Kind::Num {
                    conds.push(format!("CAST({e} AS DOUBLE) {cmp} CAST(? AS DOUBLE)"));
                } else {
                    conds.push(format!("{e} {cmp} ?"));
                }
                params.push(f.val.clone());
            }
            "~" => conds.push(format!("{e} ILIKE '%' || ? || '%'")),
            "!~" => conds.push(format!("{e} NOT ILIKE '%' || ? || '%'")),
            ">" | ">=" | "<" | "<=" => {
                conds.push(format!("CAST({e} AS DOUBLE) {op} CAST(? AS DOUBLE)"));
                params.push(f.val.clone());
            }
            "exists" => conds.push(format!("({e} IS NOT NULL AND CAST({e} AS VARCHAR) <> '')")),
            "nexists" => conds.push(format!("({e} IS NULL OR CAST({e} AS VARCHAR) = '')")),
            _ => return fail(&format!("unknown operator '{}'", f.op)),
        }
        if matches!(op, "~" | "!~") {
            params.push(f.val.clone());
        }
    }
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };

    // Measures.
    let measures: Vec<String> = if state.measures().is_empty() {
        vec!["count".to_string()]
    } else {
        state.measures()
    };
    let mut mexpr: Vec<String> = Vec::new();
    let mut mlabels: Vec<String> = Vec::new();
    for m in &measures {
        match resolve_measure(ds, m) {
            Some((expr, label)) => {
                mexpr.push(expr);
                mlabels.push(label);
            }
            None => return fail(&format!("invalid measure '{m}'")),
        }
    }

    // Group-by (max 2).
    let group_keys: Vec<String> = state.groups().into_iter().take(2).collect();
    let mut gexpr: Vec<String> = Vec::new();
    for g in &group_keys {
        match resolve_col(ds, g) {
            Some(r) => gexpr.push(r.expr),
            None => return fail(&format!("unknown column '{g}'")),
        }
    }
    let grouped = !gexpr.is_empty();

    let from = format!(" FROM otel.main.\"{}\"", ds.table);
    let bucket = bucket_for(state);
    let xms = format!(
        "epoch_ns(time_bucket(INTERVAL '{bucket}', {})) / 1000000.0",
        ds.time
    );

    // Count of matching rows.
    let count_sql = format!("SELECT count(*){from}{where_sql}");

    // Table: grouped totals, or a time-bucket detail when ungrouped.
    let (table_sql, table_params) = if grouped {
        let sel: Vec<String> = gexpr
            .iter()
            .enumerate()
            .map(|(i, e)| format!("{e} AS g{i}"))
            .chain(
                mexpr
                    .iter()
                    .enumerate()
                    .map(|(i, e)| format!("{e} AS m{i}")),
            )
            .collect();
        let gb: Vec<String> = gexpr.clone();
        let sql = format!(
            "SELECT {}{from}{where_sql} GROUP BY {} ORDER BY m0 DESC LIMIT {}",
            sel.join(", "),
            gb.join(", "),
            state.limit()
        );
        (sql, params.clone())
    } else {
        let sel: Vec<String> = std::iter::once(format!(
            "time_bucket(INTERVAL '{bucket}', {}) AS _t",
            ds.time
        ))
        .chain(
            mexpr
                .iter()
                .enumerate()
                .map(|(i, e)| format!("{e} AS m{i}")),
        )
        .collect();
        let sql = format!(
            "SELECT {}{from}{where_sql} GROUP BY time_bucket(INTERVAL '{bucket}', {}) ORDER BY _t DESC LIMIT {}",
            sel.join(", "),
            ds.time,
            state.limit()
        );
        (sql, params.clone())
    };

    // Chart: time-bucketed. Grouped → (x, group, first measure) for stacked
    // bars; ungrouped → (x, measure…) for one line per measure.
    let (chart_sql, chart_params, stacked) = if grouped {
        let sel = format!("{xms} AS _t, {} AS _g, {} AS _v", gexpr[0], mexpr[0]);
        let sql = format!(
            "SELECT {sel}{from}{where_sql} GROUP BY time_bucket(INTERVAL '{bucket}', {}), {} ORDER BY _t",
            ds.time, gexpr[0]
        );
        (sql, params.clone(), true)
    } else {
        let sel: Vec<String> = std::iter::once(format!("{xms} AS _t"))
            .chain(
                mexpr
                    .iter()
                    .enumerate()
                    .map(|(i, e)| format!("{e} AS m{i}")),
            )
            .collect();
        let sql = format!(
            "SELECT {}{from}{where_sql} GROUP BY time_bucket(INTERVAL '{bucket}', {}) ORDER BY _t",
            sel.join(", "),
            ds.time
        );
        (sql, params.clone(), false)
    };

    let display = inline(&table_sql, &table_params);

    Plan {
        error: None,
        count_sql,
        count_params: params.clone(),
        table_sql,
        table_params,
        chart_sql,
        chart_params,
        measure_labels: mlabels,
        group_keys,
        display,
        stacked,
    }
}

fn fail(msg: &str) -> Plan {
    Plan {
        error: Some(msg.to_string()),
        count_sql: String::new(),
        count_params: vec![],
        table_sql: String::new(),
        table_params: vec![],
        chart_sql: String::new(),
        chart_params: vec![],
        measure_labels: vec![],
        group_keys: vec![],
        display: String::new(),
        stacked: false,
    }
}

/// Read-only guard for the SQL console. Allows a single statement that only
/// reads; rejects anything that can mutate or reach outside the query engine.
pub fn is_read_only_sql(sql: &str) -> Result<(), String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err("empty query".to_string());
    }
    // A statement terminator inside the body means multiple statements.
    if trimmed.contains(';') {
        return Err("only a single statement is allowed".to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    let first = lower.split_whitespace().next().unwrap_or("");
    const OK: &[&str] = &[
        "select", "with", "from", "values", "describe", "desc", "show", "explain",
    ];
    if !OK.contains(&first) {
        return Err(format!(
            "only read-only queries are allowed (got '{first}')"
        ));
    }
    const BAD: &[&str] = &[
        "insert",
        "update",
        "delete",
        "drop",
        "create",
        "alter",
        "attach",
        "detach",
        "install",
        "load",
        "copy",
        "export",
        "import",
        "truncate",
        "vacuum",
        "checkpoint",
        "set",
        "reset",
        "call",
        "pragma",
        "begin",
        "commit",
        "rollback",
        "grant",
        "revoke",
    ];
    for w in lower.split(|ch: char| !ch.is_ascii_alphabetic()) {
        if BAD.contains(&w) {
            return Err(format!("statement contains disallowed keyword '{w}'"));
        }
    }
    Ok(())
}

/// Compile the SQL console query. Returns a generic (untimed) plan whose table
/// is the raw statement.
pub fn compile_sql(state: &QueryState) -> Plan {
    if let Err(e) = is_read_only_sql(&state.sql) {
        return fail(&e);
    }
    Plan {
        error: None,
        count_sql: String::new(),
        count_params: vec![],
        table_sql: state.sql.trim().trim_end_matches(';').to_string(),
        table_params: vec![],
        chart_sql: String::new(),
        chart_params: vec![],
        measure_labels: vec![],
        group_keys: vec![],
        display: state.sql.trim().to_string(),
        stacked: false,
    }
}

/// Run the plan's table + count and hand back rendered rows.
pub struct Outcome {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub total: String,
    pub error: Option<String>,
}

fn bound(p: &[String]) -> Vec<&dyn duckdb::ToSql> {
    p.iter().map(|s| s as &dyn duckdb::ToSql).collect()
}

pub fn run(db: &Db, plan: &Plan) -> Outcome {
    if let Some(e) = &plan.error {
        return Outcome {
            columns: vec![],
            rows: vec![],
            total: "0".into(),
            error: Some(e.clone()),
        };
    }
    match db.query_rows_with(&plan.table_sql, &bound(&plan.table_params)) {
        Ok((columns, rows)) => {
            let total = if plan.count_sql.is_empty() {
                rows.len().to_string()
            } else {
                db.query_rows_with(&plan.count_sql, &bound(&plan.count_params))
                    .ok()
                    .and_then(|(_, r)| r.into_iter().next())
                    .and_then(|r| r.into_iter().next())
                    .unwrap_or_else(|| "?".into())
            };
            Outcome {
                columns,
                rows,
                total,
                error: None,
            }
        }
        Err(e) => Outcome {
            columns: vec![],
            rows: vec![],
            total: "0".into(),
            error: Some(e.to_string()),
        },
    }
}

/// Vendored charting library, shipped in the binary (same copy as metrics).
const UPLOT_JS: &str = include_str!("../assets/vendor/uPlot.min.js");
const UPLOT_CSS: &str = include_str!("../assets/vendor/uPlot.min.css");

/// Series palette for breakdown colors and lines.
pub(crate) const PALETTE: [&str; 10] = [
    "#4f8cc9", "#57b98a", "#c9a04f", "#b07fc9", "#c97f7f", "#6fb3b8", "#e6c068", "#7ea6d9",
    "#f0858a", "#9a7fd0",
];

/// The Explore page: builder + SQL console, sharing one result surface.
#[page]
async fn query(cx: &Cx) -> TcResult<BoxView<'_>> {
    let db = context::<Db>(cx);
    let s = parse_query_params::<QueryState>(cx).unwrap_or_default();
    let sql_mode = s.is_sql();

    let plan = if sql_mode {
        compile_sql(&s)
    } else {
        compile(&s)
    };
    let res = run(db, &plan);
    let measures_text = plan.measure_labels.join(" · ");
    let group_text = plan.group_keys.join(", ");

    // State handed to the client builder so its controls mirror the URL.
    let seed = json!({
        "datasets": DATASETS,
        "operators": OPERATORS,
        "measureFns": MEASURE_FNS.iter().map(|(f,l,nc,nn)| json!({"fn":f,"label":l,"col":nc,"num":nn})).collect::<Vec<_>>(),
        "palette": PALETTE,
        "state": {
            "dataset": if s.dataset.is_empty() { "logs" } else { &s.dataset },
            "mode": if sql_mode { "sql" } else { "builder" },
            "range": if s.range.is_empty() { "24h" } else { &s.range },
            "bucket": s.bucket,
            "measures": s.measures(),
            "filters": s.filters(),
            "group": s.groups(),
            "sql": s.sql,
        },
    });

    Ok(view! {
        <h1>"Explore"</h1>
        <p class="sub">
            "Query the store as a guided builder or raw SQL. Filters, computations, and a breakdown over any column — attributes included."
        </p>

        <div class="qtabs">
            <a class=(if !sql_mode { "tab on" } else { "tab" }) href="/query?dataset=logs&range=24h">"Query builder"</a>
            <a class=(if sql_mode { "tab on" } else { "tab" }) href="/query?mode=sql&sql=SELECT%20*%20FROM%20otel.main.otlp_logs%20LIMIT%20100">"SQL console"</a>
        </div>

        if sql_mode {
            <form class="sqlform" method="get" action="/query">
                <input type="hidden" name="mode" value="sql" />
                <textarea name="sql" class="sqlbox" rows="6" spellcheck="false">(s.sql.clone())</textarea>
                <div class="sqlbar">
                    <button type="submit">"Run"</button>
                    <span class="muted">"read-only · single statement"</span>
                </div>
            </form>
        } else {
            <div class="qb" id="qb"></div>
        }

        if let Some(e) = &res.error {
            <div class="qerr"><b>"Query error"</b><pre>(e.clone())</pre></div>
        }

        if res.error.is_none() {
            <div class="qstats">
                <span class="qs"><b>(res.total.clone())</b>" rows"</span>
                if !plan.group_keys.is_empty() {
                    <span class="qs">"by "<b>(group_text.clone())</b></span>
                }
                if !plan.measure_labels.is_empty() {
                    <span class="qs">(measures_text.clone())</span>
                }
            </div>

            <div class="card"><div id="qchart" class="qchart"></div></div>

            <div class="qtablewrap">
                <table class="qtable">
                    <thead>
                        <tr>
                            for col in &res.columns {
                                <th>(col.clone())</th>
                            }
                        </tr>
                    </thead>
                    <tbody>
                        for row in &res.rows {
                            <tr>
                                for cell in row {
                                    <td>(cell.clone())</td>
                                }
                            </tr>
                        }
                    </tbody>
                </table>
                if res.rows.is_empty() {
                    <p class="muted qempty">"No rows match. Widen the time range or loosen a filter."</p>
                }
            </div>

            if !plan.display.is_empty() {
                <details class="qsql">
                    <summary>"Generated SQL"</summary>
                    <pre>(plan.display.clone())</pre>
                </details>
            }
        }

        <script>(Unescaped::new_unchecked(format!("window.LOTQL = {seed};")))
        </script>
        <style>(Unescaped::new_unchecked(UPLOT_CSS))</style>
        <script>(Unescaped::new_unchecked(UPLOT_JS))</script>
        <script>(Unescaped::new_unchecked(QBLD_JS))</script>
    }
    .boxed())
}

/// Client-side builder: renders controls from the injected registry and, on
/// Run, composes the query string and reloads. Results themselves are rendered
/// server-side. The chart pulls `/api/query/chart`.
const QBLD_JS: &str = r#"
(function () {
    var L = window.LOTQL; if (!L) return;

    // ---- builder form ----
    var qb = document.getElementById('qb');
    if (qb) renderForm();

    function ds(id) { for (var i=0;i<L.datasets.length;i++) if (L.datasets[i].id===id) return L.datasets[i]; return L.datasets[0]; }

    function renderForm() {
        var st = L.state;
        var d = ds(st.dataset);
        qb.innerHTML = '';
        qb.appendChild(row([
            pick('Dataset', L.datasets.map(function(x){return [x.id,x.label];}), st.dataset, 'dataset'),
            pick('Time range', [['15m','15 min'],['1h','1 hour'],['6h','6 hours'],['24h','24 hours'],['7d','7 days'],['30d','30 days'],['all','all']], st.range, 'range'),
            txt('Bucket', st.bucket, 'bucket', 'auto')
        ]));

        var ms = document.createElement('div'); ms.className='qsec';
        ms.innerHTML='<span class="qlab">Computation</span>';
        (st.measures.length?st.measures:['count']).forEach(function(m){ ms.appendChild(measureRow(d,m)); });
        var addM=document.createElement('button'); addM.className='qadd'; addM.textContent='+ add computation';
        addM.onclick=function(){ ms.insertBefore(measureRow(d,'count'), addM); };
        ms.appendChild(addM); qb.appendChild(ms);

        var fl = document.createElement('div'); fl.className='qsec';
        fl.innerHTML='<span class="qlab">Filters</span>';
        st.filters.forEach(function(f){ fl.appendChild(filterRow(d,f)); });
        var addF=document.createElement('button'); addF.className='qadd'; addF.textContent='+ add filter';
        addF.onclick=function(){ fl.insertBefore(filterRow(d,{col:'service_name',op:'=',val:''}), addF); };
        fl.appendChild(addF); qb.appendChild(fl);

        var gb = document.createElement('div'); gb.className='qsec';
        gb.innerHTML='<span class="qlab">Breakdown</span>';
        st.group.forEach(function(g){ gb.appendChild(groupRow(d,g)); });
        var addG=document.createElement('button'); addG.className='qadd'; addG.textContent='+ add breakdown';
        addG.onclick=function(){ gb.insertBefore(groupRow(d,d.cols[0].key), addG); };
        gb.appendChild(addG); qb.appendChild(gb);

        var run=document.createElement('button'); run.className='qrun'; run.textContent='Run query';
        run.onclick=submit; qb.appendChild(run);
    }

    function row(children){ var d=document.createElement('div'); d.className='qrow'; children.forEach(function(c){d.appendChild(c);}); return d; }
    function label(t){ var s=document.createElement('span'); s.className='qcap'; s.textContent=t; return s; }
    function field(wrap){ var l=document.createElement('label'); l.className='qfield'; return l; }

    function pick(cap, opts, val, name) {
        var w=document.createElement('div'); w.className='qfield'; w.appendChild(label(cap));
        var s=document.createElement('select'); s.dataset.name=name;
        opts.forEach(function(o){ var e=document.createElement('option'); e.value=o[0]; e.textContent=o[1]; if(o[0]===val)e.selected=true; s.appendChild(e); });
        s.onchange=function(){ if(name==='dataset'){ L.state.dataset=s.value; L.state.measures=[]; L.state.filters=[]; L.state.group=[]; renderForm(); } };
        w.appendChild(s); return w;
    }
    function txt(cap, val, name, ph) {
        var w=document.createElement('div'); w.className='qfield'; w.appendChild(label(cap));
        var i=document.createElement('input'); i.value=val||''; i.placeholder=ph||''; i.dataset.name=name;
        w.appendChild(i); return w;
    }

    function colOptions(d){ var o=[]; d.cols.forEach(function(c){ if(!c.is_attr) o.push([c.key,c.label]); o.push(['@'+c.key, c.label+(c.is_attr?' (attr)':'')]); }); return o; }

    function colSelect(d, cur) {
        var s=document.createElement('select'); s.className='qcol';
        colOptions(d).forEach(function(o){ var e=document.createElement('option'); e.value=o[0]; e.textContent=o[1]; if(o[0]===cur)e.selected=true; s.appendChild(e); });
        return s;
    }

    function measureRow(d, m) {
        var r=document.createElement('div'); r.className='qtoken';
        var fn=document.createElement('select'); fn.className='qfn';
        L.measureFns.forEach(function(x){ var e=document.createElement('option'); e.value=x.fn; e.textContent=x.label; fn.appendChild(e); });
        var parts=(m||'count').split(':'); var f0=parts[0], c0=parts[1]||''; fn.value=f0;
        var col=document.createElement('select'); col.className='qcol';
        var render=function(){ var need=L.measureFns.find(function(x){return x.fn===fn.value;}); col.style.display=need.col?'':'none';
            col.innerHTML=''; if(need.num){ d.cols.forEach(function(c){ if(c.kind==='num'){var e=document.createElement('option');e.value=c.key;e.textContent=c.label; if(c.key===c0)e.selected=true; col.appendChild(e);} }); }
            else { d.cols.forEach(function(c){ if(!c.is_attr){var e=document.createElement('option');e.value=c.key;e.textContent=c.label; if(c.key===c0)e.selected=true; col.appendChild(e);} }); }
        };
        fn.onchange=render; render();
        var x=document.createElement('button'); x.className='qdel'; x.textContent='×'; x.onclick=function(){ r.remove(); };
        r.appendChild(fn); r.appendChild(col); r.appendChild(x); r._kind='measure'; return r;
    }

    function filterRow(d, f) {
        var r=document.createElement('div'); r.className='qtoken';
        var col=colSelect(d, f.col||'service_name');
        var key=document.createElement('input'); key.className='qkey'; key.placeholder='attribute key';
        var op=document.createElement('select'); op.className='qop';
        L.operators.forEach(function(o){ var e=document.createElement('option'); e.value=o[0]; e.textContent=o[1]; op.appendChild(e); });
        op.value=f.op||'=';
        var val=document.createElement('input'); val.className='qval'; val.value=f.val||'';
        var sync=function(){ key.style.display = col.value[0]==='@' ? '' : 'none'; val.placeholder=(op.value==='exists'||op.value==='nexists')?'(no value)':'value'; };
        col.onchange=sync; op.onchange=sync; sync();
        var x=document.createElement('button'); x.className='qdel'; x.textContent='×'; x.onclick=function(){ r.remove(); };
        r.appendChild(col); r.appendChild(key); r.appendChild(op); r.appendChild(val); r.appendChild(x); r._kind='filter'; return r;
    }

    function groupRow(d, g) {
        var r=document.createElement('div'); r.className='qtoken';
        var col=colSelect(d, g||d.cols[0].key);
        var x=document.createElement('button'); x.className='qdel'; x.textContent='×'; x.onclick=function(){ r.remove(); };
        r.appendChild(col); r.appendChild(x); r._kind='group'; return r;
    }

    function colref(token) {
        // token may be '@source' chosen from the attr list; key from sibling input
        return token;
    }

    function submit() {
        var st = { dataset:'logs', range:'24h' };
        qb.querySelectorAll('select[data-name],input[data-name]').forEach(function(el){ st[el.dataset.name]=el.value; });
        var measures=[]; qb.querySelectorAll('.qtoken').forEach(function(t){ if(t._kind==='measure'){ var fn=t.querySelector('.qfn').value; var col=t.querySelector('.qcol'); var m = fn==='count'?'count':fn+':'+col.value; if(measures.indexOf(m)<0) measures.push(m);} });
        var filters=[]; qb.querySelectorAll('.qtoken').forEach(function(t){ if(t._kind==='filter'){ var col=t.querySelector('.qcol').value; var op=t.querySelector('.qop').value; var val=t.querySelector('.qval').value; var key=t.querySelector('.qkey').value; var cref = col[0]==='@' ? (col+':'+key) : col; if(cref) filters.push({col:cref,op:op,val:val}); } });
        var group=[]; qb.querySelectorAll('.qtoken').forEach(function(t){ if(t._kind==='group'){ var v=t.querySelector('.qcol').value; var key=t.querySelector('.qkey'); var cref = v[0]==='@' ? (v+':'+(key?key.value:'')) : v; if(group.indexOf(cref)<0 && cref) group.push(cref);} });
        var q = '?dataset='+encodeURIComponent(st.dataset)+'&range='+encodeURIComponent(st.range||'24h');
        if (st.bucket) q+='&bucket='+encodeURIComponent(st.bucket);
        q+='&measures='+encodeURIComponent(JSON.stringify(measures));
        q+='&filters='+encodeURIComponent(JSON.stringify(filters));
        q+='&group='+encodeURIComponent(JSON.stringify(group));
        window.location.search=q;
    }

    // ---- results chart ----
    var chartEl = document.getElementById('qchart');
    if (chartEl && window.uPlot) {
        fetch('/api/query/chart' + window.location.search)
            .then(function(r){return r.json();})
            .then(function(d){
                if (d.error) { chartEl.innerHTML='<span class="muted">'+d.error+'</span>'; return; }
                if (!d.xs || !d.xs.length) { chartEl.innerHTML='<span class="muted">no data to chart</span>'; return; }
                var data=[d.xs].concat(d.series.map(function(s){return s.data;}));
                var series=[{label:d.time?'time':''}].concat(d.series.map(function(s){ return d.stacked
                    ? {label:s.label, stroke:s.color, fill:s.color+'33', width:0, type:'bars', stacked:true}
                    : {label:s.label, stroke:s.color, fill:s.color+'1f', width:1.5}; }));
                new uPlot({
                    width: Math.max(chartEl.clientWidth,320), height:260, padding:[6,6,0,0],
                    scales:{ x:{ time: !!d.time } },
                    series: series,
                    axes:[ {stroke:'#8a93a5',grid:{stroke:'#1b212e'}}, {stroke:'#8a93a5',grid:{stroke:'#1b212e'}} ],
                    cursor:{points:{show:d.stacked?false:true}}
                }, data, chartEl);
            })
            .catch(function(e){ chartEl.innerHTML='<span class="muted">chart error</span>'; });
    }
})();
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn state(dataset: &str, filters: &str, measures: &str, group: &str) -> QueryState {
        QueryState {
            dataset: dataset.into(),
            range: "24h".into(),
            filters: filters.into(),
            measures: measures.into(),
            group: group.into(),
            ..Default::default()
        }
    }

    #[test]
    fn builder_whitelists_columns() {
        let s = state(
            "logs",
            r#"[{"col":"service_name","op":"=","val":"x"}]"#,
            "",
            "",
        );
        let p = compile(&s);
        assert_eq!(p.error, None);
        assert!(p.table_sql.contains("service_name = ?"));
        assert_eq!(p.table_params.last().unwrap(), "x");
    }

    #[test]
    fn unknown_column_is_refused() {
        let s = state("logs", r#"[{"col":"secret","op":"=","val":"1"}]"#, "", "");
        assert!(compile(&s).error.is_some());
    }

    #[test]
    fn attribute_uses_json_extract() {
        let s = state(
            "logs",
            r#"[{"col":"@log_attributes:http.method","op":"=","val":"POST"}]"#,
            "",
            "",
        );
        let p = compile(&s);
        assert_eq!(p.error, None);
        assert!(
            p.table_sql
                .contains("json_extract_string(log_attributes, '$.\"http.method\"')")
        );
    }

    #[test]
    fn numeric_measure_is_percentile() {
        let s = state("traces", "", r#"["p95:duration_ms"]"#, "");
        let p = compile(&s);
        assert!(
            p.table_sql.contains(
                "quantile_cont(CAST(duration_time_unix_nano / 1000000.0 AS DOUBLE), 0.95)"
            )
        );
        assert_eq!(p.measure_labels, vec!["p95(Duration (ms))".to_string()]);
    }

    #[test]
    fn numeric_measure_on_text_is_refused() {
        let s = state("logs", "", r#"["avg:body"]"#, "");
        assert!(compile(&s).error.is_some());
    }

    #[test]
    fn grouped_builds_breakdown_totals() {
        let s = state("logs", "", r#"["count"]"#, r#"["service_name"]"#);
        let p = compile(&s);
        assert!(p.stacked);
        assert!(p.table_sql.contains("GROUP BY service_name"));
        assert!(p.table_sql.contains("ORDER BY m0 DESC"));
    }

    #[test]
    fn params_are_never_inlined_in_sql() {
        let evil = r#"[{"col":"body","op":"=","val":"a'); DROP TABLE otel.main.otlp_logs;--"}]"#;
        let s = state("logs", evil, "", "");
        let p = compile(&s);
        assert!(!p.table_sql.contains("DROP"));
        assert!(p.table_sql.contains("body = ?"));
        // but the human-readable display shows the literal
        assert!(p.display.contains("DROP"));
    }

    #[test]
    fn bucket_override_is_validated() {
        let mut s = state("logs", "", "", "");
        s.bucket = "15m".into();
        assert_eq!(bucket_for(&s), "15 minutes");
        s.bucket = "1h".into();
        assert_eq!(bucket_for(&s), "1 hour");
        s.bucket = "5; DROP".into();
        assert_eq!(bucket_for(&s), auto_bucket("24h")); // rejected → auto
    }

    #[test]
    fn read_only_guard() {
        assert!(is_read_only_sql("SELECT 1").is_ok());
        assert!(is_read_only_sql("  select * from x ; ").is_ok());
        assert!(is_read_only_sql("WITH a AS (SELECT 1) SELECT * FROM a").is_ok());
        assert!(is_read_only_sql("DELETE FROM otel.main.otlp_logs").is_err());
        assert!(is_read_only_sql("SELECT 1; DROP TABLE t").is_err());
        assert!(is_read_only_sql("SELECT * FROM x UNION SELECT * FROM y; ").is_ok());
        assert!(is_read_only_sql("SELECT install_extension('x')").is_err());
        assert!(is_read_only_sql("").is_err());
    }
}
