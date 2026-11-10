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

#[page]
async fn series(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let p = parse_query_params::<SeriesParams>(cx).unwrap_or_default();
    let table = match p.shape.as_str() {
        "sum" => "otlp_metrics_sum",
        _ => "otlp_metrics_gauge",
    };

    let body = (|| -> anyhow::Result<String> {
        let (_, rows) = db.query_rows_with(
            &format!(
                "SELECT epoch_ns(time_unix_nano) / 1000000.0, COALESCE(double_value, CAST(int_value AS DOUBLE)) \
                 FROM otel.main.\"{table}\" \
                 WHERE name = ? AND (? = '' OR service_name = ?) \
                 ORDER BY time_unix_nano LIMIT 20000"
            ),
            &[&p.name, &p.service, &p.service],
        )?;
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
