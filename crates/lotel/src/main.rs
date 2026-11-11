mod api;
mod config;
mod db;
mod ingest;
mod logs;
mod metrics;
mod query;
mod services;
mod skin;
mod stats;
mod storage;
mod traces;
mod ui;

use topcoat::{
    Result,
    context::{Cx, context},
    cookie::{Cookie, CookieLayer, Cookies, SameSite, cookies, time},
    router::{Slot, layout, module_router, page},
    view::{BoxView, Unescaped, View, ViewExt, view},
};

use crate::{
    db::Db,
    ingest::Ingest,
    ui::{CSS, SKIN_COOKIE, live_refresh, nav_links, parse_skin, query_has_skin, theme_panel},
};

use crate::stats::{build_err, err_card};
use crate::ui::cell;

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

    // The three theme switches: query beats the cookie, the cookie beats the
    // defaults, and a deep link both re-renders and sticks (the cookie is
    // rewritten so the next plain navigation keeps the look).
    let query = topcoat::router::request::uri(cx)
        .query()
        .unwrap_or("")
        .to_owned();
    let jar = cookies(cx);
    let cookie = jar.get(SKIN_COOKIE).map(|c| c.value().to_owned());
    let skin = parse_skin(cookie.as_deref(), &query);
    if query_has_skin(&query) {
        jar.add(
            Cookie::build((
                SKIN_COOKIE,
                format!("{}:{}:{}", skin.pal, skin.scheme, skin.font),
            ))
            .path("/")
            .max_age(time::Duration::days(365))
            .same_site(SameSite::Lax)
            .http_only(true)
            .build(),
        );
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
        <html lang="en"
              data-pal=(skin.pal.to_string())
              data-scheme=(skin.scheme.to_string())
              data-font=(skin.font.to_string())>
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <title>"lotel — local OpenTelemetry"</title>
                <style>(Unescaped::new_unchecked(CSS))</style>
            </head>
            <body>
                <div class="layout">
                    <nav>
                        <p class="brand">"lotel"</p>
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
                    <main>
                        theme_panel(
                            pal: skin.pal.to_string(),
                            scheme: skin.scheme.to_string(),
                            font: skin.font.to_string(),
                            query: query,
                        )
                        (slot)
                    </main>
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
    println!(
        "lotel: OTLP on {} (bearer token: {})",
        ing.listen_uri, ing.token
    );
    println!("lotel: UI on http://{}:{}", cfg.host, cfg.port);

    let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
    topcoat::serve(
        listener,
        module_router!()
            .with(db)
            .with(ing)
            .layer(CookieLayer::new())
            .build(),
    )
    .await?;
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
        .query_rows("SELECT table_name, segments, sealed, rows, bytes FROM capped_tables('otel')")
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));
    let otlp = ing
        .stats(db)
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));

    let err = build_err(db, "1h");

    Ok(view! {
        <h1>"Dashboard"</h1>
        <p class="sub">
            "Local OpenTelemetry on capped DuckDB storage — telemetry that stays, then is forgotten on schedule."
        </p>

        <div class="grid">
            for row in &counts.1 {
                <div class="card stat">
                    <div class="n">(cell(row, 1))</div>
                    <div class="l">(cell(row, 0))</div>
                </div>
            }
        </div>

        <h2>"Error rate — traces · last hour"</h2>
        <label class="tail">
            <input type="checkbox" id="err-live-toggle" checked="checked" />
            "live"
        </label>
        <div id="err-live">
            err_card(input: err)
        </div>
        live_refresh(topic: "err".to_string(), target: "err-live".to_string(), toggle: "err-live-toggle".to_string(), cadence: 10000)

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
