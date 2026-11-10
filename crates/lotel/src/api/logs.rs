//! Tail fragment endpoint: bare `<tr>` rows for the logs table, so the page
//! can refresh its tbody without a navigation.

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{View, view},
};

use crate::{
    db::Db,
    logs::{LogFilters, fetch, log_rows},
};

#[page]
async fn logs(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let f = parse_query_params::<LogFilters>(cx).unwrap_or_default();
    let rows = fetch(db, &f).unwrap_or_default();
    Ok(view! { log_rows(rows: rows) })
}
