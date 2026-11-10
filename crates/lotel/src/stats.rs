//! Shared dashboard/service-map data + tiny server-rendered sparklines.
//!
//! Everything here is either a bounded aggregate query over the trace table or
//! a pure geometry helper, so both features stay JS-free and testable. The
//! span `status_code = 2` (OTLP `STATUS_CODE_ERROR`) is the error signal.

use std::collections::BTreeMap;

use crate::db::Db;

use topcoat::{
    Result,
    context::Cx,
    view::{Unescaped, View, component, view},
};

/// Lower cutoff timestamp for a range label, or `None` for all time.
pub fn cutoff_for(range: &str) -> Option<String> {
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

/// Bucket width that yields a readable sparkline for a range, from a fixed
/// safe set — the interval is never user-supplied text.
pub fn interval_for(range: &str) -> &'static str {
    match range {
        "15m" => "1 minute",
        "1h" => "2 minutes",
        "6h" => "15 minutes",
        "24h" => "1 hour",
        "7d" => "6 hours",
        "30d" => "1 day",
        _ => "1 hour",
    }
}

/// Percent label: `1.8%`, collapsing a whole percent (`2.0%` → `2%`).
pub fn pct(r: f64) -> String {
    let p = r * 100.0;
    if p >= 10.0 || p == p.trunc() {
        format!("{:.0}%", p)
    } else {
        let s = format!("{p:.1}%");
        if s.ends_with(".0%") {
            s.replace(".0%", "%")
        } else {
            s
        }
    }
}

// ——————————————————————————————————————————————— sparkline (pure SVG) ——————

/// Scale a series into a `w`×`h` box. `max` fixes the vertical scale so
/// several sparklines stay comparable; `y` is already SVG-oriented (top = 0).
pub fn spark_points(values: &[f64], w: f64, h: f64, max: f64) -> Vec<(f64, f64)> {
    if values.is_empty() {
        return Vec::new();
    }
    let scale = if max > f64::EPSILON { max } else { 1.0 };
    let pad = 1.0;
    let n = values.len();
    (0..n)
        .map(|i| {
            let x = if n == 1 {
                w / 2.0
            } else {
                (i as f64 / (n - 1) as f64) * w
            };
            let v = (values[i] / scale).clamp(0.0, 1.0);
            let y = h - pad - v * (h - 2.0 * pad);
            (x, y)
        })
        .collect()
}

/// An inline, JS-free sparkline: a filled area under a stroked line. `max`
/// pins the scale (pass the cross-series max to compare shapes); when `max`
/// is `<= 0` the series' own peak is used.
pub fn spark_svg(values: &[f64], w: f64, h: f64, color: &str, max: f64) -> String {
    let pts = spark_points(values, w, h, max);
    if pts.is_empty() {
        return format!(
            "<svg class=\"spark\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\"></svg>"
        );
    }
    let line = pts
        .iter()
        .enumerate()
        .map(|(i, (x, y))| format!("{}{x:.2},{y:.2}", if i == 0 { "M" } else { "L" }))
        .collect::<Vec<_>>()
        .join(" ");
    let (fx, _) = pts[0];
    let (lx, _) = pts[pts.len() - 1];
    let area = format!("{line} L{lx:.2},{h:.2} L{fx:.2},{h:.2} Z");
    format!(
        "<svg class=\"spark\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\" \
         preserveAspectRatio=\"none\"><path class=\"spark-area\" d=\"{area}\" fill=\"{color}\"/> \
         <path class=\"spark-line\" d=\"{line}\" stroke=\"{color}\"/></svg>"
    )
}

// ————————————————————————————————————————————— error-rate time series ——————

/// One service's error-rate series over a range, with window totals.
pub struct SvcSeries {
    pub service: String,
    pub rates: Vec<f64>,
    pub total: i64,
    pub errs: i64,
}

fn rate(total: i64, errs: i64) -> f64 {
    if total == 0 {
        0.0
    } else {
        errs as f64 / total as f64
    }
}

