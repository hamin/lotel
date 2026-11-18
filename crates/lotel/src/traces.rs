//! Traces: recent spans, per-service latency, and a single-trace waterfall.
//!
//! The feed shares the logs page's filter bar (crate::filters): service,
//! span name, status, min duration (ms) and attribute filters all AND
//! together over the recent-spans list, the duration distribution and the
//! p95 trend. The waterfall view of one trace is unfiltered by design.

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, request::uri},
    view::{BoxView, Unescaped, View, ViewExt, component, view},
};

use crate::{
    db::Db,
    filters::{Bar, TRACE_COLS, attr_field, empty_range_hint, range_field, service_field},
    stats,
    ui::{cell, thousands},
};

/// The traces page's filter set: the shared bar plus this page's
/// first-class fields (span name, status, min duration).
#[derive(Debug, Clone)]
pub struct TraceFilters {
    pub trace: String,
    /// Select one span for the detail card on the waterfall view.
    pub span: String,
    pub service: String,
    pub name: String,
    /// "" | ok | error
    pub status: String,
    /// lower bound in milliseconds (text as typed; validated below)
    pub mindur: String,
    /// the shared model: time range and the `f` attribute filters
    pub bar: Bar,
}

impl TraceFilters {
    pub fn from(bar: Bar) -> Self {
        Self {
            trace: bar.get("trace").to_string(),
            span: bar.get("span").to_string(),
            service: bar.get("service").to_string(),
            name: bar.get("name").to_string(),
            status: bar.get("status").to_string(),
            mindur: bar.get("mindur").to_string(),
            bar,
        }
    }

    fn mindur_bad(&self) -> bool {
        !self.mindur.is_empty() && self.mindur.parse::<f64>().is_err()
    }

    /// Inline parse feedback shown next to the input — never a 500.
    pub fn errors(&self) -> Vec<String> {
        let mut errs = self.bar.attr_errors();
        if self.mindur_bad() {
            errs.push(format!(
                "min duration must be a number of milliseconds, got '{}'",
                self.mindur
            ));
        }
        errs
    }

    /// The AND-joined conditions (without WHERE) over the span columns,
    /// and their bound parameters in order of appearance. The feed, the
    /// duration distribution and the p95 trend share them.
    fn conds(&self) -> (Vec<String>, Vec<String>) {
        let mut conds = Vec::new();
        let mut params = Vec::new();
        if !self.name.is_empty() {
            conds.push("(\"name\" ILIKE '%' || ? || '%')".to_string());
            params.push(self.name.clone());
        }
        match self.status.as_str() {
            "error" => conds.push("status_code = 2".to_string()),
            "ok" => conds.push("(status_code IS NULL OR status_code <> 2)".to_string()),
            _ => {}
        }
        if !self.mindur.is_empty() && !self.mindur_bad() {
            conds.push("duration_time_unix_nano >= CAST(? AS DOUBLE) * 1000000.0".to_string());
            params.push(self.mindur.clone());
        }
        if !self.service.is_empty() {
            conds.push("(service_name = ?)".to_string());
            params.push(self.service.clone());
        }
        let (ac, ap) = self.bar.attr_conds(&TRACE_COLS);
        conds.extend(ac);
        params.extend(ap);
        (conds, params)
    }
}

/// One span of a trace, positioned for the waterfall.
#[derive(Clone)]
pub struct WaterfallSpan {
    pub span_id: String,
    pub parent: String,
    pub service: String,
    pub name: String,
    pub kind: String,
    pub status_msg: String,
    pub attrs: String,
    pub ms: String,
    pub depth: usize,
    pub offset_pct: f64,
    pub width_pct: f64,
    pub is_error: bool,
}

fn kind_label(kind: i64) -> &'static str {
    match kind {
        1 => "internal",
        2 => "server",
        3 => "client",
        4 => "producer",
        5 => "consumer",
        _ => "",
    }
}

