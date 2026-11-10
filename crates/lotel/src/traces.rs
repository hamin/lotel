//! Traces: recent spans, per-service latency, and a single-trace waterfall.

use serde::Deserialize;

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{BoxView, View, ViewExt, component, view},
};

use crate::db::Db;

#[derive(Debug, Default, Deserialize)]
pub struct TraceFilters {
    #[serde(default)]
    pub trace: String,
    #[serde(default)]
    pub service: String,
    /// Select one span for the detail card on the waterfall view.
    #[serde(default)]
    pub span: String,
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
fn service_class(service: &str) -> &'static str {
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
        format!("{:.2} ms", ms)
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

#[page]
async fn traces(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let f = parse_query_params::<TraceFilters>(cx).unwrap_or_default();

    if !f.trace.is_empty() {
        let (spans, total_ns) = fetch_waterfall(db, &f.trace).unwrap_or_default();
        let selected = spans.iter().find(|s| s.span_id == f.span).cloned();
        return Ok(view! {
            trace_view(
                trace: f.trace.clone(),
                spans: spans,
                total_ns: total_ns,
                selected: selected,
            )
        }
        .boxed());
    }

    // Feed view: per-service latency + recent spans.
    let spans = db
        .query_rows_with(
            "SELECT start_time_unix_nano, service_name, name,
                    round(duration_time_unix_nano / 1000000.0, 2),
                    COALESCE(status_status_message, ''), trace_id, span_id
             FROM otel.main.otlp_traces
             WHERE (? = '' OR service_name = ?)
             ORDER BY start_time_unix_nano DESC LIMIT 200",
            &[&f.service, &f.service],
        )
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));

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

    Ok(view! {
        <h1>"Traces"</h1>
        <p class="sub">"Spans land in the capped store with age-based retention; p95 comes straight from the segment files."</p>

        <h2>"By service"</h2>
        <table>
            <thead>
                <tr><th>"service"</th><th>"spans"</th><th>"p50 ms"</th><th>"p95 ms"</th><th>"errors"</th></tr>
            </thead>
            <tbody>
                for row in &services.1 {
                    <tr>
                        for cell in row {
                            <td>(cell.clone())</td>
                        }
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
                for row in &spans.1 {
                    <tr>
                        <td class="muted">(row[0].clone())</td>
                        <td>(row[1].clone())</td>
                        <td class="logbody">(row[2].clone())</td>
                        <td>(row[3].clone())</td>
                        <td>(row[4].clone())</td>
                        <td><a class="trace-link mono" href=(format!("/traces?trace={}", row[5]))>(row[6].clone())</a></td>
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
) -> Result<impl View> {
    let errors = spans.iter().filter(|s| s.is_error).count();

    Ok(view! {
        <h1>"Trace"</h1>
        <p class="sub">
            <span class="mono">(trace.clone())</span>
            " · "
            ((spans.len()).to_string())
            " spans · "
            (fmt_ms(total_ns))
            " total · "
            if errors > 0 {
                <span class="badge error">(format!("{errors} errored"))</span>
            } else {
                <span class="badge info">"no errors"</span>
            }
        </p>
        <p><a class="trace-link" href="/traces">"← all spans"</a></p>

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
