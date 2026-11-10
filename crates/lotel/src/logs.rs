//! Logs explorer: filtered, tail-able view over otel.main.otlp_logs.

use serde::Deserialize;

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{View, component, view},
};

use crate::{
    db::Db,
    ui::{live_refresh, sev_class},
};

/// Query-string filters (`?q=&service=&severity=&range=&limit=`).
#[derive(Debug, Default, Deserialize)]
pub struct LogFilters {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub severity: String,
    /// `1h` | `6h` | `24h` | `7d` | anything else = all time
    #[serde(default)]
    pub range: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

impl LogFilters {
    /// (cutoff timestamp for the WHERE, validated row cap)
    fn bind(&self) -> (String, usize) {
        let hours = match self.range.as_str() {
            "1h" => Some(1),
            "6h" => Some(6),
            "24h" => Some(24),
            "7d" => Some(24 * 7),
            _ => None,
        };
        let cutoff = match hours {
            Some(h) => chrono::Utc::now() - chrono::Duration::hours(h),
            None => chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
        };
        let cutoff = cutoff.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string();
        (cutoff, self.limit.unwrap_or(200).clamp(1, 2000))
    }
}

#[cfg(test)]
mod tests {
    use super::LogFilters;

    #[test]
    fn all_time_cutoff_is_epoch() {
        let f = LogFilters::default();
        let (cutoff, limit) = f.bind();
        assert_eq!(cutoff, "1970-01-01 00:00:00");
        assert_eq!(limit, 200);
    }

    #[test]
    fn range_and_limit_are_clamped() {
        let f = LogFilters { range: "24h".into(), limit: Some(9999), ..Default::default() };
        let (cutoff, limit) = f.bind();
        assert_ne!(cutoff, "1970-01-01 00:00:00");
        assert_eq!(limit, 2000);

        let f = LogFilters { limit: Some(0), ..Default::default() };
        assert_eq!(f.bind().1, 1);
    }
}

/// One rendered log line.
pub struct LogRow {
    pub time: String,
    pub service: String,
    pub severity: String,
    pub body: String,
    pub trace_id: String,
}

/// Shared query for the page and the tail fragment endpoint.
pub(crate) fn fetch(db: &Db, f: &LogFilters) -> anyhow::Result<Vec<LogRow>> {
    let (cutoff, limit) = f.bind();
    let (_, rows) = db.query_rows_with(
        &format!(
            "SELECT time_unix_nano, service_name, severity_text, body, COALESCE(trace_id, '')
             FROM otel.main.otlp_logs
             WHERE time_unix_nano >= CAST(? AS TIMESTAMP)
               AND (? = '' OR body ILIKE '%' || ? || '%')
               AND (? = '' OR service_name = ?)
               AND (? = '' OR upper(severity_text) = upper(?))
             ORDER BY time_unix_nano DESC
             LIMIT {limit}"
        ),
        &[
            &cutoff, &f.q, &f.q, &f.service, &f.service, &f.severity, &f.severity,
        ],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| LogRow {
            time: r[0].clone(),
            service: r[1].clone(),
            severity: r[2].clone(),
            body: r[3].clone(),
            trace_id: r[4].clone(),
        })
        .collect())
}

pub(crate) fn option_values(db: &Db, column: &str) -> Vec<String> {
    // column comes from our own code, never user input
    db.query_rows(&format!(
        "SELECT DISTINCT {column} FROM otel.main.otlp_logs WHERE {column} IS NOT NULL AND {column} != '' ORDER BY 1"
    ))
    .map(|(_, rows)| rows.into_iter().map(|r| r[0].clone()).collect())
    .unwrap_or_default()
}

/// The table rows — also served bare by `/api/logs` for the tail poller.
#[component]
pub(crate) async fn log_rows(cx: &Cx, rows: Vec<LogRow>) -> Result<impl View> {
    Ok(view! {
        for row in rows {
            <tr>
                <td class="muted">(row.time)</td>
                <td>(row.service)</td>
                <td><span class=(format!("badge {}", sev_class(&row.severity)))>(row.severity)</span></td>
                <td class="logbody">(row.body)</td>
                <td>
                    if !row.trace_id.is_empty() {
                        <a class="trace-link mono" href=(format!("/traces?trace={}", row.trace_id))>"trace"</a>
                    }
                </td>
            </tr>
        }
    })
}

#[page]
async fn logs(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let f = parse_query_params::<LogFilters>(cx).unwrap_or_default();

    let rows = fetch(db, &f).unwrap_or_default();
    let services = option_values(db, "service_name");
    let severities = option_values(db, "severity_text");

    Ok(view! {
        <h1>"Logs"</h1>
        <p class="sub">"Recent log records from the capped store. Tail updates in place."</p>

        <form class="filters" method="get" action="/logs">
            <label>"Search"
                <input type="search" name="q" placeholder="body contains…" value=(f.q.clone()) />
            </label>
            <label>"Service"
                <select name="service">
                    <option value="" if f.service.is_empty() { selected="selected" }>"all"</option>
                    for s in &services {
                        <option value=(s.clone()) if *s == f.service { selected="selected" }>(s.clone())</option>
                    }
                </select>
            </label>
            <label>"Severity"
                <select name="severity">
                    <option value="" if f.severity.is_empty() { selected="selected" }>"all"</option>
                    for s in &severities {
                        <option value=(s.clone()) if *s == f.severity { selected="selected" }>(s.clone())</option>
                    }
                </select>
            </label>
            <label>"Time range"
                <select name="range">
                    <option value="1h" if f.range == "1h" { selected="selected" }>"1 hour"</option>
                    <option value="6h" if f.range == "6h" { selected="selected" }>"6 hours"</option>
                    <option value="24h" if f.range == "24h" { selected="selected" }>"24 hours"</option>
                    <option value="7d" if f.range == "7d" { selected="selected" }>"7 days"</option>
                    <option value="all" if !["1h", "6h", "24h", "7d"].contains(&f.range.as_str()) { selected="selected" }>"all"</option>
                </select>
            </label>
            <button type="submit">"Apply"</button>
            <label class="tail">
                <input type="checkbox" id="tail-toggle" checked="checked" />
                "live tail"
            </label>
        </form>

        <table>
            <thead>
                <tr><th>"time"</th><th>"service"</th><th>"severity"</th><th>"body"</th><th></th></tr>
            </thead>
            <tbody id="logrows">
                log_rows(rows: rows)
            </tbody>
        </table>

        live_refresh(topic: "logs".to_string(), target: "logrows".to_string(), toggle: "tail-toggle".to_string(), cadence: 3000)
    })
}