/// Palette index for a service, stable across requests.
pub(crate) fn service_class(service: &str) -> &'static str {
    const CLASSES: [&str; 6] = ["svc0", "svc1", "svc2", "svc3", "svc4", "svc5"];
    let h: u64 = service
        .bytes()
        .fold(0xcbf29ce484222325, |acc, b| (acc ^ b as u64).wrapping_mul(0x100000001b3));
    CLASSES[(h % 6) as usize]
}

fn fmt_ms(ns: i64) -> String {
    let ms = ns as f64 / 1e6;
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 10.0 {
        format!("{:.1} ms", ms)
    } else {
        format!("{:.2} ms", ns as f64 / 1e6)
    }
}

/// Label for a duration-distribution bucket (the CASE in the feed query).
fn dist_label(b: i64) -> &'static str {
    match b {
        0 => "<1 ms",
        1 => "1-5 ms",
        2 => "5-10 ms",
        3 => "10-25 ms",
        4 => "25-50 ms",
        5 => "50-100 ms",
        6 => "100-250 ms",
        7 => "250-500 ms",
        8 => "500 ms-1 s",
        _ => "≥1 s",
    }
}

#[cfg(test)]
mod tests {
    use super::{TraceFilters, dist_label, fmt_ms, kind_label, service_class};
    use crate::filters::Bar;

    #[test]
    fn durations_format_humably() {
        assert_eq!(fmt_ms(3_900_000_000), "3.90 s");
        assert_eq!(fmt_ms(1_500_000), "1.50 ms");
        assert_eq!(fmt_ms(12_340_000), "12.3 ms");
        assert_eq!(fmt_ms(12_345_678), "12.3 ms");
        assert_eq!(fmt_ms(500_000), "0.50 ms");
    }

    #[test]
    fn kind_labels() {
        assert_eq!(kind_label(2), "server");
        assert_eq!(kind_label(3), "client");
        assert_eq!(kind_label(0), "");
    }

    #[test]
    fn duration_buckets_read_left_to_right() {
        assert_eq!(dist_label(0), "<1 ms");
        assert_eq!(dist_label(5), "50-100 ms");
        assert_eq!(dist_label(8), "500 ms-1 s");
        assert_eq!(dist_label(9), "≥1 s");
        assert_eq!(dist_label(99), "≥1 s", "everything past a second lands in the last bucket");
    }

    #[test]
    fn service_colors_are_stable_and_bounded() {
        assert_eq!(service_class("checkout"), service_class("checkout"));
        for s in ["checkout", "cart", "payments", "api-gateway", ""] {
            assert!(
                ["svc0", "svc1", "svc2", "svc3", "svc4", "svc5"].contains(&service_class(s)),
                "bad class for {s}"
            );
        }
    }

