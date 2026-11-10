use topcoat::{
    Result,
    context::{Cx, context},
    router::page,
    view::{View, view},
};

use crate::db::Db;

#[page]
async fn health(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let ok = db.query_rows("SELECT 1 AS alive").is_ok();
    Ok(view! {
        if ok {
            "ok"
        } else {
            "degraded"
        }
    })
}