/// Error-rate-per-bucket series per service (and an overall series) over
/// `range`, oldest→newest. One grouped query plus one overall query.
pub fn error_series(db: &Db, range: &str) -> (Vec<f64>, Vec<SvcSeries>) {
    let bucket = interval_for(range);
    let cut = cutoff_for(range);
    let since = |col: &str| -> String {
        match &cut {
            Some(_) => format!("WHERE {col} >= CAST(? AS TIMESTAMP)"),
            None => String::new(),
        }
    };
    let params: Vec<&dyn duckdb::ToSql> = match &cut {
        Some(c) => vec![c as &dyn duckdb::ToSql],
        None => vec![],
    };

    // Overall series (one line per bucket, oldest first).
    let overall_sql = format!(
        "SELECT count(*) AS total, sum(CASE WHEN status_code = 2 THEN 1 ELSE 0 END) AS errs \
         FROM otel.main.otlp_traces {w} \
         GROUP BY time_bucket(INTERVAL '{bucket}', start_time_unix_nano) \
         ORDER BY time_bucket(INTERVAL '{bucket}', start_time_unix_nano)",
        w = since("start_time_unix_nano")
    );
    let overall = db
        .query_rows_with(&overall_sql, &params)
        .unwrap_or_default()
        .1
        .into_iter()
        .filter_map(|r| {
            let t = r.first()?.parse().unwrap_or(0);
            let e = r.get(1).and_then(|v| v.parse().ok()).unwrap_or(0);
            Some(rate(t, e))
        })
        .collect();

    // Per-service series, aligned by bucket index.
    let svc_sql = format!(
        "SELECT service_name, \
                time_bucket(INTERVAL '{bucket}', start_time_unix_nano) AS b, \
                count(*) AS total, \
                sum(CASE WHEN status_code = 2 THEN 1 ELSE 0 END) AS errs \
         FROM otel.main.otlp_traces {w} \
         GROUP BY 1, 2 ORDER BY 1, 2",
        w = since("start_time_unix_nano")
    );
    let grouped = db.query_rows_with(&svc_sql, &params).unwrap_or_default().1;
    let mut per: BTreeMap<String, Vec<(String, i64, i64)>> = BTreeMap::new();
    for r in grouped {
        if r.len() < 4 {
            continue;
        }
        let svc = if r[0].is_empty() {
            "—".to_string()
        } else {
            r[0].clone()
        };
        per.entry(svc).or_default().push((
            r[1].clone(),
            r[2].parse().unwrap_or(0),
            r[3].parse().unwrap_or(0),
        ));
    }
    let mut series: Vec<SvcSeries> = per
        .into_iter()
        .map(|(service, pts)| {
            let total = pts.iter().map(|p| p.1).sum();
            let errs = pts.iter().map(|p| p.2).sum();
            let rates = pts.iter().map(|p| rate(p.1, p.2)).collect();
            SvcSeries {
                service,
                rates,
                total,
                errs,
            }
        })
        .collect();
    // Hottest error rates first, then busiest.
    series.sort_by(|a, b| {
        let ar = rate(a.total, a.errs);
        let br = rate(b.total, b.errs);
        br.partial_cmp(&ar)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.total.cmp(&a.total))
    });
    (overall, series)
}

/// Per-service span volume, error count, and latency percentiles over `range`.
pub struct SvcStat {
    pub service: String,
    pub spans: i64,
    pub errs: i64,
    pub p50_ms: f64,
    pub p95_ms: f64,
}

/// One inter-service call edge (caller `src` → callee `dst`).
pub struct SvcEdge {
    pub src: String,
    pub dst: String,
    pub calls: i64,
    pub errs: i64,
    pub p95_ms: f64,
}

impl SvcEdge {
    pub fn err_rate(&self) -> f64 {
        rate(self.calls, self.errs)
    }
}

