//! Query engine behind the Explore page: a visual builder and a
//! read-only SQL console over the six signal tables.
//!
//! Safety model: the builder never interpolates user text into SQL. Column,
//! measure, operator and bucket tokens are resolved through this module's
//! whitelists; user *values* and attribute JSON paths are always bound
//! parameters (DuckDB accepts the `json_extract` path as a `?`). The SQL
//! console is gated by [`is_read_only_sql`] before it ever reaches DuckDB.

use serde::{Deserialize, Serialize};
use serde_json::json;

use topcoat::{
    Result as TcResult,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{BoxView, Unescaped, ViewExt, view},
};

use crate::config::Config;
use crate::db::{Db, quote};

/// SQL divisor turning epoch nanoseconds into the epoch SECONDS (fractional,
/// kept as f64) that uPlot's `time` x-scale expects. Shared by both chart
/// endpoints ([`crate::query`]'s builder and `/api/metrics/series`) so the
/// unit can never drift between them again.
pub(crate) const NS_TO_SECS: &str = "1000000000.0";

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
    /// Included in the facet sidebar (false for high-cardinality id columns).
    pub facet: bool,
}

impl ColDef {
    /// Opt out of faceting (ids, free-text message bodies, etc.).
    const fn nf(mut self) -> Self {
        self.facet = false;
        self
    }
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
    /// Column keys shown, in order, as columns in the raw-events table.
    pub display: &'static [&'static str],
}

/// Text columns shared by every signal for quick filtering/grouping.
const fn c(key: &'static str, label: &'static str, kind: Kind, expr: &'static str) -> ColDef {
    ColDef {
        key,
        label,
        kind,
        is_attr: false,
        expr,
        facet: true,
    }
}
const fn a(key: &'static str, label: &'static str, expr: &'static str) -> ColDef {
    ColDef {
        key,
        label,
        kind: Kind::Text,
        is_attr: true,
        expr,
        facet: false,
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
            c("trace_id", "Trace id", Kind::Text, "trace_id").nf(),
            a("log_attributes", "Log attribute", "log_attributes"),
            a(
                "resource_attributes",
                "Resource attribute",
                "resource_attributes",
            ),
            a("scope_attributes", "Scope attribute", "scope_attributes"),
        ],
        display: &["service_name", "severity_text", "body", "trace_id"],
    },
    Dataset {
        id: "traces",
        label: "Traces",
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
            c("trace_id", "Trace id", Kind::Text, "trace_id").nf(),
            a("span_attributes", "Span attribute", "span_attributes"),
            a(
                "resource_attributes",
                "Resource attribute",
                "resource_attributes",
            ),
            a("scope_attributes", "Scope attribute", "scope_attributes"),
        ],
        display: &["service_name", "name", "duration_ms", "kind", "trace_id"],
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
        display: &["name", "service_name", "value", "unit"],
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
        display: &["name", "service_name", "value", "unit"],
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
        display: &["name", "service_name", "count", "sum", "mean"],
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
        display: &["name", "service_name", "count", "sum", "zero_count"],
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
    // a visit with no dataset (a fresh /query, or the chart fragment's empty
    // search) means the builder's visible default: Logs
    #[serde(default = "default_dataset")]
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
    #[serde(default)]
    pub tab: String, // series | breakdown | events (empty = auto)
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Vec<T> {
    if s.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(s).unwrap_or_default()
    }
}

