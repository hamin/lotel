//! Metrics: the metric catalog across gauge + sum tables.

use topcoat::{
    Result,
    context::{Cx, context},
    router::page,
    view::{View, view},
};

use crate::db::Db;

#[page]
async fn metrics(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);

    let catalog = db
        .query_rows(
            "SELECT shape, name, unit, service_name, points, latest, example FROM (
                SELECT 'gauge' AS shape, name, unit, service_name,
                       count(*) AS points, max(time_unix_nano) AS latest,
                       max(COALESCE(double_value, int_value::DOUBLE)) AS example
                FROM otel.main.otlp_metrics_gauge GROUP BY name, unit, service_name
                UNION ALL
                SELECT 'sum', name, unit, service_name,
                       count(*), max(time_unix_nano),
                       max(COALESCE(double_value, int_value::DOUBLE))
                FROM otel.main.otlp_metrics_sum GROUP BY name, unit, service_name
             ) ORDER BY latest DESC NULLS LAST",
        )
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));

    Ok(view! {
        <h1>"Metrics"</h1>
        <p class="sub">"Gauges and sums as ingested (histograms land in their own segments; charts arrive in M4)."</p>

        <table>
            <thead>
                <tr><th>"shape"</th><th>"metric"</th><th>"unit"</th><th>"service"</th><th>"points"</th><th>"latest"</th><th>"example"</th></tr>
            </thead>
            <tbody>
                for row in &catalog.1 {
                    <tr>
                        for cell in row {
                            <td>(cell.clone())</td>
                        }
                    </tr>
                }
            </tbody>
        </table>
    })
}