    #[test]
    fn filters_grow_from_the_shared_bar() {
        let f = TraceFilters::from(Bar::parse(
            "trace=abc&span=de&service=cart&name=pay&status=error&mindur=250&f=http.method%3DPOST",
        ));
        assert_eq!(f.trace, "abc");
        assert_eq!(f.span, "de");
        assert_eq!(f.service, "cart");
        assert_eq!(f.name, "pay");
        assert_eq!(f.status, "error");
        assert_eq!(f.mindur, "250");
        assert_eq!(f.bar.attrs().len(), 1);

        let (conds, params) = f.conds();
        // name, status, min duration, service, one attribute filter (2 `?`)
        assert_eq!(conds.len(), 5);
        assert!(conds[0].starts_with("(\"name\" ILIKE"));
        assert_eq!(conds[1], "status_code = 2");
        assert_eq!(
            conds[2],
            "duration_time_unix_nano >= CAST(? AS DOUBLE) * 1000000.0"
        );
        assert_eq!(conds[3], "(service_name = ?)");
        assert_eq!(params[0], "pay");
        assert_eq!(params[1], "250");
        assert_eq!(params[2], "cart");
        assert_eq!(params[3], r#"$."http.method""#);
        assert_eq!(params[4], "POST");
    }

    #[test]
    fn status_ok_excludes_only_error_and_bad_duration_errors_inline() {
        let f = TraceFilters::from(Bar::parse("status=ok"));
        let (conds, params) = f.conds();
        assert_eq!(conds, vec!["(status_code IS NULL OR status_code <> 2)".to_string()]);
        assert!(params.is_empty());

        let f = TraceFilters::from(Bar::parse("status=junk"));
        assert!(f.conds().0.is_empty(), "unknown status values filter nothing");

        let f = TraceFilters::from(Bar::parse("mindur=soon"));
        assert!(f.conds().0.is_empty(), "an unparsable duration is skipped, not executed");
        assert_eq!(f.errors().len(), 1);
        assert!(f.errors()[0].contains("soon"));
        // an empty bar is silent
        assert!(TraceFilters::from(Bar::default()).errors().is_empty());
    }
}

/// Fetch a trace and return (spans in waterfall order, total duration ns).
fn fetch_waterfall(db: &Db, trace: &str) -> anyhow::Result<(Vec<WaterfallSpan>, i64)> {
    let (_, rows) = db.query_rows_with(
        "SELECT epoch_ns(start_time_unix_nano), duration_time_unix_nano, span_id,
                COALESCE(parent_span_id, ''), service_name, name,
                COALESCE(kind, 0), COALESCE(status_code, 0),
                COALESCE(status_status_message, ''), COALESCE(span_attributes, '{}')
         FROM otel.main.otlp_traces
         WHERE trace_id = ?
         ORDER BY start_time_unix_nano",
        &[&trace],
    )?;

    struct Node {
        idx: usize,
        start_ns: i64,
        dur_ns: i64,
    }

    let mut starts = Vec::with_capacity(rows.len());
    let mut durs = Vec::with_capacity(rows.len());
    for r in &rows {
        starts.push(r[0].parse::<i64>().unwrap_or(0));
        durs.push(r[1].parse::<i64>().unwrap_or(0));
    }

    // Tree: children per parent (input is start-ordered, so children stay sorted).
    let ids: std::collections::HashSet<&str> = rows.iter().map(|r| r[2].as_str()).collect();
    let mut children: std::collections::HashMap<&str, Vec<usize>> =
        std::collections::HashMap::new();
    let mut roots: Vec<Node> = Vec::new();
    for i in 0..rows.len() {
        let parent = rows[i][3].as_str();
        if parent.is_empty() || !ids.contains(parent) {
            roots.push(Node { idx: i, start_ns: starts[i], dur_ns: durs[i] });
        } else {
            children.entry(parent).or_default().push(i);
        }
    }
    // Roots in start order (defensive; input already ordered).
    roots.sort_by_key(|n| n.start_ns);

    let min_start = starts.iter().copied().min().unwrap_or(0);
    let max_end = starts
        .iter()
        .zip(&durs)
        .map(|(s, d)| s + d)
        .max()
        .unwrap_or(0);
    let total = (max_end - min_start).max(1);

    // Preorder DFS: each span after its parent, children by start time.
    let mut ordered: Vec<WaterfallSpan> = Vec::with_capacity(rows.len());
    {
        let mut stack: Vec<(Node, usize)> = roots.into_iter().map(|n| (n, 0)).collect();
        while let Some((node, depth)) = stack.pop() {
            let i = node.idx;
            let r = &rows[i];
            let offset = node.start_ns - min_start;
            ordered.push(WaterfallSpan {
                span_id: r[2].clone(),
                parent: r[3].clone(),
                service: r[4].clone(),
                name: r[5].clone(),
                kind: kind_label(r[6].parse().unwrap_or(0)).to_string(),
                status_msg: r[8].clone(),
                attrs: r[9].clone(),
                ms: fmt_ms(node.dur_ns),
                depth,
                offset_pct: (offset as f64 / total as f64) * 100.0,
                width_pct: ((node.dur_ns as f64 / total as f64) * 100.0).max(0.8),
                is_error: r[7].parse::<i64>().unwrap_or(0) == 2,
            });
            if let Some(kids) = children.get(r[2].as_str()) {
                for &k in kids.iter().rev() {
                    stack.push((
                        Node { idx: k, start_ns: starts[k], dur_ns: durs[k] },
                        depth + 1,
                    ));
                }
            }
        }
    }
    Ok((ordered, total))
}

/// Distinct service names, for the bar's Service select.
pub(crate) fn option_values(db: &Db) -> Vec<String> {
    db.query_rows(
        "SELECT DISTINCT service_name FROM otel.main.otlp_traces \
         WHERE service_name IS NOT NULL AND service_name != '' ORDER BY 1",
    )
    .map(|(_, rows)| rows.into_iter().map(|r| r[0].clone()).collect())
    .unwrap_or_default()
}

/// The recent-spans feed under the bar's filters: (columns, rows) with the
/// start time, ms and status already rendered by SQL, as the page stitches
/// them straight into cells.
pub(crate) fn fetch_feed(db: &Db, f: &TraceFilters) -> anyhow::Result<(Vec<String>, Vec<Vec<String>>)> {
    let cutoff = f.bar.cutoff();
    let (conds, params) = f.conds();
    let mut sql = String::from(
        "SELECT start_time_unix_nano, service_name, name,
                round(duration_time_unix_nano / 1000000.0, 2),
                CASE COALESCE(status_code, 0)
                    WHEN 1 THEN 'ok'
                    WHEN 2 THEN 'error' || COALESCE(': ' || NULLIF(status_status_message, ''), '')
                    ELSE 'unset' END,
                trace_id, span_id
         FROM otel.main.otlp_traces
         WHERE start_time_unix_nano >= CAST(? AS TIMESTAMP)",
    );
    for c in &conds {
        sql.push_str(&format!(" AND {c}"));
    }
    sql.push_str(" ORDER BY start_time_unix_nano DESC LIMIT 200");
    let mut bind: Vec<&dyn duckdb::ToSql> = vec![&cutoff];
    bind.extend(params.iter().map(|s| s as &dyn duckdb::ToSql));
    db.query_rows_with(&sql, &bind)
}

#[page]
async fn traces(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let f = TraceFilters::from(Bar::parse(uri(cx).query().unwrap_or("")));

    if !f.trace.is_empty() {
        let (spans, total_ns) = fetch_waterfall(db, &f.trace).unwrap_or_default();
        let selected = spans.iter().find(|s| s.span_id == f.span).cloned();
        let back = f.bar.href_without("/traces", &["trace", "span"]);
        return Ok(view! {
            trace_view(
                trace: f.trace.clone(),
                spans: spans,
                total_ns: total_ns,
                selected: selected,
                back: back,
            )
        }
        .boxed());
    }

    // Feed view: per-service latency + recent spans, under the bar.
    let (conds, params) = f.conds();
    let (feed, dist) = {
        let feed = fetch_feed(db, &f).unwrap_or_else(|e| {
            (vec!["error".into()], vec![vec![format!("{e}")]])
        });
        // Duration histogram over the same window and filters as the feed.
        let mut sql = String::from(
            "WITH d AS (
                SELECT duration_time_unix_nano AS ns FROM otel.main.otlp_traces
                WHERE start_time_unix_nano >= CAST(? AS TIMESTAMP)",
        );
        for c in &conds {
            sql.push_str(&format!(" AND {c}"));
        }
        sql.push_str(
            " AND duration_time_unix_nano IS NOT NULL)
             SELECT CASE
                        WHEN ns < 1000000 THEN 0     WHEN ns < 5000000 THEN 1    WHEN ns < 10000000 THEN 2
                        WHEN ns < 25000000 THEN 3    WHEN ns < 50000000 THEN 4   WHEN ns < 100000000 THEN 5
                        WHEN ns < 250000000 THEN 6   WHEN ns < 500000000 THEN 7  WHEN ns < 1000000000 THEN 8
                        ELSE 9 END AS b, count(*) AS n
             FROM d GROUP BY 1 ORDER BY 1",
        );
        let cutoff = f.bar.cutoff();
        let mut bind: Vec<&dyn duckdb::ToSql> = vec![&cutoff];
        bind.extend(params.iter().map(|s| s as &dyn duckdb::ToSql));
        let dist = db.query_rows_with(&sql, &bind).unwrap_or_default().1;
        (feed, dist)
    };

    // An empty feed with a range narrower than all: the window is the likely
    // suspect, so offer the same query at range=all (the logs page's hint).
    let empty_hint = if feed.1.is_empty() && f.bar.range_is_narrow() {
        Some(f.bar.href("/traces", &[("range", "all")]))
    } else {
        None
    };

    let services = db
        .query_rows(
            "SELECT service_name, count(*) AS spans,
                    round(quantile_cont(duration_time_unix_nano / 1000000.0, 0.5), 1) AS p50_ms,
                    round(quantile_cont(duration_time_unix_nano / 1000000.0, 0.95), 1) AS p95_ms,
                    sum(CASE WHEN status_code = 2 THEN 1 ELSE 0 END) AS errored
             FROM otel.main.otlp_traces
             GROUP BY 1 ORDER BY 2 DESC",
        )
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));

    // Latency analytics over the current filters (the feed's own window):
    // an overall p95 trend, a per-service trend column, and a duration
    // histogram. One shared scale in the series hue keeps every sparkline comparable.
    let range = if f.bar.get("range").is_empty() { "all" } else { f.bar.get("range") };
    let (overall_p95, lat_series) = stats::latency_series_where(db, range, &conds, &params);
    let interval = stats::interval_for(range).to_string();
    let max_p95 = lat_series
        .iter()
        .flat_map(|s| s.p95s.iter().copied())
        .chain(overall_p95.iter().copied())
        .fold(0.0f64, f64::max)
        .max(1.0);
    let trends: Vec<String> = services
        .1
        .iter()
        .map(|r| {
            let key = if r[0].is_empty() { "—" } else { r[0].as_str() };
            lat_series
                .iter()
                .find(|s| s.service == key)
                .map(|s| stats::spark_svg(&s.p95s, 130.0, 22.0, "var(--color-accent-2)", max_p95))
                .unwrap_or_default()
        })
        .collect();
    let overall_spark = if overall_p95.is_empty() {
        String::new()
    } else {
        stats::spark_svg(&overall_p95, 560.0, 44.0, "var(--color-accent-2)", max_p95)
    };

    let dist_top = dist
        .iter()
        .filter_map(|r| r[1].parse::<f64>().ok())
        .fold(0.0f64, f64::max)
        .max(1.0);
    let dist_rows: Vec<(String, String, u32)> = dist
        .into_iter()
        .filter_map(|r| {
            let b = r[0].parse::<i64>().ok()?;
            let w = ((r[1].parse::<f64>().unwrap_or(0.0) / dist_top) * 100.0).round() as u32;
            Some((dist_label(b).to_string(), r[1].clone(), w))
        })
        .collect();

    let services_options = option_values(db);
    let service = f.service.clone();
    let name = f.name.clone();
    let status = f.status.clone();
    let mindur = f.mindur.clone();
    let range_current = f.bar.get("range").to_string();
    let raw_f = f.bar.raw_f();
    let errors = f.errors();

    Ok(view! {
        <h1>"Traces"</h1>
        <p class="sub">"Spans land in the capped store with age-based retention; p95 comes straight from the segment files."</p>

        <form class="filters" method="get" action="/traces">
            service_field(options: services_options, current: service)
            <label>"Span name"
                <input type="search" name="name" placeholder="name contains…" value=(name.clone()) />
            </label>
            <label>"Status"
                <select name="status">
                    <option value="" if status.is_empty() { selected="selected" }>"all"</option>
                    <option value="ok" if status == "ok" { selected="selected" }>"ok"</option>
                    <option value="error" if status == "error" { selected="selected" }>"error"</option>
                </select>
            </label>
            <label>"Min duration (ms)"
                <input type="search" name="mindur" placeholder="0" value=(mindur.clone()) />
            </label>
            range_field(current: range_current)
            attr_field(raw: raw_f, errors: errors)
            <button type="submit">"Apply"</button>
        </form>

        <div class="card lat">
            <div>
                <h3>"Overall p95 per bucket"</h3>
                if overall_spark.is_empty() {
                    <p class="muted">"No spans in the store yet."</p>
                } else {
                    (Unescaped::new_unchecked(overall_spark))
                    <p class="lat-cap">(interval.clone())" buckets · the series hue, one shared scale across all sparklines on this page"</p>
                }
            </div>
            <div>
                <h3>"Span duration distribution"</h3>
                if dist_rows.is_empty() {
                    <p class="muted">"No spans with a duration yet."</p>
                }
                for (label, n, width) in &dist_rows {
                    <div class="dist-row">
                        <span class="muted">(label.clone())</span>
                        <span class="facet-bar"><i style=(format!("width:{}%", width))></i></span>
                        <span class="dist-n">(thousands(n))</span>
                    </div>
                }
            </div>
        </div>

        <h2>"By service"</h2>
        <table>
            <thead>
                <tr><th>"service"</th><th>"spans"</th><th>"p50 ms"</th><th>"p95 ms"</th><th>"errors"</th><th>"p95 trend"</th></tr>
            </thead>
            <tbody>
                for (row, trend) in services.1.iter().zip(&trends) {
                    <tr>
                        <td>(cell(row, 0))</td>
                        <td>(cell(row, 1))</td>
                        <td>(cell(row, 2))</td>
                        <td>(cell(row, 3))</td>
                        <td>(cell(row, 4))</td>
                        <td class="sm-spark sm-lat">(Unescaped::new_unchecked(trend.clone()))</td>
                    </tr>
                }
            </tbody>
        </table>

        <h2>"Recent spans"</h2>
        <table>
            <thead>
                <tr><th>"start"</th><th>"service"</th><th>"span"</th><th>"ms"</th><th>"status"</th><th>"trace"</th></tr>
            </thead>
            <tbody>
                if let Some(h) = &empty_hint {
                    empty_range_hint(colspan: 6, noun: "spans", href: h.clone())
                }
                for row in &feed.1 {
                    <tr>
                        <td class="muted">(cell(row, 0))</td>
                        <td>(cell(row, 1))</td>
                        <td class="namecell">(cell(row, 2))</td>
                        <td>(cell(row, 3))</td>
                        <td>(cell(row, 4))</td>
                        <td><a class="trace-link mono" href=(format!("/traces?trace={}", cell(row, 5)))>(cell(row, 6))</a></td>
                    </tr>
                }
            </tbody>
        </table>
    }
    .boxed())
}