fn default_dataset() -> String {
    "logs".into()
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

/// A column reference resolved to a safe SQL expression + kind. An attribute
/// read carries one `?` for its JSON path; [`Resolved::path`] must be bound
/// once for every occurrence of that expression in the statement.
struct Resolved {
    expr: String,
    /// JSON path bound to the `?` in `expr` (Some only for `@col:key` reads).
    path: Option<String>,
    kind: Kind,
    label: String,
}

/// Bind a column's JSON path once, for one occurrence of `r.expr` in the SQL
/// being built. No-op for plain columns.
fn push_path(params: &mut Vec<String>, r: &Resolved) {
    if let Some(p) = &r.path {
        params.push(p.clone());
    }
}

/// Bound parameters for a list of expressions, one `?` (a JSON path) each,
/// in order of appearance.
fn paths(list: &[Option<String>]) -> Vec<String> {
    list.iter().flatten().cloned().collect()
}

/// Resolve a builder column reference. Plain ids map to a registry column;
/// `@col:key` reads a JSON attribute via `json_extract_string` with the key
/// carried as a bound path parameter — it never enters the SQL text, so no
/// quoting in a key can break out of a string literal.
fn resolve_col(ds: &Dataset, col: &str) -> Option<Resolved> {
    if let Some(rest) = col.strip_prefix('@') {
        let (src, key) = rest.split_once(':')?;
        let cd = ds.cols.iter().find(|c| c.key == src && c.is_attr)?;
        // The `\"` escaping is DuckDB's JSON-path string syntax (the path is
        // data here, not SQL), kept so keys with a quote still address the
        // right member once bound.
        return Some(Resolved {
            expr: format!("json_extract_string({}, ?)", cd.expr),
            path: Some(format!("$.\"{}\"", key.replace('"', "\\\""))),
            kind: Kind::Text,
            label: format!("{} · {}", cd.label, key),
        });
    }
    let cd = ds.cols.iter().find(|c| c.key == col && !c.is_attr)?;
    Some(Resolved {
        expr: cd.expr.to_string(),
        path: None,
        kind: cd.kind,
        label: cd.label.to_string(),
    })
}

/// Resolve a measure token (`count`, `avg:value`, `p95:duration_ms`).
/// Returns (SQL expression, label, path to bind for the expression's `?`).
fn resolve_measure(ds: &Dataset, m: &str) -> Option<(String, String, Option<String>)> {
    if m == "count" {
        return Some(("count(*)".to_string(), "count".to_string(), None));
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
    // Every aggregate above wraps the column expression exactly once.
    Some((expr, format!("{func}({})", r.label), r.path))
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
    /// "sql" | "events" | "series" | "breakdown" — drives how the page renders.
    pub mode: &'static str,
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

/// Build the shared `WHERE …` clause and its bound parameters for a dataset,
/// from the range + filter rows. Reused by the plan compiler and the facets.
fn build_where(ds: &Dataset, state: &QueryState) -> Result<(String, Vec<String>), String> {
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
        let r = resolve_col(ds, &f.col).ok_or_else(|| format!("unknown column '{}'", f.col))?;
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
                push_path(&mut params, &r); // path `?` precedes the value `?`
                params.push(f.val.clone());
            }
            "~" | "!~" => {
                let like = if op == "~" { "ILIKE" } else { "NOT ILIKE" };
                conds.push(format!("{e} {like} '%' || ? || '%'"));
                push_path(&mut params, &r);
                params.push(f.val.clone());
            }
            ">" | ">=" | "<" | "<=" => {
                conds.push(format!("CAST({e} AS DOUBLE) {op} CAST(? AS DOUBLE)"));
                push_path(&mut params, &r);
                params.push(f.val.clone());
            }
            // These repeat the expression twice, so its path binds twice.
            "exists" => {
                conds.push(format!("({e} IS NOT NULL AND CAST({e} AS VARCHAR) <> '')"));
                push_path(&mut params, &r);
                push_path(&mut params, &r);
            }
            "nexists" => {
                conds.push(format!("({e} IS NULL OR CAST({e} AS VARCHAR) = '')"));
                push_path(&mut params, &r);
                push_path(&mut params, &r);
            }
            _ => return Err(format!("unknown operator '{}'", f.op)),
        }
    }
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    Ok((where_sql, params))
}

