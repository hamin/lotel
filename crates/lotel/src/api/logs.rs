//! Tail fragment endpoint: bare `<tr>` rows for the logs table, so the page
//! can refresh its tbody without a navigation.

use topcoat::{
    Result,
    context::{Cx, context},
    router::page,
    view::{View, view},
};

use crate::{
    db::Db,
    filters::Bar,
    logs::{fetch, log_rows},
};

#[page]
async fn logs(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let bar = Bar::parse(topcoat::router::request::uri(cx).query().unwrap_or(""));
    let rows = fetch(db, &bar).unwrap_or_default();
    Ok(view! { log_rows(rows: rows, bar: bar) })
}