/// Node stats (volume / errors / p50 / p95) per service over `range`.
pub fn service_stats(db: &Db, range: &str) -> Vec<SvcStat> {
    let cut = cutoff_for(range);
    let w = if cut.is_some() {
        "WHERE start_time_unix_nano >= CAST(? AS TIMESTAMP)"
    } else {
        ""
    };
    let params: Vec<&dyn duckdb::ToSql> = match &cut {
        Some(c) => vec![c as &dyn duckdb::ToSql],
        None => vec![],
    };
    let sql = format!(
        "SELECT service_name, count(*) AS spans, \
                sum(CASE WHEN status_code = 2 THEN 1 ELSE 0 END) AS errs, \
                quantile_cont(duration_time_unix_nano / 1000000.0, 0.5) AS p50, \
                quantile_cont(duration_time_unix_nano / 1000000.0, 0.95) AS p95 \
         FROM otel.main.otlp_traces {w} GROUP BY 1 ORDER BY 2 DESC"
    );
    db.query_rows_with(&sql, &params)
        .unwrap_or_default()
        .1
        .into_iter()
        .filter(|r| r.len() >= 5)
        .map(|r| SvcStat {
            service: if r[0].is_empty() {
                "—".to_string()
            } else {
                r[0].clone()
            },
            spans: r[1].parse().unwrap_or(0),
            errs: r[2].parse().unwrap_or(0),
            p50_ms: r[3].parse().unwrap_or(0.0),
            p95_ms: r[4].parse().unwrap_or(0.0),
        })
        .collect()
}

/// Inter-service call edges from parent→child spans over `range`. The join is
/// on `(trace_id, parent_span_id) = (trace_id, span_id)`, so an edge is a real
/// observed call; both sides share the same trace.
pub fn service_edges(db: &Db, range: &str) -> Vec<SvcEdge> {
    let cut = cutoff_for(range);
    let w = if cut.is_some() {
        "WHERE c.start_time_unix_nano >= CAST(? AS TIMESTAMP)"
    } else {
        ""
    };
    let params: Vec<&dyn duckdb::ToSql> = match &cut {
        Some(c) => vec![c as &dyn duckdb::ToSql],
        None => vec![],
    };
    let sql = format!(
        "SELECT p.service_name AS src, c.service_name AS dst, count(*) AS calls, \
                sum(CASE WHEN c.status_code = 2 THEN 1 ELSE 0 END) AS errs, \
                quantile_cont(c.duration_time_unix_nano / 1000000.0, 0.95) AS p95 \
         FROM otel.main.otlp_traces c \
         JOIN otel.main.otlp_traces p \
           ON c.trace_id = p.trace_id AND c.parent_span_id = p.span_id \
         {w} GROUP BY 1, 2 ORDER BY 3 DESC"
    );
    db.query_rows_with(&sql, &params)
        .unwrap_or_default()
        .1
        .into_iter()
        .filter(|r| r.len() >= 5 && r[0] != r[1])
        .map(|r| SvcEdge {
            src: r[0].clone(),
            dst: r[1].clone(),
            calls: r[2].parse().unwrap_or(0),
            errs: r[3].parse().unwrap_or(0),
            p95_ms: r[4].parse().unwrap_or(0.0),
        })
        .collect()
}

/// A pre-rendered error-rate row (owned, so the boxed view can move it in).
pub(crate) struct ErrRow {
    service: String,
    href: String,
    spark: String,
    rate: String,
    hi: bool,
}

/// Owned render input for the dashboard error-rate card.
pub(crate) struct ErrInput {
    now_pct: String,
    overall_pct: String,
    empty: bool,
    big_spark: String,
    rows: Vec<ErrRow>,
    foot: String,
}

