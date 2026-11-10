//! Metrics: the metric catalog across gauge + sum tables, and a uPlot chart
//! per (shape, name, service) with the JS vendored into the binary.

use serde::Deserialize;
use serde_json::json;

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{BoxView, Unescaped, ViewExt, view},
};

use crate::db::Db;

/// Vendored at build time — the binary carries its own charting library.
const UPLOT_JS: &str = include_str!("../assets/vendor/uPlot.min.js");
const UPLOT_CSS: &str = include_str!("../assets/vendor/uPlot.min.css");

#[derive(Debug, Default, Deserialize)]
pub struct MetricFilters {
    /// `gauge` | `sum` — set together with `name` when charting.
    #[serde(default)]
    pub shape: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub service: String,
    /// `1` renders the chart view instead of the catalog.
    #[serde(default)]
    pub chart: String,
}

/// Percent-encode for query strings (metric names carry dots and slashes).
fn urlenc(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::urlenc;

    #[test]
    fn encodes_query_values() {
        assert_eq!(urlenc("checkout.queue.depth"), "checkout.queue.depth");
        assert_eq!(urlenc("http.method=POST"), "http.method%3DPOST");
        assert_eq!(urlenc("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlenc(""), "");
    }
}

fn table_for(shape: &str) -> &'static str {
    match shape {
        "sum" => "otlp_metrics_sum",
        _ => "otlp_metrics_gauge",
    }
}

#[page]
async fn metrics(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let f = parse_query_params::<MetricFilters>(cx).unwrap_or_default();

    if !f.name.is_empty() && f.chart == "1" {
        return chart(cx, db, f.name.clone(), f.service.clone(), f.shape.clone()).await;
    }

    let catalog = db
        .query_rows(
            "SELECT shape, name, unit, service_name, points, latest, example FROM (
                SELECT 'gauge' AS shape, name, unit, service_name,
                       count(*) AS points, max(time_unix_nano) AS latest,
                       max(COALESCE(double_value, CAST(int_value AS DOUBLE))) AS example
                FROM otel.main.otlp_metrics_gauge GROUP BY name, unit, service_name
                UNION ALL
                SELECT 'sum', name, unit, service_name,
                       count(*), max(time_unix_nano),
                       max(COALESCE(double_value, CAST(int_value AS DOUBLE)))
                FROM otel.main.otlp_metrics_sum GROUP BY name, unit, service_name
             ) ORDER BY latest DESC NULLS LAST",
        )
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));

    Ok(view! {
        <h1>"Metrics"</h1>
        <p class="sub">"Gauges and sums as ingested (histograms land in their own segments; click a metric to chart it)."</p>

        <table>
            <thead>
                <tr><th>"shape"</th><th>"metric"</th><th>"unit"</th><th>"service"</th><th>"points"</th><th>"latest"</th><th>"example"</th></tr>
            </thead>
            <tbody>
                for row in &catalog.1 {
                    <tr>
                        if row.len() == 7 {
                            <td><span class=(format!("badge {}", if row[0] == "gauge" { "debug" } else { "info" }))>(row[0].clone())</span></td>
                            <td class="logbody">
                                <a class="trace-link" href=(format!("/metrics?shape={}&name={}&service={}&chart=1", urlenc(&row[0]), urlenc(&row[1]), urlenc(&row[3])))>(row[1].clone())</a>
                            </td>
                            <td>(row[2].clone())</td>
                            <td>(row[3].clone())</td>
                            <td>(row[4].clone())</td>
                            <td class="muted">(row[5].clone())</td>
                            <td>(row[6].clone())</td>
                        } else {
                            <td colspan="7" class="muted">(row.join(" · "))</td>
                        }
                    </tr>
                }
            </tbody>
        </table>
    }
    .boxed())
}

/// Chart view: header stats + a uPlot line chart fed by /api/metrics/series.
async fn chart<'a>(cx: &'a Cx, db: &Db, name: String, service: String, shape: String) -> Result<BoxView<'a>> {
    let f = MetricFilters { shape, name, service, chart: String::new() };
    let table = table_for(&f.shape);
    let stats = db
        .query_rows_with(
            &format!(
                "SELECT count(*), min(time_unix_nano), max(time_unix_nano), any_value(unit) \
                 FROM otel.main.\"{table}\" \
                 WHERE name = ? AND (? = '' OR service_name = ?)"
            ),
            &[&f.name, &f.service, &f.service],
        )
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}"), "?".into(), "?".into(), "?".into()]]));
    let s = stats.1.first().cloned().unwrap_or_else(|| vec!["0".into(), "?".into(), "?".into(), "?".into()]);

    let label = if f.service.is_empty() {
        f.name.clone()
    } else {
        format!("{} · {}", f.name, f.service)
    };
    let params = json!({ "name": f.name, "service": f.service, "shape": f.shape, "label": label });
    let init = format!(
        r#"
(function () {{
    var el = document.getElementById('chart');
    if (!el) return;
    var P = {params};
    fetch('/api/metrics/series?name=' + encodeURIComponent(P.name)
          + '&service=' + encodeURIComponent(P.service) + '&shape=' + encodeURIComponent(P.shape))
        .then(function (r) {{ return r.json(); }})
        .then(function (d) {{
            if (!d.xs || !d.xs.length) {{ el.textContent = 'no points'; return; }}
            new uPlot({{
                width: Math.max(el.clientWidth, 320),
                height: 280,
                padding: [6, 6, 0, 0],
                scales: {{ x: {{ time: true }} }},
                series: [
                    {{ label: 'time' }},
                    {{ label: P.label, stroke: '#7dd3a0', fill: 'rgba(125, 211, 160, 0.10)', width: 1.5 }}
                ],
                axes: [
                    {{ stroke: '#8a93a5', grid: {{ stroke: '#1b212e' }} }},
                    {{ stroke: '#8a93a5', grid: {{ stroke: '#1b212e' }} }}
                ],
                cursor: {{ points: {{ show: false }} }}
            }}, [d.xs, d.ys], el);
        }});
}})();
"#
    );

    Ok(view! {
        <h1>(f.name.clone())</h1>
        <p class="sub">
            <span class=(format!("badge {}", if f.shape == "gauge" { "debug" } else { "info" }))>(f.shape.clone())</span>
            " "
            if !s[3].is_empty() && s[3] != "?" {
                <span class="badge other">(s[3].clone())</span>
            }
            " "
            if !f.service.is_empty() {
                <span class="badge trace">(f.service.clone())</span>
            }
        </p>
        <p class="muted">
            (s[0].clone())" points · window "
            (s[1].clone())" → "(s[2].clone())
            " · raw points across all label sets"
        </p>
        <p><a class="trace-link" href="/metrics">"← all metrics"</a></p>

        <div class="card"><div id="chart"></div></div>

        <style>(Unescaped::new_unchecked(UPLOT_CSS))</style>
        <script>(Unescaped::new_unchecked(UPLOT_JS))</script>
        <script>(Unescaped::new_unchecked(init))</script>
    }
    .boxed())
}
