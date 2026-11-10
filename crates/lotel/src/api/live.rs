//! The single freshness seam: one endpoint that renders the HTML fragment for
//! a named `topic`. Pages don't poll bespoke per-view URLs — they subscribe to
//! a topic (see `ui::live_refresh`). OSS backs this with poll-on-request; the
//! hosted build can push the same fragments over SSE/WS from an ingest bus
//! without any page changing.

use serde::Deserialize;

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{BoxView, ViewExt, view},
};

use crate::{
    db::Db,
    services::{build_live, sm_map},
    stats::{build_err, err_card},
};

#[derive(Debug, Default, Deserialize)]
struct LiveQuery {
    /// Which live region to render: `map` | `err`.
    #[serde(default)]
    topic: String,
    #[serde(default)]
    range: String,
}

#[page]
async fn live(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let q = parse_query_params::<LiveQuery>(cx).unwrap_or_default();
    match q.topic.as_str() {
        "map" => {
            let range = if q.range.is_empty() { "1h" } else { q.range.as_str() };
            let input = build_live(db, range);
            Ok(view! { sm_map(input: input) }.boxed())
        }
        "err" => {
            let input = build_err(db, "1h");
            Ok(view! { err_card(input: input) }.boxed())
        }
        _ => Ok(view! { <span/> }.boxed()),
    }
}
