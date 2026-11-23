// The agent views' query layer (pages, builders and the render redactor).
#[allow(dead_code)]
mod agents;
mod api;
mod assets;
mod config;
mod correlate;
mod db;
mod duckdb_link;
mod fields;
mod filters;
mod ingest;
mod logs;
mod metrics;
mod query;
mod services;
mod setup;
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
    config::Config,
    db::Db,
    ingest::Ingest,
    ui::{
        CSS, NAV_JS, SKIN_COOKIE, UseBy, live_refresh, nav_groups, parse_skin, query_has_skin, short_ttl,
        theme_panel, thousands,
    },
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

    // The four theme switches: query beats the cookie, the cookie beats the
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
                format!("{}:{}:{}:{}", skin.pal, skin.scheme, skin.font, skin.style),
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
        "/sessions" => "Sessions",
        "/spend" => "Spend",
        "/tools" => "Tools",
        "/setup" => "Setup",
        "/logs" => "Logs",
        "/traces" => "Traces",
        "/metrics" => "Metrics",
        "/query" => "Explore",
        "/services" => "Services",
        "/storage" => "Storage",
        _ => "",
    };

    // The retention stickers beside the settings popover: one per signal,
    // read straight from the boot config the tables were created with.
    let cfg = context::<Config>(cx);
    let usebys = vec![
        UseBy { signal: "logs", ttl: short_ttl(&cfg.ttl_logs) },
        UseBy { signal: "traces", ttl: short_ttl(&cfg.ttl_traces) },
        UseBy { signal: "metrics", ttl: short_ttl(&cfg.ttl_metrics) },
    ];

    Ok(view! {
        <!DOCTYPE html>
        <html lang="en"
              data-pal=(skin.pal.to_string())
              data-scheme=(skin.scheme.to_string())
              data-font=(skin.font.to_string())
              data-style=(skin.style.to_string())>
            <head>
                <meta charset="utf-8" />
                <meta name="viewport" content="width=device-width, initial-scale=1" />
                <title>"lotel: local OpenTelemetry"</title>
                <style>(Unescaped::new_unchecked(CSS))</style>
            </head>
            <body>
                <div class="navbar" aria-hidden="true"></div>
                <div class="layout">
                    <nav>
                        <p class="brand">"lotel"</p>
                        for (heading, links) in nav_groups() {
                            if let Some(h) = heading {
                                <p class="nav-group">(h)</p>
                            }
                            for (label, href) in links.iter().copied() {
                                <a href=(href)
                                   if label == active {
                                       aria-current="page"
                                   }
                                >
                                    (label)
                                </a>
                            }
                        }
                    </nav>
                    <main>
                        theme_panel(
                            pal: skin.pal.to_string(),
                            scheme: skin.scheme.to_string(),
                            font: skin.font.to_string(),
                            style: skin.style.to_string(),
                            query: query,
                            usebys: usebys,
                        )
                        (slot)
                    </main>
                </div>
                <script>(Unescaped::new_unchecked(NAV_JS))</script>
            </body>
        </html>
    }
    .boxed())
}

/// Bundled DuckDB release. The `duckdb` crate is pinned to the matching
/// version (see Cargo.toml); a blob built against another release fails to
/// LOAD at startup, so this compile-time stamp cannot silently drift.
const DUCKDB_VERSION: &str = "2.0.0";

/// Version the bundled capped blob reports through `capped_version()`.
const CAPPED_VERSION: &str = "0.1.0";

/// The one release stamp: printed by `lotel --version` and reused
/// verbatim by the startup banner. The lotel version comes only from
/// Cargo.toml; build.rs stamps the git SHA ("unknown" outside a checkout).
fn version_line() -> String {
    format!(
        "lotel {} ({} · duckdb {DUCKDB_VERSION} · capped {CAPPED_VERSION})",
        env!("CARGO_PKG_VERSION"),
        env!("LOTEL_GIT_SHA"),
    )
}

