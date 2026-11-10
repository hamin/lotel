//! Bare service-map fragment: just the canvas (edge SVG + nodes) and its live
//! summary, so the page can refresh `#sm-live` in place without a navigation.

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{View, view},
};

use crate::{
    db::Db,
    services::{MapFilters, build_live, sm_map},
};

#[page]
async fn services(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let f = parse_query_params::<MapFilters>(cx).unwrap_or_default();
    let range = if f.range.is_empty() { "1h" } else { f.range.as_str() };
    let data = build_live(db, range);
    Ok(view! { sm_map(input: data) })
}