/// Single-trace waterfall with an optional span detail card.
#[component]
async fn trace_view(
    cx: &Cx,
    trace: String,
    spans: Vec<WaterfallSpan>,
    total_ns: i64,
    selected: Option<WaterfallSpan>,
    back: String,
) -> Result<impl View> {
    let errors = spans.iter().filter(|s| s.is_error).count();

    Ok(view! {
        <h1>"Trace"</h1>
        <p class="sub meta">
            <span class="mono seg">(trace.clone())</span>
            <span class="seg">((spans.len()).to_string())" spans"</span>
            <span class="seg">(fmt_ms(total_ns))" total"</span>
            if errors > 0 {
                <span class="badge error">(format!("{errors} errored"))</span>
            } else {
                <span class="badge info">"no errors"</span>
            }
        </p>
        <p><a class="trace-link" href=(back)>"← all spans"</a></p>

        if spans.is_empty() {
            <p class="muted">"No spans for this trace (or it has aged out)."</p>
        } else {
            <div class="wf">
                <div class="wfruler">
                    <div class="wflabel">"span"</div>
                    <div class="wftrack">
                        <span class="tick" style="left:0%">"0"</span>
                        <span class="tick" style="left:50%">(fmt_ms(total_ns / 2))</span>
                        <span class="tick" style="left:calc(100% - 4rem)">"end"</span>
                    </div>
                </div>
                for s in spans {
                    <a class="wfrow" href=(format!("/traces?trace={}&span={}", trace.clone(), s.span_id.clone()))>
                        <div class="wflabel" style=(format!("padding-left:{}px", s.depth * 14))>
                            <span class=(format!("svc-dot {}", service_class(&s.service)))>"●"</span>
                            <b>(s.service.clone())</b>
                            " "
                            (s.name.clone())
                            if !s.kind.is_empty() {
                                <span class="muted">(format!(" · {}", s.kind))</span>
                            }
                        </div>
                        <div class="wftrack">
                            <span
                                class=(format!("wfbar {} {}", service_class(&s.service), if s.is_error { "wferr" } else { "" }))
                                style=(format!("left:{}%;width:{}%", s.offset_pct, s.width_pct))
                                title=(format!("{} · {} · {}", s.span_id.clone(), s.kind.clone(), s.ms.clone()))
                            >
                                (s.ms.clone())
                            </span>
                        </div>
                    </a>
                }
            </div>

            if let Some(s) = selected {
                <h2>"Span"</h2>
                <div class="card">
                    <table class="kv">
                        <tr><td>"span_id"</td><td><code>(s.span_id.clone())</code></td></tr>
                        <tr><td>"parent"</td><td><code>(if s.parent.is_empty() { "— (root)".to_string() } else { s.parent.clone() })</code></td></tr>
                        <tr><td>"service"</td><td>(s.service.clone())</td></tr>
                        <tr><td>"name"</td><td>(s.name.clone())</td></tr>
                        <tr><td>"kind"</td><td>(s.kind.clone())</td></tr>
                        <tr><td>"duration"</td><td>(s.ms.clone())</td></tr>
                        if !s.status_msg.is_empty() {
                            <tr><td>"status"</td><td><span class="badge error">"ERROR"</span>" "(s.status_msg.clone())</td></tr>
                        }
                    </table>
                    <h2>"Attributes"</h2>
                    <pre>(s.attrs.clone())</pre>
                </div>
            }
        }
    })
}