/// Compare a compile-time stamp against what the runtime reports. DuckDB's
/// `version()` leads with a `v` (`v2.0.0`); it is stripped before comparing.
/// `Some(detail)` naming both values when they differ, `None` when they agree.
fn version_mismatch(expected: &str, runtime: &str) -> Option<String> {
    let runtime = runtime.trim_start_matches('v');
    (runtime != expected).then(|| format!("binary stamped {expected}, runtime reports {runtime}"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Hand-rolled argument handling (no clap): --version answers before
    // config, the store, or any port is touched.
    if std::env::args().any(|arg| arg == "--version" || arg == "-V") {
        println!("{}", version_line());
        return Ok(());
    }

    let mut cfg = config::Config::from_env()?;
    let db = Db::open(&cfg)?;

    // The stamp above is compile-time constants (--version must answer
    // without opening the store), so cross-check them against the loaded
    // runtime: a swapped or stale blob warns loudly on stderr but runs.
    let duck = db.duckdb_version().ok().and_then(|v| version_mismatch(DUCKDB_VERSION, &v));
    if let Some(detail) = duck {
        eprintln!("lotel: warning: duckdb version mismatch — {detail}");
    }
    if let Some(detail) = version_mismatch(CAPPED_VERSION, &db.capped_version) {
        eprintln!("lotel: warning: capped version mismatch — {detail}");
    }

    let ing = ingest::start(&db, &cfg, &db.ext_dir)?;
    // The UI shows the limits the store enforces, not what the env asked for.
    match ingest::adopt_store_retention(&db, &mut cfg) {
        Ok(warnings) => warnings.iter().for_each(|w| eprintln!("{w}")),
        Err(e) => eprintln!("lotel: warning: could not read the store's retention limits: {e}"),
    }

    println!("{}", version_line());
    println!("lotel: store {}", db.data_dir_display());
    println!(
        "lotel: OTLP on {} (bearer token: {})",
        ing.listen_uri, ing.token
    );
    println!("lotel: UI on http://{}:{}", cfg.host, cfg.port);
    // The opt-in announces itself: a lotel that shows prompt text must
    // never look like the default one.
    if let Some(line) = agent_content_line(cfg.agent_show_content) {
        println!("{line}");
    }

    let listener = tokio::net::TcpListener::bind((cfg.host.as_str(), cfg.port)).await?;
    topcoat::serve(
        listener,
        module_router!()
            // The agent pages live in `agents.rs` under explicit absolute paths
            // (several routes, one module), so module derivation skips them and
            // they are registered by hand.
            .page(agents::sessions_page)
            .page(agents::spend_page)
            .page(agents::tools_page)
            .page(setup::setup_page)
            .with(db)
            .with(ing)
            .with(cfg)
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
            "Local OpenTelemetry on capped DuckDB storage: telemetry that stays, then is forgotten on schedule."
        </p>

        <div class="grid">
            for row in &counts.1 {
                <div class="card stat">
                    <div class="n">(thousands(&cell(row, 1)))</div>
                    <div class="l">(cell(row, 0))</div>
                </div>
            }
        </div>

        <h2>"Error rate: traces · last hour"</h2>
        <label class="tail">
            <input type="checkbox" id="err-live-toggle" checked="checked" />
            "live"
        </label>
        <div id="err-live">
            err_card(input: err)
        </div>
        live_refresh(topic: "err".to_string(), target: "err-live".to_string(), toggle: "err-live-toggle".to_string(), cadence: 10000)

        <h2>"Ingest: otlp_serve"</h2>
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

        <h2>"Store: capped tables"</h2>
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
            <tr><td>"OTLP endpoint"</td><td><code>(ing.listen_uri.clone())</code>", OTLP/HTTP, bearer-authenticated"</td></tr>
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

/// The startup banner line that makes the content opt-in impossible to
/// miss: it exists only when `LOTEL_AGENT_SHOW_CONTENT` turned the render
/// redactor off, so a lotel that shows prompt text never reads like the
/// private default.
fn agent_content_line(show_content: bool) -> Option<&'static str> {
    show_content.then_some("lotel: agent content: shown")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The release stamp: `lotel <ver> (<sha> · duckdb <v> · capped <v>)` — the
    /// same string --version prints and the banner leads with.
    #[test]
    fn version_line_has_the_release_stamp_shape() {
        let v = version_line();
        let sha = v
            .strip_prefix(&format!("lotel {} (", env!("CARGO_PKG_VERSION")))
            .unwrap_or_else(|| panic!("must open with the crate version: {v}"))
            .strip_suffix(&format!(" · duckdb {DUCKDB_VERSION} · capped {CAPPED_VERSION})"))
            .unwrap_or_else(|| panic!("must close with the component stamp: {v}"));
        assert!(!sha.is_empty(), "build.rs must stamp LOTEL_GIT_SHA");
    }

    /// The startup cross-check: DuckDB reports `v2.0.0` but the stamp says
    /// `2.0.0` — agreement must be silent, drift must name both values.
    #[test]
    fn version_mismatch_strips_the_v_prefix_and_names_drift() {
        assert_eq!(version_mismatch("2.0.0", "v2.0.0"), None);
        assert_eq!(version_mismatch("2.0.0", "2.0.0"), None);
        assert_eq!(version_mismatch("0.1.0", "0.1.0"), None);
        assert_eq!(
            version_mismatch("2.0.0", "v2.1.0").as_deref(),
            Some("binary stamped 2.0.0, runtime reports 2.1.0")
        );
        assert!(version_mismatch("0.1.0", "").is_some(), "empty runtime is drift");
    }

    /// The privacy banner: masking is the silent default, and the only
    /// line that exists is the one admitting content is being shown.
    #[test]
    fn the_banner_admits_content_only_when_it_is_shown() {
        assert_eq!(agent_content_line(false), None, "masked is the quiet default");
        assert_eq!(agent_content_line(true), Some("lotel: agent content: shown"));
    }
}