/// Compute the dashboard error-rate card over `range` — window totals, the
/// overall trend, and the top services. Shared by the home page and the
/// `/api/live?topic=err` fragment so the two never drift.
pub(crate) fn build_err(db: &Db, range: &str) -> ErrInput {
    let (overall_rates, svc_rates) = error_series(db, range);
    let rate = |t: i64, e: i64| if t == 0 { 0.0 } else { e as f64 / t as f64 };
    let w_spans: i64 = svc_rates.iter().map(|s| s.total).sum();
    let w_errs: i64 = svc_rates.iter().map(|s| s.errs).sum();
    // Shared scale so every sparkline is comparable; floor keeps them visible.
    let peak = overall_rates
        .iter()
        .chain(svc_rates.iter().flat_map(|s| s.rates.iter()))
        .cloned()
        .fold(0.0f64, f64::max)
        .max(0.02);
    let big_spark = spark_svg(&overall_rates, 280.0, 46.0, "#f0858a", peak);
    let rows: Vec<ErrRow> = svc_rates
        .iter()
        .take(6)
        .map(|s| {
            let r = rate(s.total, s.errs);
            ErrRow {
                service: s.service.clone(),
                href: format!("/traces?service={}", s.service),
                spark: spark_svg(&s.rates, 120.0, 22.0, "#f0858a", peak),
                rate: pct(r),
                hi: r >= 0.02,
            }
        })
        .collect();
    ErrInput {
        now_pct: pct(*overall_rates.last().unwrap_or(&0.0)),
        overall_pct: pct(rate(w_spans, w_errs)),
        empty: w_spans == 0,
        big_spark,
        rows,
        foot: format!("{w_errs} errored of {w_spans} spans · sparklines share one scale"),
    }
}

/// The dashboard error-rate card. Rendered identically by the home page and the
/// `/api/live?topic=err` poll fragment.
#[component]
pub(crate) async fn err_card(cx: &Cx, input: ErrInput) -> Result<impl View> {
    let ErrInput {
        now_pct,
        overall_pct,
        empty,
        big_spark,
        rows,
        foot,
    } = input;
    Ok(view! {
        <div class="card errcard">
            <div class="err-top">
                <div class="err-now">
                    <span class="err-big">(now_pct)</span>
                    <span class="muted">"now · " (overall_pct)" over the window"</span>
                </div>
                <div class="err-spark">(Unescaped::new_unchecked(big_spark))</div>
            </div>
            if empty {
                <p class="muted">"No spans in the last hour."</p>
            } else {
                <div class="err-svc-list">
                    for e in rows {
                        <a class="err-svc" href=(e.href)>
                            <span class="err-svc-name">(e.service)</span>
                            <span class="err-svc-spark">(Unescaped::new_unchecked(e.spark))</span>
                            <span class=(if e.hi { "err-svc-rate hi" } else { "err-svc-rate" })>
                                (e.rate)
                            </span>
                        </a>
                    }
                </div>
                <p class="muted err-foot">(foot)</p>
            }
        </div>
    })
}

#[cfg(test)]
mod tests {
    use super::{pct, spark_points, spark_svg};

    #[test]
    fn percent_format_collapses_wholes() {
        assert_eq!(pct(0.0), "0%");
        assert_eq!(pct(0.018), "1.8%");
        assert_eq!(pct(0.02), "2%");
        assert_eq!(pct(0.5), "50%");
        assert_eq!(pct(1.0), "100%");
    }

    #[test]
    fn sparkline_maps_endpoints_and_peaks() {
        // 3 rising points fill the box bottom→top with the peak at the top pad.
        let pts = spark_points(&[0.0, 0.5, 1.0], 100.0, 40.0, 1.0);
        assert_eq!(pts.len(), 3);
        assert_eq!(pts[0].0, 0.0);
        assert_eq!(pts[2].0, 100.0);
        assert!(pts[2].1 < pts[0].1, "peak (higher value) sits above trough");
        assert!(pts[0].1 <= 39.0 + f64::EPSILON);
    }

    #[test]
    fn sparkline_shared_scale_and_empty() {
        // Same 0..1 values under a shared max of 2 sit in the lower half.
        let low = spark_points(&[1.0], 100.0, 40.0, 2.0)[0].1;
        let high = spark_points(&[1.0], 100.0, 40.0, 1.0)[0].1;
        assert!(low > high);
        let empty = spark_svg(&[], 10.0, 10.0, "#f00", 0.0);
        assert!(empty.starts_with("<svg") && empty.ends_with("</svg>"));
        assert!(!empty.contains("path"));
    }

    #[test]
    fn sparkline_emits_area_and_line() {
        let s = spark_svg(&[0.1, 0.4, 0.2], 120.0, 30.0, "#f0858a", 0.5);
        assert!(s.contains("spark-area"));
        assert!(s.contains("spark-line"));
        assert!(s.contains("#f0858a"));
    }
}
