//! JSON time-series endpoint for the metric chart: `[xs, ys]` in the shape
//! uPlot consumes, served bare (the `/api/*` layout exception).

use serde::Deserialize;
use serde_json::json;

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{Unescaped, View, view},
};

use crate::db::Db;
use crate::query::NS_TO_SECS;

#[derive(Debug, Default, Deserialize)]
pub struct SeriesParams {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub service: String,
    /// `gauge` | `sum`
    #[serde(default)]
    pub shape: String,
}

/// The series query for the metric chart. The x column is epoch SECONDS
/// (fractional f64) — uPlot's time scale multiplies by 1e3 internally, so
/// feeding it nanosecond/1e6 milliseconds put every chart's x axis in the
/// year ~58 million. The divisor is the shared [`NS_TO_SECS`] so the unit
/// can never drift out of sync with the Explore chart again.
fn series_sql(table: &str) -> String {
    format!(
        "SELECT epoch_ns(time_unix_nano) / {NS_TO_SECS}, COALESCE(double_value, CAST(int_value AS DOUBLE)) \
         FROM otel.main.\"{table}\" \
         WHERE name = ? AND (? = '' OR service_name = ?) \
         ORDER BY time_unix_nano LIMIT 20000"
    )
}

#[page]
async fn series(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let p = parse_query_params::<SeriesParams>(cx).unwrap_or_default();
    let table = match p.shape.as_str() {
        "sum" => "otlp_metrics_sum",
        _ => "otlp_metrics_gauge",
    };

    let body = (|| -> anyhow::Result<String> {
        let (_, rows) = db.query_rows_with(&series_sql(table), &[&p.name, &p.service, &p.service])?;
        let mut xs = Vec::with_capacity(rows.len());
        let mut ys = Vec::with_capacity(rows.len());
        for r in rows {
            xs.push(r[0].parse::<f64>().unwrap_or(0.0));
            ys.push(r[1].parse::<f64>().ok());
        }
        Ok(json!({ "xs": xs, "ys": ys }).to_string())
    })()
    .unwrap_or_else(|e| json!({ "error": e.to_string() }).to_string());

    Ok(view! { (Unescaped::new_unchecked(body)) })
}

#[cfg(test)]
mod tests {
    use super::series_sql;
    use crate::query::NS_TO_SECS;

    #[test]
    fn chart_x_is_epoch_seconds_not_milliseconds() {
        // The unit, pinned: 2026-11-13T00:00:00Z is 1_794_528_000 epoch
        // seconds; the ns stamp the extension stores must divide down to
        // exactly that, not to 1e3× larger milliseconds.
        let ns = 1_794_528_000_000_000_000i128;
        let secs: f64 = NS_TO_SECS.parse().expect("divisor parses");
        assert_eq!(ns as f64 / secs, 1_794_528_000.0);

        // And the SQL must carry that divisor, never the old /1e6.
        let sql = series_sql("otlp_metrics_gauge");
        assert!(
            sql.contains(&format!("epoch_ns(time_unix_nano) / {NS_TO_SECS}")),
            "{sql}"
        );
        assert!(!sql.contains("/ 1000000.0"), "milliseconds leaked back in: {sql}");
    }
}
