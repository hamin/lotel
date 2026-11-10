//! Traces: recent spans, per-service latency, and a single-trace drill-down.

use serde::Deserialize;

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{View, view},
};

use crate::db::Db;

#[derive(Debug, Default, Deserialize)]
pub struct TraceFilters {
    #[serde(default)]
    pub trace: String,
    #[serde(default)]
    pub service: String,
}

#[page]
async fn traces(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let f = parse_query_params::<TraceFilters>(cx).unwrap_or_default();

    // Drill-down when ?trace= is set, otherwise the recent span feed.
    let spans = if f.trace.is_empty() {
        db.query_rows_with(
            "SELECT start_time_unix_nano, service_name, name,
                    round(duration_time_unix_nano / 1000000.0, 2),
                    COALESCE(status_status_message, ''), trace_id, span_id
             FROM otel.main.otlp_traces
             WHERE (? = '' OR service_name = ?)
             ORDER BY start_time_unix_nano DESC LIMIT 200",
            &[&f.service, &f.service],
        )
    } else {
        db.query_rows_with(
            "SELECT start_time_unix_nano, service_name, name,
                    round(duration_time_unix_nano / 1000000.0, 2),
                    COALESCE(status_status_message, ''), trace_id, span_id
             FROM otel.main.otlp_traces
             WHERE trace_id = ?
             ORDER BY start_time_unix_nano",
            &[&f.trace],
        )
    }
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

        <h2>
            if f.trace.is_empty() {
                "Recent spans"
            } else {
                "Trace "
            }
        </h2>
        if !f.trace.is_empty() {
            <p>
                <span class="muted mono">(f.trace.clone())</span>
                <a class="trace-link" href="/traces">"← all spans"</a>
            </p>
        }
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
    })
}
