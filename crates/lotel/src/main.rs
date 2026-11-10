mod api;
mod config;
mod db;
mod ingest;
mod logs;
mod metrics;
mod query;
mod services;
mod stats;
mod storage;
mod traces;
mod ui;

use topcoat::{
    Result,
    context::{Cx, context},
    router::{Slot, layout, module_router, page},
    view::{BoxView, Unescaped, View, ViewExt, view},
};

use crate::{
    db::Db,
    ingest::Ingest,
    ui::{CSS, nav_links},
};

/// A pre-rendered error-rate row (owned, so the boxed view can move it in).
struct ErrRow {
    service: String,
    href: String,
    spark: String,
    rate: String,
    hi: bool,
}

/// The chrome every page shares; declared at the crate root so it wraps all
/// module-routed pages.
#[layout]
async fn root_layout(cx: &Cx, slot: Slot<'_>) -> Result<BoxView<'_>> {
    let path = topcoat::router::request::uri(cx).path().to_owned();

    // Fragment endpoints (the live tail) render bare: the JS swaps their
    // output into an existing page, so no chrome.
    if path.starts_with("/api/") {
        return Ok(view! { (slot) }.boxed());
    }

    let active: &'static str = match path.as_str() {
        "/" => "Dashboard",
        "/logs" => "Logs",
        "/traces" => "Traces",
        "/metrics" => "Metrics",
        "/query" => "Explore",
        "/services" => "Services",
        "/storage" => "Storage",
        _ => "",
    };

    Ok(view! {
        <!DOCTYPE html>
        <html>
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <title>"lotel — local OpenTelemetry"</title>
                <style>(Unescaped::new_unchecked(CSS))</style>
            </head>
            <body>
                <div class="layout">
                    <nav>
                        <p class="brand">"lotel" <span class="dot">"●"</span></p>
                        for (label, href) in nav_links() {
                            <a href=(href)
                               if label == active {
                                   aria-current="page"
                               }
                            >
                                (label)
                            </a>
                        }
                    </nav>
                    <main>(slot)</main>
                </div>
            </body>
        </html>
    }
    .boxed())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = config::Config::from_env()?;
    let db = Db::open(&cfg)?;
    let ing = ingest::start(&db, &cfg, &db.ext_dir)?;

    println!(
        "lotel: duckdb {} · capped {} · store {}",
        db.duckdb_version()?,
        db.capped_version,
        db.data_dir_display()
    );
    println!("lotel: OTLP on {} (bearer token: {})", ing.listen_uri, ing.token);
    println!("lotel: UI on http://{}:{}", cfg.host, cfg.port);

    let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
    topcoat::serve(listener, module_router!().with(db).with(ing).build()).await?;
    Ok(())
}