/// Compile a builder query into count/table/chart SQL.
pub fn compile(state: &QueryState) -> Plan {
    let ds = match dataset(&state.dataset) {
        Some(d) => d,
        None => return fail("unknown dataset"),
    };

    // WHERE + shared params.
    let (where_sql, params) = match build_where(ds, state) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };

    // Measures.
    let measures: Vec<String> = if state.measures().is_empty() {
        vec!["count".to_string()]
    } else {
        state.measures()
    };
    let mut mexpr: Vec<String> = Vec::new();
    let mut mpath: Vec<Option<String>> = Vec::new();
    let mut mlabels: Vec<String> = Vec::new();
    for m in &measures {
        match resolve_measure(ds, m) {
            Some((expr, label, path)) => {
                mexpr.push(expr);
                mpath.push(path);
                mlabels.push(label);
            }
            None => return fail(&format!("invalid measure '{m}'")),
        }
    }

    // Group-by (max 2).
    let group_keys: Vec<String> = state.groups().into_iter().take(2).collect();
    let mut gexpr: Vec<String> = Vec::new();
    let mut gpath: Vec<Option<String>> = Vec::new();
    for g in &group_keys {
        match resolve_col(ds, g) {
            Some(r) => {
                gexpr.push(r.expr);
                gpath.push(r.path);
            }
            None => return fail(&format!("unknown column '{g}'")),
        }
    }
    let grouped = !gexpr.is_empty();

    let from = format!(" FROM otel.main.\"{}\"", ds.table);
    let bucket = bucket_for(state);
    // The chart x axis: uPlot's time scale expects epoch SECONDS (fractional
    // kept as f64), so nanoseconds divide by 1e9 — the old 1e6 fed it
    // milliseconds and it rendered dates in the year ~58 million.
    let xs_secs = format!(
        "epoch_ns(time_bucket(INTERVAL '{bucket}', {})) / {NS_TO_SECS}",
        ds.time
    );

    // Count of matching rows.
    let count_sql = format!("SELECT count(*){from}{where_sql}");

    // Table: grouped totals, or a time-bucket detail when ungrouped.
    // Grouped SQL groups by the SELECT aliases (g0, g1), never by repeating
    // the expressions: DuckDB matches GROUP BY expressions syntactically, so
    // an attribute read would have to bind its path `?` twice — and two
    // placeholders are never "the same expression" to the binder.
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
        let gb: Vec<String> = (0..gexpr.len()).map(|i| format!("g{i}")).collect();
        let sql = format!(
            "SELECT {}{from}{where_sql} GROUP BY {} ORDER BY m0 DESC LIMIT {}",
            sel.join(", "),
            gb.join(", "),
            state.limit()
        );
        // `?` order of appearance: SELECT (groups then measures), then WHERE.
        let mut tp = paths(&gpath);
        tp.extend(paths(&mpath));
        tp.extend(params.iter().cloned());
        (sql, tp)
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
        let mut tp = paths(&mpath);
        tp.extend(params.iter().cloned());
        (sql, tp)
    };

    // Chart: time-bucketed. Grouped → (x, group, first measure) for stacked
    // bars; ungrouped → (x, measure…) for one line per measure.
    let (chart_sql, chart_params, stacked) = if grouped {
        let sel = format!("{xs_secs} AS _t, {} AS _g, {} AS _v", gexpr[0], mexpr[0]);
        // _g aliases the group expression for the same reason the table
        // groups by g0: one expression, one path `?`.
        let sql = format!(
            "SELECT {sel}{from}{where_sql} GROUP BY time_bucket(INTERVAL '{bucket}', {}), _g ORDER BY _t",
            ds.time
        );
        // `?` order: SELECT (_g then _v), then WHERE.
        let mut cp: Vec<String> = Vec::new();
        cp.extend(gpath.first().and_then(|p| p.clone()));
        cp.extend(mpath.first().and_then(|p| p.clone()));
        cp.extend(params.iter().cloned());
        (sql, cp, true)
    } else {
        let sel: Vec<String> = std::iter::once(format!("{xs_secs} AS _t"))
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
        let mut cp = paths(&mpath);
        cp.extend(params.iter().cloned());
        (sql, cp, false)
    };

    let display = inline(&table_sql, &table_params);

    Plan {
        error: None,
        count_sql,
        count_params: params,
        table_sql,
        table_params,
        chart_sql,
        chart_params,
        measure_labels: mlabels,
        group_keys,
        display,
        stacked,
        mode: if grouped { "breakdown" } else { "series" },
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
        mode: "series",
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
        mode: "sql",
    }
}

/// Compile the raw-events table: the actual matching rows (log lines, spans,
/// datapoints), newest first. This is the drill-through target — the rows
/// behind every aggregate.
pub fn compile_events(state: &QueryState) -> Plan {
    let ds = match dataset(&state.dataset) {
        Some(d) => d,
        None => return fail("unknown dataset"),
    };
    let (where_sql, params) = match build_where(ds, state) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let from = format!(" FROM otel.main.\"{}\"", ds.table);
    let mut sel: Vec<String> = vec![format!("CAST({} AS VARCHAR) AS _t", ds.time)];
    // Display columns resolve through the same path; any attribute read adds
    // a path param ahead of the WHERE params (SELECT precedes WHERE).
    let mut table_params: Vec<String> = Vec::new();
    for k in ds.display {
        match resolve_col(ds, k) {
            Some(r) => {
                sel.push(format!("{} AS \"{}\"", r.expr, k));
                table_params.extend(r.path);
            }
            None => return fail(&format!("unknown column '{k}'")),
        }
    }
    let sql = format!(
        "SELECT {}{from}{where_sql} ORDER BY {} DESC LIMIT {}",
        sel.join(", "),
        ds.time,
        state.limit()
    );
    let count_sql = format!("SELECT count(*){from}{where_sql}");
    table_params.extend(params.iter().cloned());
    let display = inline(&sql, &table_params);
    Plan {
        error: None,
        count_sql,
        count_params: params,
        table_sql: sql,
        table_params,
        chart_sql: String::new(),
        chart_params: vec![],
        measure_labels: vec![],
        group_keys: vec![],
        display,
        stacked: false,
        mode: "events",
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

/// The result grid's cells as they are displayed — the SQL console and the
/// Explore events/breakdown tabs both render through here, so one rule covers
/// every stored value the page can echo (§5): each cell goes through the shared
/// redactor unless the operator opted in with `LOTEL_AGENT_SHOW_CONTENT`.
///
/// The drill link is built from the *displayed* text too: a masked cell whose
/// href still carried the raw value would leak the prompt through the link,
/// which is the very thing the redactor exists to prevent.
fn display_cells(
    rows: &[Vec<String>],
    show_content: bool,
    href: impl Fn(usize, &str) -> Option<String>,
) -> Vec<Vec<(String, Option<String>)>> {
    rows.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(ci, cell)| {
                    let text =
                        if show_content { cell.clone() } else { crate::agents::redact_cell(cell) };
                    let link = href(ci, &text);
                    (text, link)
                })
                .collect()
        })
        .collect()
}