// ————————————————————————————————————————————— DB fixtures ——————————————————————————————————————————————
// The traces side of the shared-bar promise: filters return exactly the
// expected rows from the fixture spans.

#[cfg(test)]
mod db_tests {
    use super::{TraceFilters, fetch_feed};
    use crate::filters::Bar;
    use crate::logs::db_tests::temp_db;
    use std::fs;

    /// Two slow spans of one trace (POST /payments errors at 3.2 s under
    /// checkout; the root span is ok at 3.9 s) and one fast cart span —
    /// the shape of the OTLP fixtures the smoke test ingests.
    fn seed_traces(db: &crate::db::Db) {
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE otel.main.otlp_traces(
                    start_time_unix_nano TIMESTAMP, duration_time_unix_nano BIGINT,
                    trace_id VARCHAR, span_id VARCHAR, parent_span_id VARCHAR,
                    service_name VARCHAR, \"name\" VARCHAR, kind INTEGER,
                    status_code INTEGER, status_status_message VARCHAR,
                    resource_attributes VARCHAR, span_attributes VARCHAR)
                 WITH (time_col = 'start_time_unix_nano', ttl = '3 days', max_bytes = '64MB');
                 INSERT INTO otel.main.otlp_traces
                    (start_time_unix_nano, duration_time_unix_nano, trace_id, span_id, parent_span_id,
                     service_name, \"name\", kind, status_code, status_status_message, resource_attributes, span_attributes)
                 VALUES
                    ('2026-11-17 10:00:00', 3200000000, 't1', 'sp1', 'sp2',
                     'checkout', 'POST /payments', 2, 2, 'gateway timeout',
                     '{\"service.name\":\"checkout\"}', '{\"http.method\":\"POST\",\"http.status_code\":504}'),
                    ('2026-11-17 09:59:59', 3900000000, 't1', 'sp2', '',
                     'checkout', 'POST /checkout', 1, 0, '',
                     '{\"service.name\":\"checkout\"}', '{}'),
                    ('2026-11-17 10:00:01', 45000000, 't2', 'sp3', '',
                     'cart', 'GET /cart', 2, 0, '',
                     '{\"service.name\":\"cart\",\"deployment.environment\":\"prod\"}', '{\"http.method\":\"GET\"}');",
            )?;
            Ok(())
        })
        .expect("seed traces");
    }

    fn names(tag: &str, query: &str) -> Vec<String> {
        let db = temp_db(tag);
        seed_traces(&db);
        let f = TraceFilters::from(Bar::parse(query));
        let (_, rows) = fetch_feed(&db, &f).expect("filtered feed");
        let mut out: Vec<String> = rows.into_iter().map(|r| r[2].clone()).collect();
        out.sort();
        let _ = fs::remove_dir_all(&db.data_dir);
        out
    }

    #[test]
    fn min_duration_returns_only_the_slow_spans() {
        assert_eq!(
            names("tr-mindur", "mindur=3500"),
            vec!["POST /checkout".to_string()]
        );
        assert_eq!(
            names("tr-mindur-all", "mindur=1"),
            vec!["GET /cart".to_string(), "POST /checkout".to_string(), "POST /payments".to_string()]
        );
        // 3.2 s is exactly the payments span — inclusive lower bound
        assert_eq!(
            names("tr-mindur-exact", "mindur=3200"),
            vec!["POST /checkout".to_string(), "POST /payments".to_string()]
        );
    }

    #[test]
    fn attribute_name_status_and_service_filters() {
        assert_eq!(
            names("tr-attr", "f=http.method%3DPOST"),
            vec!["POST /payments".to_string()],
            "the root span carries no attributes at all — only has-the-key rows match"
        );
        assert_eq!(
            names("tr-attr-num", "f=http.status_code%3D504"),
            vec!["POST /payments".to_string()]
        );
        assert_eq!(names("tr-name", "name=%2Fcart"), vec!["GET /cart".to_string()]);
        assert_eq!(names("tr-status", "status=error"), vec!["POST /payments".to_string()]);
        assert_eq!(
            names("tr-status-ok", "status=ok"),
            vec!["GET /cart".to_string(), "POST /checkout".to_string()]
        );
        assert_eq!(
            names("tr-resource", "f=resource.deployment.environment%3Dprod"),
            vec!["GET /cart".to_string()]
        );
        // body filters have no column on traces: skipped, not an error
        assert_eq!(
            names("tr-body-skip", "f=body.msg%3Danything"),
            vec!["GET /cart".to_string(), "POST /checkout".to_string(), "POST /payments".to_string()]
        );
    }

    #[test]
    fn filters_and_and_bad_input_never_500s() {
        assert_eq!(
            names("tr-and", "mindur=1000&f=http.method%3DPOST&status=error&service=checkout"),
            vec!["POST /payments".to_string()]
        );
        // junk min duration and a junk attribute line: everything stands
        assert_eq!(
            names("tr-junk", "mindur=soon&f=nonsense"),
            vec!["GET /cart".to_string(), "POST /checkout".to_string(), "POST /payments".to_string()]
        );
    }
}