#[page]
async fn home(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let ing = context::<Ingest>(cx);

    let counts = db
        .query_rows(
            "SELECT 'logs' AS signal, count(*) AS rows FROM otel.main.otlp_logs
             UNION ALL SELECT 'traces', count(*) FROM otel.main.otlp_traces
             UNION ALL SELECT 'gauge', count(*) FROM otel.main.otlp_metrics_gauge
             UNION ALL SELECT 'sum', count(*) FROM otel.main.otlp_metrics_sum
             UNION ALL SELECT 'histogram', count(*) FROM otel.main.otlp_metrics_histogram",
        )
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));
    let store = db
        .query_rows(
            "SELECT table_name, segments, sealed, rows, bytes FROM capped_tables('otel')",
        )
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));
    let otlp = ing
        .stats(db)
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));

    // Error-rate trend over the last hour (traces), for the dashboard sparkline.
    let (overall_rates, svc_rates) = stats::error_series(db, "1h");
    let rate = |t: i64, e: i64| if t == 0 { 0.0 } else { e as f64 / t as f64 };
    let w_spans: i64 = svc_rates.iter().map(|s| s.total).sum();
    let w_errs: i64 = svc_rates.iter().map(|s| s.errs).sum();
    let overall_rate = rate(w_spans, w_errs);
    let cur_rate = overall_rates.last().copied().unwrap_or(0.0);
    let peak = overall_rates
        .iter()
        .chain(svc_rates.iter().flat_map(|s| s.rates.iter()))
        .cloned()
        .fold(0.0f64, f64::max)
        .max(0.02);
    let big_spark = stats::spark_svg(&overall_rates, 280.0, 46.0, "#f0858a", peak);
    let err_rows: Vec<ErrRow> = svc_rates
        .iter()
        .take(6)
        .map(|s| {
            let r = rate(s.total, s.errs);
            ErrRow {
                service: s.service.clone(),
                href: format!("/traces?service={}", s.service),
                spark: stats::spark_svg(&s.rates, 120.0, 22.0, "#f0858a", peak),
                rate: stats::pct(r),
                hi: r >= 0.02,
            }
        })
        .collect();

    Ok(view! {
        <h1>"Dashboard"</h1>
        <p class="sub">
            "Local OpenTelemetry on capped DuckDB storage — telemetry that stays, then is forgotten on schedule."
        </p>

        <div class="grid">
            for row in &counts.1 {
                <div class="card stat">
                    <div class="n">(row[1].clone())</div>
                    <div class="l">(row[0].clone())</div>
                </div>
            }
        </div>

        <h2>"Error rate — traces · last hour"</h2>
        <div class="card errcard">
            <div class="err-top">
                <div class="err-now">
                    <span class="err-big">(stats::pct(cur_rate))</span>
                    <span class="muted">"now · " (stats::pct(overall_rate))" over the window"</span>
                </div>
                <div class="err-spark">(Unescaped::new_unchecked(big_spark))</div>
            </div>
            if w_spans == 0 {
                <p class="muted">"No spans in the last hour."</p>
            } else {
                <div class="err-svc-list">
                    for e in err_rows {
                        <a class="err-svc" href=(e.href)>
                            <span class="err-svc-name">(e.service)</span>
                            <span class="err-svc-spark">(Unescaped::new_unchecked(e.spark))</span>
                            <span class=(if e.hi { "err-svc-rate hi" } else { "err-svc-rate" })>
                                (e.rate)
                            </span>
                        </a>
                    }
                </div>
                <p class="muted err-foot">
                    (w_errs.to_string())" errored of "(w_spans.to_string())" spans · sparklines share one scale"
                </p>
            }
        </div>

        <h2>"Ingest — otlp_serve"</h2>
        <table>
            <thead>
                <tr>
                    for col in &otlp.0 {
                        <th>(col.clone())</th>
                    }
                </tr>
            </thead>
            <tbody>
                for row in &otlp.1 {
                    <tr>
                        for cell in row {
                            <td>(cell.clone())</td>
                        }
                    </tr>
                }
            </tbody>
        </table>

        <h2>"Store — capped tables"</h2>
        <table>
            <thead>
                <tr>
                    for col in &store.0 {
                        <th>(col.clone())</th>
                    }
                </tr>
            </thead>
            <tbody>
                for row in &store.1 {
                    <tr>
                        for cell in row {
                            <td>(cell.clone())</td>
                        }
                    </tr>
                }
            </tbody>
        </table>

        <h2>"Runtime"</h2>
        <table class="kv">
            <tr><td>"DuckDB"</td><td>(db.duckdb_version().unwrap_or_else(|_| "?".into()))</td></tr>
            <tr><td>"capped-core"</td><td>(db.capped_version.clone())</td></tr>
            <tr><td>"data dir"</td><td><code>(db.data_dir_display())</code></td></tr>
            <tr><td>"OTLP endpoint"</td><td><code>(ing.listen_uri.clone())</code>" — OTLP/HTTP, bearer-authenticated"</td></tr>
        </table>

        <h2>"Point your collector at it"</h2>
        <pre>(format!(
            "export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf\nexport OTEL_EXPORTER_OTLP_ENDPOINT={}\nexport OTEL_EXPORTER_OTLP_HEADERS=\"Authorization=Bearer {}\"",
            otlp_http_url(&ing.listen_uri), ing.token
        ))</pre>
    })
}

/// `otlp:host:port` (as registered with the extension) → the URL exporters use.
fn otlp_http_url(listen_uri: &str) -> String {
    format!("http://{}", listen_uri.trim_start_matches("otlp:"))
}
