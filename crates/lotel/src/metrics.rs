//! Metrics: the catalog across all four shapes, a uPlot line chart per
//! (shape, name, service) for gauge/sum, and a server-rendered distribution
//! (CSS bars) for histogram/exp-histogram. The charting JS is vendored in.

use std::collections::BTreeMap;

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
    /// `gauge` | `sum` | `histogram` | `exp_histogram`.
    #[serde(default)]
    pub shape: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub service: String,
    /// `1` renders the chart/distribution view instead of the catalog.
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

fn is_hist(shape: &str) -> bool {
    shape == "histogram" || shape == "exp_histogram"
}

/// Badge class per shape (matches the `.badge.*` palette in ui.rs).
fn shape_badge(shape: &str) -> &'static str {
    match shape {
        "gauge" => "debug",
        "sum" => "info",
        "histogram" => "hist",
        "exp_histogram" => "exp",
        _ => "other",
    }
}

fn table_for(shape: &str) -> &'static str {
    match shape {
        "sum" => "otlp_metrics_sum",
        "histogram" => "otlp_metrics_histogram",
        "exp_histogram" => "otlp_metrics_exp_histogram",
        _ => "otlp_metrics_gauge",
    }
}

/// Parse DuckDB's VARCHAR rendering of a list — `[1, 2, 3]` — into i64s.
fn parse_i64_list(s: &str) -> Vec<i64> {
    s.trim()
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .map(|inner| {
            inner
                .split(',')
                .map(|x| x.trim())
                .filter(|x| !x.is_empty())
                .map(|x| x.parse::<i64>().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
}

/// Same for a DOUBLE list — `[5.0, 10.0]`.
fn parse_f64_list(s: &str) -> Vec<f64> {
    s.trim()
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .map(|inner| {
            inner
                .split(',')
                .map(|x| x.trim())
                .filter(|x| !x.is_empty())
                .map(|x| x.parse::<f64>().unwrap_or(0.0))
                .collect()
        })
        .unwrap_or_default()
}

/// Compact number: integer-valued floats print as ints, the rest at 3 sig figs.
fn fnum(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        let mut s = format!("{x:.3}");
        while s.ends_with('0') {
            s.pop();
        }
        s.strip_suffix('.').unwrap_or(&s).to_string()
    }
}

/// One histogram bar: a bucket range label and its summed count.
pub struct Bar {
    pub label: String,
    pub count: i64,
}

/// OTel histogram buckets: `counts.len() == bounds.len() + 1`; bucket 0 is
/// (-inf, b0], bucket i is (b[i-1], b[i]], the last is (b[n-1], +inf).
fn regular_bars(counts: &[i64], bounds: &[f64]) -> Vec<Bar> {
    let n = bounds.len();
    if counts.is_empty() {
        return Vec::new();
    }
    (0..counts.len())
        .map(|i| {
            let label = if n == 0 {
                "all".to_string()
            } else if i == 0 {
                format!("\u{2264} {}", fnum(bounds[0]))
            } else if i == n {
                format!("> {}", fnum(bounds[n - 1]))
            } else {
                format!("{} \u{2013} {}", fnum(bounds[i - 1]), fnum(bounds[i]))
            };
            Bar {
                label,
                count: counts[i],
            }
        })
        .collect()
}

/// Aggregate OTel histogram rows: sum bucket counts element-wise (bounds are
/// stable per metric, so position is a safe key); take bounds from the widest.
fn aggregate_regular(rows: &[(Vec<i64>, Vec<f64>)]) -> Vec<Bar> {
    let bounds = rows
        .iter()
        .map(|(_, b)| b.as_slice())
        .max_by_key(|b| b.len())
        .unwrap_or(&[])
        .to_vec();
    let n = bounds.len() + 1;
    let mut counts = vec![0i64; n];
    for (c, _) in rows {
        for (i, v) in c.iter().enumerate().take(n) {
            counts[i] += v;
        }
    }
    regular_bars(&counts, &bounds)
}

/// One exponential-histogram row: positive offset, positive counts, negative
/// offset, negative counts, zero count.
type ExpRow = (i64, Vec<i64>, i64, Vec<i64>, i64);

/// Aggregate exponential-histogram rows by absolute bucket index. Positive
/// bucket k in a row has absolute exponent `offset + k`; keying on that lets
/// rows with different offsets share the sealer. Negative buckets are keyed
/// negative; index -1 is reserved for the zero bucket.
fn aggregate_exp(rows: &[ExpRow], scale: i64) -> Vec<Bar> {
    let base = 2f64.powf(2f64.powi(-scale as i32));
    let mut pos: BTreeMap<i64, i64> = BTreeMap::new();
    let mut neg: BTreeMap<i64, i64> = BTreeMap::new();
    let mut zero = 0i64;
    for (po, pc, no, nc, z) in rows {
        zero += z;
        for (k, v) in pc.iter().enumerate() {
            *pos.entry(po + k as i64).or_default() += v;
        }
        for (k, v) in nc.iter().enumerate() {
            *neg.entry(no + k as i64).or_default() += v;
        }
    }
    let edge = |e: i64| base.powi(e as i32);
    let mut bars = Vec::new();
    // Most-negative bucket first (descending exponent), then zero, then ascending.
    for (e, count) in neg.iter().rev() {
        bars.push(Bar {
            label: format!("[{}, {})", fnum(-edge(e + 1)), fnum(-edge(*e))),
            count: *count,
        });
    }
    if zero > 0 {
        bars.push(Bar {
            label: "\u{2248} 0".to_string(),
            count: zero,
        });
    }
    for (e, count) in &pos {
        bars.push(Bar {
            label: format!("({}, {}]", fnum(edge(e - 1)), fnum(edge(*e))),
            count: *count,
        });
    }
    bars
}

#[page]
async fn metrics(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let f = parse_query_params::<MetricFilters>(cx).unwrap_or_default();

    if !f.name.is_empty() && f.chart == "1" {
        return if is_hist(&f.shape) {
            distribution(cx, db, f.name.clone(), f.service.clone(), f.shape.clone()).await
        } else {
            chart(cx, db, f.name.clone(), f.service.clone(), f.shape.clone()).await
        };
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
                UNION ALL
                SELECT 'histogram', name, unit, service_name,
                       count(*), max(time_unix_nano), sum(\"sum\") / nullif(sum(\"count\"), 0)
                FROM otel.main.otlp_metrics_histogram GROUP BY name, unit, service_name
                UNION ALL
                SELECT 'exp_histogram', name, unit, service_name,
                       count(*), max(time_unix_nano), sum(\"sum\") / nullif(sum(\"count\"), 0)
                FROM otel.main.otlp_metrics_exp_histogram GROUP BY name, unit, service_name
             ) ORDER BY latest DESC NULLS LAST",
        )
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));

    Ok(view! {
        <h1>"Metrics"</h1>
        <p class="sub">"All four metric shapes as ingested. Click a metric to chart it (gauge/sum → time series, histogram → distribution)."</p>

        <table>
            <thead>
                <tr><th>"shape"</th><th>"metric"</th><th>"unit"</th><th>"service"</th><th>"points"</th><th>"latest"</th><th>"example"</th></tr>
            </thead>
            <tbody>
                for row in &catalog.1 {
                    <tr>
                        if row.len() == 7 {
                            <td><span class=(format!("badge {}", shape_badge(&row[0])))>(row[0].clone())</span></td>
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

/// Line chart for gauge/sum, fed by /api/metrics/series.
async fn chart<'a>(
    cx: &'a Cx,
    db: &Db,
    name: String,
    service: String,
    shape: String,
) -> Result<BoxView<'a>> {
    let table = table_for(&shape);
    let stats = db
        .query_rows_with(
            &format!(
                "SELECT count(*), min(time_unix_nano), max(time_unix_nano), any_value(unit) \
                 FROM otel.main.\"{table}\" \
                 WHERE name = ? AND (? = '' OR service_name = ?)"
            ),
            &[&name, &service, &service],
        )
        .unwrap_or_else(|e| {
            (
                vec!["error".into()],
                vec![vec![format!("{e}"), "?".into(), "?".into(), "?".into()]],
            )
        });
    let s = stats
        .1
        .first()
        .cloned()
        .unwrap_or_else(|| vec!["0".into(), "?".into(), "?".into(), "?".into()]);

    let label = if service.is_empty() {
        name.clone()
    } else {
        format!("{name} · {service}")
    };
    let params = json!({ "name": name, "service": service, "shape": shape, "label": label });
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
        <h1>(name.clone())</h1>
        <p class="sub">
            <span class=(format!("badge {}", shape_badge(&shape)))>(shape.clone())</span>
            " "
            if !s[3].is_empty() && s[3] != "?" {
                <span class="badge other">(s[3].clone())</span>
            }
            " "
            if !service.is_empty() {
                <span class="badge trace">(service.clone())</span>
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

/// Distribution view for histogram/exp_histogram: aggregated bucket bars.
async fn distribution<'a>(
    cx: &'a Cx,
    db: &Db,
    name: String,
    service: String,
    shape: String,
) -> Result<BoxView<'a>> {
    let table = table_for(&shape);
    // Header stats (window, sample total, mean, min, max, unit).
    let stats = db
        .query_rows_with(
            &format!(
                "SELECT count(*), min(time_unix_nano), max(time_unix_nano), any_value(unit), \
                        sum(\"count\"), sum(\"sum\") / nullif(sum(\"count\"), 0), min(\"min\"), max(\"max\") \
                 FROM otel.main.\"{table}\" WHERE name = ? AND (? = '' OR service_name = ?)"
            ),
            &[&name, &service, &service],
        )
        .unwrap_or_default();
    let s = stats
        .1
        .first()
        .cloned()
        .unwrap_or_else(|| vec!["0".into(); 8]);

    // Fetch the raw bucket columns and aggregate the bars in Rust.
    let bars = if shape == "histogram" {
        let rows = db
            .query_rows_with(
                &format!(
                    "SELECT bucket_counts, explicit_bounds FROM otel.main.\"{table}\" \
                     WHERE name = ? AND (? = '' OR service_name = ?)"
                ),
                &[&name, &service, &service],
            )
            .unwrap_or_default()
            .1
            .into_iter()
            .map(|r| (parse_i64_list(&r[0]), parse_f64_list(&r[1])))
            .collect::<Vec<_>>();
        aggregate_regular(&rows)
    } else {
        let rows = db
            .query_rows_with(
                &format!(
                    "SELECT positive_offset, positive_bucket_counts, negative_offset, \
                            negative_bucket_counts, COALESCE(zero_count, 0) \
                 FROM otel.main.\"{table}\" WHERE name = ? AND (? = '' OR service_name = ?)"
                ),
                &[&name, &service, &service],
            )
            .unwrap_or_default()
            .1
            .into_iter()
            .map(|r| {
                (
                    r[0].parse::<i64>().unwrap_or(0),
                    parse_i64_list(&r[1]),
                    r[2].parse::<i64>().unwrap_or(0),
                    parse_i64_list(&r[3]),
                    r[4].parse::<i64>().unwrap_or(0),
                )
            })
            .collect::<Vec<_>>();
        let scale = db
            .query_rows_with(
                &format!(
                    "SELECT any_value(scale) FROM otel.main.\"{table}\" WHERE name = ? LIMIT 1"
                ),
                &[&name],
            )
            .unwrap_or_default()
            .1
            .first()
            .and_then(|r| r[0].parse::<i64>().ok())
            .unwrap_or(0);
        aggregate_exp(&rows, scale)
    };
    let max = bars.iter().map(|b| b.count).max().unwrap_or(0).max(1);

    Ok(view! {
        <h1>(name.clone())</h1>
        <p class="sub">
            <span class=(format!("badge {}", shape_badge(&shape)))>(shape.clone())</span>
            " "
            if !s[3].is_empty() && s[3] != "?" {
                <span class="badge other">(s[3].clone())</span>
            }
            " "
            if !service.is_empty() {
                <span class="badge trace">(service.clone())</span>
            }
        </p>
        <p class="muted">
            (s[0].clone())" points · "
            (s[4].clone())" samples · mean "
            (if s[5].is_empty() { "—".to_string() } else { s[5].clone() })
            " · min "
            (if s[6].is_empty() { "—".to_string() } else { s[6].clone() })
            " · max "
            (if s[7].is_empty() { "—".to_string() } else { s[7].clone() })
        </p>
        <p class="muted">"window "(s[1].clone())" → "(s[2].clone())" · aggregated across all label sets"</p>
        <p><a class="trace-link" href="/metrics">"← all metrics"</a></p>

        if bars.is_empty() {
            <p class="muted">"No buckets for this metric (or it has aged out)."</p>
        } else {
            <div class="card">
                <div class="hist">
                    for b in &bars {
                        <div class="hrow">
                            <div class="hlabel mono">(b.label.clone())</div>
                            <div class="htrack">
                                <div class="hbar" style=(format!("width:{}%", (b.count as f64 / max as f64) * 100.0))></div>
                                <span class="hcount">(b.count.to_string())</span>
                            </div>
                        </div>
                    }
                </div>
            </div>
        }
    }
    .boxed())
}

#[cfg(test)]
mod tests {
    use super::{aggregate_exp, aggregate_regular, fnum, parse_f64_list, parse_i64_list, urlenc};

    #[test]
    fn encodes_query_values() {
        assert_eq!(urlenc("checkout.queue.depth"), "checkout.queue.depth");
        assert_eq!(urlenc("http.method=POST"), "http.method%3DPOST");
        assert_eq!(urlenc("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlenc(""), "");
    }

    #[test]
    fn parses_duckdb_lists() {
        assert_eq!(parse_i64_list("[1, 2, 3]"), vec![1, 2, 3]);
        assert_eq!(parse_i64_list("[]"), Vec::<i64>::new());
        assert_eq!(parse_f64_list("[5.0, 10.0, 50.0]"), vec![5.0, 10.0, 50.0]);
        assert_eq!(parse_f64_list(""), Vec::<f64>::new());
    }

    #[test]
    fn compacts_numbers() {
        assert_eq!(fnum(5.0), "5");
        assert_eq!(fnum(120.0), "120");
        assert_eq!(fnum(1.25), "1.25");
        assert_eq!(fnum(-0.0), "0");
    }

    #[test]
    fn histogram_bucket_labels() {
        let bars =
            aggregate_regular(&[(vec![120, 480, 300, 80, 20], vec![5.0, 10.0, 50.0, 100.0])]);
        assert_eq!(bars.len(), 5);
        assert_eq!(bars[0].label, "≤ 5");
        assert_eq!(bars[1].label, "5 – 10");
        assert_eq!(bars[4].label, "> 100");
        assert_eq!(bars[1].count, 480);
    }

    #[test]
    fn histogram_aggregates_across_points() {
        let bars = aggregate_regular(&[
            (vec![1, 2, 3], vec![5.0, 10.0]),
            (vec![10, 20, 30], vec![5.0, 10.0]),
        ]);
        assert_eq!(
            bars.iter().map(|b| b.count).collect::<Vec<_>>(),
            vec![11, 22, 33]
        );
    }

    #[test]
    fn exp_histogram_reconstructs_exponential_edges() {
        // scale 2 → base = 2^(1/4) ≈ 1.1892; offset 0, positive counts [60,140].
        let bars = aggregate_exp(&[(0, vec![60, 140], 0, vec![], 40)], 2);
        // zero bucket + 2 positive buckets.
        assert_eq!(bars.len(), 3);
        assert_eq!(bars[0].count, 40); // ≈ 0
        assert_eq!(bars[2].count, 140); // highest positive bucket
        assert!(bars[2].label.contains(']'));
    }
}