/// One value in a facet: label, row count, bar width (0-100), and the
/// click-through URL that toggles a `col = value` filter.
#[derive(Serialize)]
pub struct FacetItem {
    pub v: String,
    pub cnt: i64,
    pub pct: i64,
    pub href: String,
}

/// A faceted column: top values plus whole-column stats for the hover tooltip.
#[derive(Serialize)]
pub struct Facet {
    pub key: String,
    pub label: String,
    pub kind: &'static str, // "num" | "text"
    pub distinct: i64,
    pub min: Option<String>,
    pub max: Option<String>,
    pub items: Vec<FacetItem>,
}

/// Percent-encode a query-string value (unreserved set passes through).
fn qenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Truncate a facet/table label to `n` chars with an ellipsis.
fn short(s: &str, n: usize) -> String {
    let mut it = s.chars();
    let head: String = it.by_ref().take(n).collect();
    if it.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Rebuild the `/query` URL for the current state with a different filter set
/// and result tab.
fn build_url(state: &QueryState, filters: &[Filter], tab: &str) -> String {
    let m = serde_json::to_string(&state.measures()).unwrap_or_else(|_| "[]".into());
    let g = serde_json::to_string(&state.groups()).unwrap_or_else(|_| "[]".into());
    let f = serde_json::to_string(filters).unwrap_or_else(|_| "[]".into());
    let mut s = format!(
        "/query?dataset={}&range={}",
        qenc(&state.dataset),
        qenc(&state.range)
    );
    if !state.bucket.is_empty() {
        s.push_str(&format!("&bucket={}", qenc(&state.bucket)));
    }
    s.push_str(&format!(
        "&measures={}&filters={}&group={}&tab={}",
        qenc(&m),
        qenc(&f),
        qenc(&g),
        qenc(tab)
    ));
    s
}

/// URL for the same query switched to a different result tab.
fn tab_url(state: &QueryState, tab: &str) -> String {
    build_url(state, &state.filters(), tab)
}

/// URL that toggles a `col = val` equality filter (click again to remove).
pub fn drill_url(state: &QueryState, col: &str, val: &str) -> String {
    let mut fs = state.filters();
    match fs
        .iter()
        .position(|x| x.col == col && x.op == "=" && x.val == val)
    {
        Some(p) => {
            fs.remove(p);
        }
        None => fs.push(Filter {
            col: col.into(),
            op: "=".into(),
            val: val.into(),
        }),
    }
    build_url(state, &fs, &state.tab)
}

/// Compute facet columns for the current dataset + filters: top values and
/// (per column) count-distinct and, for numerics, min/max. One query/column.
/// Facet labels and their drill links come from stored values, so they go
/// through the same display redactor as the result cells (plan §5).
pub fn facets(db: &Db, state: &QueryState, cfg: &Config) -> Vec<Facet> {
    let ds = match dataset(&state.dataset) {
        Some(d) => d,
        None => return vec![],
    };
    let (where_sql, params) = match build_where(ds, state) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let from = format!(" FROM otel.main.\"{}\"", ds.table);
    let mut out = Vec::new();
    for cd in ds.cols.iter().filter(|c| c.facet) {
        let e = cd.expr;
        let nonnull = if cd.kind == Kind::Num {
            format!("{e} IS NOT NULL")
        } else {
            format!("({e} IS NOT NULL AND CAST({e} AS VARCHAR) <> '')")
        };
        let full_where = if where_sql.is_empty() {
            format!(" WHERE {nonnull}")
        } else {
            format!("{where_sql} AND {nonnull}")
        };
        let inner = format!("SELECT {e} AS v, count(*) AS c{from}{full_where} GROUP BY 1");
        let (stats, numeric) = if cd.kind == Kind::Num {
            (", min(v) OVER () AS mn, max(v) OVER () AS mx", true)
        } else {
            ("", false)
        };
        let sql = format!(
            "SELECT v, c, count(*) OVER () AS d{stats} FROM ({inner}) ORDER BY c DESC LIMIT 6"
        );
        let (_, rows) = match db.query_rows_with(&sql, &bound(&params)) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if rows.is_empty() {
            continue;
        }
        let distinct = rows[0][2].parse().unwrap_or(0);
        let (min, max) = if numeric {
            (rows[0].get(3).cloned(), rows[0].get(4).cloned())
        } else {
            (None, None)
        };
        let cnts: Vec<i64> = rows.iter().map(|r| r[1].parse().unwrap_or(0)).collect();
        let top = cnts.iter().max().copied().unwrap_or(0).max(1);
        let items: Vec<FacetItem> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let v = cfg.display(&r[0]);
                FacetItem {
                    href: drill_url(state, cd.key, &v),
                    v,
                    cnt: cnts[i],
                    pct: (cnts[i] as f64 / top as f64 * 100.0).round() as i64,
                }
            })
            .collect();
        out.push(Facet {
            key: cd.key.to_string(),
            label: cd.label.to_string(),
            kind: if numeric { "num" } else { "text" },
            distinct,
            min,
            max,
            items,
        });
    }
    out
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
    let cfg = context::<Config>(cx);
    let mut s = parse_query_params::<QueryState>(cx).unwrap_or_default();
    // A fresh visit carries no dataset; the builder's select shows Logs, so
    // the compiler must agree rather than fail the first screen with
    // "unknown dataset".
    if s.dataset.is_empty() {
        s.dataset = "logs".into();
    }
    let sql_mode = s.is_sql();

    // Pick the result tab. SQL console wins; else honour ?tab=, defaulting to
    // the raw events when ungrouped and the breakdown when grouped.
    let want_events = !sql_mode
        && match s.tab.as_str() {
            "events" => true,
            "series" | "breakdown" => false,
            _ => s.groups().is_empty(),
        };
    let plan = if sql_mode {
        compile_sql(&s)
    } else if want_events {
        compile_events(&s)
    } else {
        compile(&s)
    };
    let res = run(db, &plan);
    let measures_text = plan.measure_labels.join(" · ");
    let group_text = plan.group_keys.join(", ");

    let ds = dataset(if s.dataset.is_empty() {
        "logs"
    } else {
        &s.dataset
    });
    let grouped = !plan.group_keys.is_empty();
    let ng = plan.group_keys.len();
    let events_mode = plan.mode == "events";

    // Facets + per-column stats (builder only): one windowed query per column.
    let facets = if sql_mode { Vec::new() } else { facets(db, &s, cfg) };
    let mut stats = serde_json::Map::new();
    for f in &facets {
        stats.insert(
            f.key.clone(),
            json!({ "kind": f.kind, "distinct": f.distinct, "min": f.min, "max": f.max }),
        );
    }

    // Friendly headers (builder) + drill-through links on breakdown cells.
    let head_cells: Vec<(String, Option<String>)> = if sql_mode {
        res.columns.iter().map(|c| (c.clone(), None)).collect()
    } else if events_mode {
        let mut h = vec![("time".to_string(), None)];
        if let Some(d) = ds {
            h.extend(d.display.iter().map(|k| {
                let label = resolve_col(d, k)
                    .map(|r| r.label)
                    .unwrap_or_else(|| k.to_string());
                (label, Some(k.to_string()))
            }));
        }
        h
    } else if grouped {
        let mut h: Vec<(String, Option<String>)> = plan
            .group_keys
            .iter()
            .map(|k| {
                let label = ds
                    .and_then(|d| resolve_col(d, k))
                    .map(|r| r.label)
                    .unwrap_or_else(|| k.clone());
                (label, Some(k.clone()))
            })
            .collect();
        h.extend(plan.measure_labels.iter().map(|m| (m.clone(), None)));
        h
    } else {
        let mut h = vec![("time".to_string(), None)];
        h.extend(plan.measure_labels.iter().map(|m| (m.clone(), None)));
        h
    };
    let disp = ds.map(|d| d.display).unwrap_or(&[]);
    let cells = display_cells(&res.rows, cfg.agent_show_content, |ci, cell| {
        if events_mode {
            // col 0 is time (plain); then one per display column.
            if ci == 0 {
                None
            } else {
                disp.get(ci - 1).map(|k| {
                    if *k == "trace_id" && !cell.is_empty() {
                        format!("/traces?trace={}", cell)
                    } else {
                        drill_url(&s, k, cell)
                    }
                })
            }
        } else if !sql_mode && grouped && ci < ng {
            Some(if plan.group_keys[ci] == "trace_id" {
                format!("/traces?trace={}", cell)
            } else {
                drill_url(&s, &plan.group_keys[ci], cell)
            })
        } else {
            None
        }
    });

    // Result tabs (builder only). Breakdown shows only when grouped.
    let tabs: Vec<(&'static str, &'static str, String, bool)> = if sql_mode {
        Vec::new()
    } else {
        let cur = plan.mode;
        let mut t = vec![
            ("events", "Events", tab_url(&s, "events"), cur == "events"),
            ("series", "Series", tab_url(&s, "series"), cur == "series"),
        ];
        if grouped {
            t.push((
                "breakdown",
                "Breakdown",
                tab_url(&s, "breakdown"),
                cur == "breakdown",
            ));
        }
        t
    };

    // State handed to the client builder so its controls mirror the URL.
    let seed = json!({
        "datasets": DATASETS,
        "operators": OPERATORS,
        "measureFns": MEASURE_FNS.iter().map(|(f,l,nc,nn)| json!({"fn":f,"label":l,"col":nc,"num":nn})).collect::<Vec<_>>(),
        "palette": PALETTE,
        "stats": stats,
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
            "Query the store as a guided builder or raw SQL. Filters, computations, and a breakdown over any column, attributes included."
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
            <div class="qcols">
                <div class="qmain">
                    if !tabs.is_empty() {
                        <div class="rtabs">
                            for (_id, label, href, active) in &tabs {
                                <a
                                    class=(if *active { "rtab on" } else { "rtab" })
                                    href=(href.clone())
                                >
                                    (label.to_string())
                                </a>
                            }
                        </div>
                    }
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
                                    for (label, dstat) in &head_cells {
                                        <th data-stat=(dstat.clone().unwrap_or_default())>(label.clone())</th>
                                    }
                                </tr>
                            </thead>
                            <tbody>
                                for row in &cells {
                                    <tr>
                                        for (text, href) in row {
                                            if let Some(h) = href {
                                                <td><a class="fcell" href=(h.clone())>(text.clone())</a></td>
                                            } else {
                                                <td>(text.clone())</td>
                                            }
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
                </div>

                if !sql_mode {
                    <aside class="qfacets">
                        <div class="qfacets-head">"Facets"</div>
                        for f in &facets {
                            <div class="facet">
                                <div class="fhead" data-stat=(f.key.clone())>
                                    <span class="fname">(f.label.clone())</span>
                                    <span class="fkind">(if f.kind == "num" { "#" } else { "T" })</span>
                                </div>
                                <div class="fvals">
                                    for it in &f.items {
                                        <a class="fval" href=(it.href.clone())>
                                            <span class="fvbar" style=(format!("width:{}%", it.pct))></span>
                                            <span class="fvlab">(short(&it.v, 42))</span>
                                            <span class="fvcnt">(it.cnt.to_string())</span>
                                        </a>
                                    }
                                </div>
                            </div>
                        }
                    </aside>
                }
            </div>
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
                // colors come from the live skin, not hardcoded hex: the
                // series ladder mirrors the service-dot ramp so a service
                // reads the same in table, waterfall, and chart. The seeded
                // console hex stays as the fallback if a token is missing.
                function tok(n,fb){try{var v=getComputedStyle(document.documentElement).getPropertyValue(n).trim();if(v)return v;}catch(e){}return fb;}
                var RAMP=['--color-accent-2','--color-ink-2','--color-muted','--color-muted-2','--color-accent','--color-ink'];
                var showPt=function(u,si){var el=document.createElement('div');el.className='u-cursor-pt';el.style.background=u.series[si].stroke||tok('--color-accent-2','#4f8cc9');return el;};
                var series=[{label:d.time?'time':''}].concat(d.series.map(function(s,i){ var c=tok(RAMP[i%RAMP.length],s.color); return d.stacked
                    ? {label:s.label, stroke:c, fill:c+'33', width:0, type:'bars', stacked:true}
                    : {label:s.label, stroke:c, fill:c+'1f', width:1.5}; }));
                new uPlot({
                    width: Math.max(chartEl.clientWidth,320), height:260, padding:[6,6,0,0],
                    scales:{ x:{ time: !!d.time } },
                    series: series,
                    axes:[ {stroke:tok('--color-muted','#8a93a5'),grid:{stroke:tok('--color-rule-2','#1b212e'),width:1}}, {stroke:tok('--color-muted','#8a93a5'),grid:{stroke:tok('--color-rule-2','#1b212e'),width:1},size:58} ],
                    cursor:{points:{show:d.stacked?false:showPt}}
                }, data, chartEl);
            })
            .catch(function(e){ chartEl.innerHTML='<span class="muted">chart error: '+((e&&e.message)||e)+'</span>'; });
    }

    // ---- column stats tooltip (facet headers + result headers) ----
    var tip = document.createElement('div');
    tip.className = 'qtip'; tip.style.display = 'none';
    document.body.appendChild(tip);
    document.querySelectorAll('[data-stat]').forEach(function(el){
        el.addEventListener('mouseenter', function(){
            var k = el.getAttribute('data-stat'); if (!k) return;
            var st = (L.stats || {})[k]; if (!st) return;
            tip.textContent = st.kind === 'num'
                ? (st.distinct + ' distinct · min ' + st.min + ' · max ' + st.max)
                : (st.distinct + (st.distinct === 1 ? ' distinct value' : ' distinct values'));
            var r = el.getBoundingClientRect();
            tip.style.display = 'block';
            tip.style.left = (r.left + window.scrollX) + 'px';
            tip.style.top = (r.bottom + 6 + window.scrollY) + 'px';
        });
        el.addEventListener('mouseleave', function(){ tip.style.display = 'none'; });
    });
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
    fn chart_x_axis_is_epoch_seconds() {
        // uPlot's time scale expects seconds: both builder chart shapes
        // (lines and stacked) must divide the ns bucket stamp by 1e9, and
        // never the old 1e6 that produced millisecond x values.
        let lines = compile(&state("logs", "", "count", ""));
        assert_eq!(lines.error, None);
        assert!(
            lines.chart_sql.contains(&format!(") / {NS_TO_SECS} AS _t")),
            "{}",
            lines.chart_sql
        );
        assert!(!lines.chart_sql.contains("/ 1000000.0"), "{}", lines.chart_sql);

        let stacked = compile(&state("logs", "", "count", r#"["service_name"]"#));
        assert_eq!(stacked.error, None);
        assert!(
            stacked.chart_sql.contains(&format!(") / {NS_TO_SECS} AS _t")),
            "{}",
            stacked.chart_sql
        );
        assert!(
            !stacked.chart_sql.contains("/ 1000000.0"),
            "{}",
            stacked.chart_sql
        );
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
        // The key travels as a bound path parameter, never inlined into the
        // SQL text (J0: inlining it let a single quote break out of the
        // string literal).
        assert!(
            p.table_sql
                .contains("json_extract_string(log_attributes, ?)"),
            "{}",
            p.table_sql
        );
        assert!(
            p.table_params
                .iter()
                .any(|q| q == "$.\"http.method\""),
            "{:?}",
            p.table_params
        );
        // the human-readable display still shows the full call
        assert!(
            p.display
                .contains("json_extract_string(log_attributes, '$.\"http.method\"')"),
            "{}",
            p.display
        );
    }

    #[test]
    fn attribute_key_quote_is_bound_not_inlined() {
        // J0: resolve_col used to inline '$."<key>"' escaping only ", so a
        // key like x')||'y closed the SQL string literal and injected text
        // into the statement. The path must be a bound parameter instead:
        // no quote in a key can ever reach the SQL text.
        let evil = r#"[{"col":"@log_attributes:x')||'y","op":"=","val":"z"}]"#;
        let p = compile(&state("logs", evil, "", ""));
        assert_eq!(p.error, None);
        for sql in [&p.table_sql, &p.count_sql, &p.chart_sql] {
            assert!(!sql.contains("x'"), "key leaked into SQL text: {sql}");
            assert!(!sql.contains("|| '"), "key text reached the operator: {sql}");
        }
        assert!(
            p.table_sql.contains("json_extract_string(log_attributes, ?)"),
            "path must be bound: {}",
            p.table_sql
        );
        assert!(
            p.table_params
                .iter()
                .any(|q| q == r#"$."x')||'y""#),
            "path must arrive as a bound parameter: {:?}",
            p.table_params
        );
    }

    #[test]
    fn attribute_paths_bind_per_expression_occurrence() {
        // `exists` repeats the expression twice in one condition → one bound
        // path per `?`, in order of appearance.
        let p = compile(&state(
            "logs",
            r#"[{"col":"@log_attributes:http.method","op":"exists","val":""}]"#,
            "",
            "",
        ));
        assert_eq!(p.error, None);
        assert_eq!(p.table_sql.matches('?').count(), 3, "{}", p.table_sql); // cutoff + 2×path
        assert_eq!(p.table_params.len(), 3);
        assert_eq!(p.table_params[1], "$.\"http.method\"");
        assert_eq!(p.table_params[2], "$.\"http.method\"");

        // Grouped: the table/chart group by the SELECT aliases (g0 / _g), so
        // the attribute expression — and its path `?` — appears exactly once.
        // (DuckDB matches GROUP BY expressions syntactically; repeating the
        // expression with a second placeholder binder-errors "must appear in
        // the GROUP BY clause" — the regression this pins.)
        let g = compile(&state("logs", "", "", r#"["@log_attributes:http.method"]"#));
        assert_eq!(g.error, None);
        assert!(g.table_sql.contains("GROUP BY g0"), "{}", g.table_sql);
        assert_eq!(g.table_sql.matches('?').count(), 2, "{}", g.table_sql); // path + cutoff
        assert_eq!(g.table_params.len(), 2);
        assert_eq!(g.table_params[0], "$.\"http.method\"");
        assert!(g.chart_sql.contains(", _g ORDER BY _t"), "{}", g.chart_sql);
        assert_eq!(g.chart_sql.matches('?').count(), 2, "{}", g.chart_sql);
        assert_eq!(g.chart_params.len(), 2);
        assert_eq!(g.chart_params[0], "$.\"http.method\""); // SELECT _g
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
        // Groups are projected once and GROUP BY references the alias, so an
        // attribute group's bound path `?` appears exactly once (DuckDB
        // matches GROUP BY expressions syntactically, not by bound value).
        assert!(p.table_sql.contains("service_name AS g0"));
        assert!(p.table_sql.contains("GROUP BY g0"));
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

    #[test]
    fn drill_url_adds_then_removes_filter() {
        let s = state("logs", "", r#"["avg:value"]"#, r#"["service_name"]"#);
        let added = drill_url(&s, "service_name", "cart");
        assert!(added.contains("dataset=logs") && added.contains("range=24h"));
        assert!(added.contains("cart"), "value should appear: {added}");
        assert!(
            added.contains("measures=") && added.contains("group="),
            "keeps state: {added}"
        );

        let with = QueryState {
            dataset: "logs".into(),
            range: "24h".into(),
            filters: r#"[{"col":"service_name","op":"=","val":"cart"}]"#.into(),
            ..Default::default()
        };
        let removed = drill_url(&with, "service_name", "cart");
        assert!(
            removed.contains("filters=%5B%5D"),
            "filter removed → empty list: {removed}"
        );
    }

    #[test]
    fn drill_url_distinguishes_values_by_col() {
        let s = QueryState {
            dataset: "logs".into(),
            range: "24h".into(),
            filters: r#"[{"col":"service_name","op":"=","val":"cart"}]"#.into(),
            ..Default::default()
        };
        // same value, different column → not treated as a toggle-off
        let added = drill_url(&s, "severity_text", "cart");
        assert!(
            added.contains("severity_text") && added.contains("service_name"),
            "{added}"
        );
    }

    #[test]
    fn short_label_truncates() {
        assert_eq!(short("hello", 10), "hello");
        assert_eq!(short("hello world", 5), "hello\u{2026}");
        assert_eq!(short("", 3), "");
    }

    #[test]
    fn traces_dataset_is_labeled_traces() {
        let d = dataset("traces").unwrap();
        assert_eq!(d.label, "Traces");
        // rows are still spans; trace_id is the drill-through relationship
        assert!(d.display.contains(&"trace_id"));
        assert!(d.display.contains(&"name"));
    }

    #[test]
    fn events_compiles_raw_rows_newest_first() {
        let s = state(
            "traces",
            r#"[{"col":"service_name","op":"=","val":"cart"}]"#,
            "",
            "",
        );
        let p = compile_events(&s);
        assert_eq!(p.error, None);
        assert_eq!(p.mode, "events");
        // one column per display field plus time, newest first, capped
        assert!(p.table_sql.contains("AS \"trace_id\""), "{}", p.table_sql);
        assert!(p.table_sql.contains("AS \"name\""), "{}", p.table_sql);
        assert!(p.table_sql.contains("ORDER BY start_time_unix_nano DESC"));
        assert!(p.table_sql.contains("LIMIT 50"));
        assert!(p.table_sql.contains("service_name = ?") || p.table_sql.contains("= ?"));
        // bound, not interpolated: the value never appears in the SQL text
        assert!(!p.table_sql.contains("cart"));
        assert!(p.count_sql.starts_with("SELECT count(*)"));
    }

    #[test]
    fn events_rejects_bad_column_and_operator() {
        let s = state("logs", r#"[{"col":"evil","op":"=","val":"x"}]"#, "", "");
        assert!(compile_events(&s).error.is_some());
    }

    // ——— privacy: the console / Explore result cells (§5) ———————————————————

    /// The grid every result table renders through: a content value in any
    /// cell is masked, the placeholder keeps its key and shape, numbers and
    /// plain text survive, and the drill link is built from the raw value so
    /// it still matches its row. `LOTEL_AGENT_SHOW_CONTENT` is the only way
    /// the cell renders as stored.
    #[test]
    fn result_cells_run_through_the_shared_redactor() {
        let rows = vec![
            vec![
                "2026-11-17 10:00:00".to_string(),
                r#"{"event.name":"user_prompt","prompt":"the private prompt"}"#.to_string(),
                "1500".to_string(),
            ],
            vec![
                "2026-11-17 10:00:01".to_string(),
                r#"["cli starting","{\"args\":[\"the private command\"]}"]"#.to_string(),
                "7".to_string(),
            ],
        ];
        let hrefs =
            |ci: usize, v: &str| if ci == 0 { None } else { Some(format!("/query?cell={ci}&v={v}")) };

        let cells = display_cells(&rows, false, hrefs);
        let flat: Vec<&str> = cells.iter().flatten().map(|(t, _)| t.as_str()).collect();
        assert_eq!(flat[0], "2026-11-17 10:00:00", "a timestamp is its own text");
        assert_eq!(flat[2], "1500", "a number is not re-shaped by the redactor");
        assert!(flat[1].contains("\"prompt\":\"[REDACTED]\""), "{}", flat[1]);
        assert!(!flat.iter().any(|t| t.contains("private")), "content leaked: {flat:?}");
        assert!(flat[4].contains("cli starting"), "the message element is kept: {}", flat[4]);
        // the drill link is built from the displayed text too: a masked cell
        // must not hand the value back through its own href
        let link = cells[0][1].1.as_deref().expect("the drill link stays");
        assert!(link.contains("[REDACTED]"), "masked label: {link}");
        assert!(!link.contains("private"), "the href must not carry the value: {link}");
        assert!(cells[0][0].1.is_none(), "time is not a drill");

        let shown = display_cells(&rows, true, hrefs);
        assert!(shown[0][1].0.contains("private prompt"), "opt-in shows the cell verbatim");
        assert!(shown[1][1].0.contains("private command"));
        assert!(shown[0][1].1.as_deref().is_some_and(|h| h.contains("private prompt")));
    }
}
