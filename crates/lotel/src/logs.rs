//! Logs explorer: filtered, tail-able view over otel.main.otlp_logs, with
//! top-value facets and a BM25 "best matches" panel (capped_fts) when
//! a text query is active.
//!
//! The filter bar itself — time range, service, free text, attribute
//! filters — lives in [`crate::filters`] and is shared with the traces page;
//! severity is this page's one first-class extra.

use serde_json::{Map, Value};

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, request::uri},
    view::{View, component, view},
};

use crate::{
    config::Config,
    db::Db,
    fields::{fields_panel, profile_logs},
    filters::{Bar, LOG_COLS, attr_field, empty_range_hint, range_field, service_field},
    ui::{live_refresh, sev_class, thousands},
};

#[cfg(test)]
mod tests {
    use super::{LogRow, RowView, chip_href, fnv1a};
    use crate::{agents::REDACTED, config::Config, filters::Bar};

    fn chips(v: &super::BodyView) -> Vec<(&str, &str)> {
        v.chips.iter().map(|c| (c.k.as_str(), c.v.as_str())).collect()
    }

    #[test]
    fn opencode_array_body_splits_into_message_and_chips() {
        let body = r#"["cli starting","{\"version\":\"2.3.0\",\"args\":[\"run\"]}"]"#;
        let v = super::render_body(body, "");
        assert_eq!(v.text, "cli starting");
        // the payload string is decoded one level; its fields become chips,
        // in stable map order (sorted — same order the store writes attrs in)
        assert_eq!(chips(&v), [("args", "[\"run\"]"), ("version", "2.3.0")]);
        // expandable: the pretty body keeps the full payload escaped one level
        let pretty = v.body_json.expect("json body expands");
        assert!(pretty.contains("cli starting") && pretty.contains("\\\"version\\\""));
        assert!(v.attrs_json.is_none());
    }

    #[test]
    fn object_body_uses_message_key_and_chips_the_rest() {
        let v = super::render_body(r#"{"message":"session created","provider":"anthropic","tokens":1200}"#, "");
        assert_eq!(v.text, "session created");
        assert_eq!(chips(&v), [("provider", "anthropic"), ("tokens", "1200")]);

        // "msg" is a message too
        let v = super::render_body(r#"{"msg":"hi","code":7}"#, "");
        assert_eq!(v.text, "hi");
        assert_eq!(chips(&v), [("code", "7")]);

        // no message key: the compact JSON is the text, fields still chip out
        let v = super::render_body(r#"{"a":1}"#, "");
        assert_eq!(v.text, r#"{"a":1}"#);
        assert_eq!(chips(&v), [("a", "1")]);
    }

    #[test]
    fn json_in_strings_decodes_exactly_one_level() {
        // element decodes to an object; its "payload" string is itself JSON
        // and must NOT decode a second time — it keeps its raw spelling.
        let body = r#"["run","{\"payload\":\"{\\\"deep\\\": 1}\"}"]"#;
        let v = super::render_body(body, "");
        assert_eq!(v.text, "run");
        assert_eq!(chips(&v), [("payload", "{\"deep\": 1}")]);
    }

    #[test]
    fn non_object_array_elements_become_positional_chips() {
        let v = super::render_body(r#"["done",42,"tail",["x"]]"#, "");
        assert_eq!(v.text, "done");
        assert_eq!(chips(&v), [("#1", "42"), ("#2", "tail"), ("#3", "[\"x\"]")]);
    }

    #[test]
    fn non_json_and_json_scalars_render_verbatim() {
        let v = super::render_body("gateway timeout after 3 retries", "");
        assert_eq!(v.text, "gateway timeout after 3 retries");
        assert!(v.chips.is_empty() && v.body_json.is_none() && v.attrs_json.is_none());

        // valid JSON scalars are not "JSON bodies" — they stay plain text
        let v = super::render_body("42", "");
        assert_eq!(v.text, "42");
        assert!(v.body_json.is_none());

        // broken JSON is just text
        let v = super::render_body("{\"message\":", "");
        assert_eq!(v.text, "{\"message\":");
        assert!(v.body_json.is_none());
    }

    #[test]
    fn huge_body_truncates_safely() {
        // past the parse cap: never parsed, no expansion, text clipped —
        // multibyte input must not panic at the cut.
        let body = format!("[\"{}\"]", "é".repeat(super::PARSE_LIMIT));
        let v = super::render_body(&body, "");
        assert!(v.body_json.is_none());
        assert!(v.text.ends_with('…'));
        assert!(v.text.chars().count() <= super::TEXT_LIMIT + 1);

        // parseable but too long to display: text and pretty print both clip
        let body = format!(r#"{{"message":"{}"}}"#, "ü".repeat(30_000));
        let v = super::render_body(&body, "");
        assert!(v.text.ends_with('…'));
        assert!(v.text.chars().count() <= super::TEXT_LIMIT + 1);
        let pretty = v.body_json.expect("still expands");
        assert!(pretty.chars().count() <= super::PRE_LIMIT + 1);
    }

    #[test]
    fn attribute_chips_are_capped_and_in_stable_order() {
        let attrs = r#"{"tool_name":"bash","success":true,"duration_ms":1500,"model":"claude-sonnet-4-5","session.id":"ses_01","extra":"x"}"#;
        let v = super::render_body("claude_code.tool_result", attrs);
        assert_eq!(v.text, "claude_code.tool_result"); // plain body: verbatim
        // best-known keys first (tool_name, success, duration_ms, model),
        // the rest follow sorted — one-line budget of MAX_CHIPS
        assert_eq!(chips(&v), [
            ("tool_name", "bash"),
            ("success", "true"),
            ("duration_ms", "1500"),
            ("model", "claude-sonnet-4-5"),
        ]);
        // the budget-dropped keys are still there in the expansion, pretty-printed
        let pretty = v.attrs_json.expect("attributes expand");
        assert!(pretty.contains("\"session.id\"") && pretty.contains("\n  \"extra\""));
        assert!(v.body_json.is_none()); // plain body has nothing to pretty-print

        // body fields claim the budget before attributes do
        let v = super::render_body(r#"{"message":"m","a":1,"b":2,"c":3,"d":4}"#, attrs);
        assert_eq!(chips(&v), [("a", "1"), ("b", "2"), ("c", "3"), ("d", "4")]);
        assert_eq!(v.chips.len(), super::MAX_CHIPS);
    }

    #[test]
    fn empty_attributes_render_like_no_attributes_at_all() {
        let v = super::render_body("plain", "");
        assert!(v.attrs_json.is_none() && v.chips.is_empty());
        // the store keeps "{}" for attribute-less rows: nothing to chip or expand
        let v = super::render_body("plain", "{}");
        assert!(v.chips.is_empty());
        assert!(v.attrs_json.expect("empty object still pretty-prints").contains("{}"));
    }

    #[test]
    fn chips_link_to_matching_bar_filters() {
        let bar = Bar::parse("service=cart");
        let body = r#"["cli starting","{\"version\":\"2.3.0\"}"]"#;
        let v = super::render_body(body, r#"{"tool_name":"bash"}"#);
        // a body chip carries the body. prefix so the filter reads the body
        let href = chip_href(&v.chips[0], &bar).expect("body chip links");
        assert!(href.starts_with("/logs?service=cart&f=body."), "{href}");
        assert!(href.contains("%3D2.3.0"), "{href}");
        // an attribute chip stays bare-key
        let href = chip_href(&v.chips[1], &bar).expect("attr chip links");
        assert!(href.contains("tool_name%3Dbash"), "{href}");
        assert!(!href.contains("body."), "{href}");
        // positional chips (#N) address nothing the filter language reads
        let v = super::render_body(r#"["done",42]"#, "");
        assert!(chip_href(&v.chips[0], &bar).is_none());
    }

    #[test]
    fn chip_links_survive_values_that_look_like_syntax() {
        let bar = Bar::parse("");
        let v = super::render_body(r#"{"message":"m","expr":"a=b~c"}"#, "");
        let href = chip_href(&v.chips[0], &bar).expect("chip links");
        // the whole `key=value` is one percent-encoded f param
        assert_eq!(href, "/logs?f=body.expr%3Da%3Db%7Ec");
        // …which parses back to exactly this filter
        let back = Bar::parse(&href["/logs?".len()..]);
        assert_eq!(back.attrs().len(), 1);
        assert_eq!(back.attrs()[0].key, "expr");
        assert_eq!(back.attrs()[0].value, "a=b~c");
    }

    #[test]
    fn row_data_keys_are_stable_and_distinct() {
        let row = |time: &str, span: &str, body: &str| LogRow {
            time: time.into(),
            service: "svc".into(),
            severity: "INFO".into(),
            body: body.into(),
            log_attributes: "{}".into(),
            trace_id: String::new(),
            span_id: span.into(),
        };
        let key = |r: &LogRow| RowView::new(r, &Bar::default()).data_key;
        // same row, same key: that is what lets the live tail re-open it
        assert_eq!(key(&row("t1", "s1", "b")), key(&row("t1", "s1", "b")));
        // any identity part differing yields a new key
        assert_ne!(key(&row("t1", "s1", "b")), key(&row("t2", "s1", "b")));
        assert_ne!(key(&row("t1", "s2", "b")), key(&row("t1", "s1", "b")));
        assert_ne!(key(&row("t1", "", "b1")), key(&row("t1", "", "b2")));
        assert_eq!(fnv1a("same"), fnv1a("same"));
    }

    // ——— privacy: the render redactor on the page (§5) ——————————————————————

    /// A boot config that differs only in the content knob, so the render
    /// tests can say "masking on" / "masking off" without ten fields each.
    fn cfg(show_content: bool) -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 0,
            data_dir: Default::default(),
            extension_dir: None,
            ttl_logs: "7 days".into(),
            ttl_traces: "3 days".into(),
            ttl_metrics: "30 days".into(),
            max_bytes: "64MB".into(),
            otlp_addr: "127.0.0.1:0".into(),
            ingest_token: None,
            agent_show_content: show_content,
        }
    }

    fn log_row(body: &str, attrs: &str) -> LogRow {
        LogRow {
            time: "2026-11-17 10:00:00".into(),
            service: "opencode".into(),
            severity: "INFO".into(),
            body: body.into(),
            log_attributes: attrs.into(),
            trace_id: String::new(),
            span_id: "s1".into(),
        }
    }

    /// Everything a row can show, in one string, so a leak anywhere is caught:
    /// main text, the chip line, and both expanded blobs.
    fn rendered(v: &RowView) -> String {
        let chips: Vec<(&str, &str)> = v.chips.iter().map(|c| (c.k.as_str(), c.v.as_str())).collect();
        format!("{}|{chips:?}|{:?}|{:?}", v.text, v.body_json, v.attrs_json)
    }

    /// The one real leak in the capture (§2), tested through the renderer the
    /// page actually uses: the `cli starting` message stays so the row is
    /// still recognisable, and the command line behind it — the user's prompt
    /// included — reaches neither the text, the chips, nor the expansion.
    /// `LOTEL_AGENT_SHOW_CONTENT` is the only way to see it.
    #[test]
    fn cli_starting_row_hides_the_payload_unless_content_is_shown() {
        let canary = "summarise the private file";
        let payload = format!(r#"{{"version":"2.3.0","args":["run","{canary}"]}}"#);
        let quoted = format!("{payload:?}");
        let body = format!(r#"["cli starting",{quoted}]"#);
        let r = log_row(&body, "{}");

        let hidden = RowView::new(&r.for_display(&cfg(false)), &Bar::default());
        let shown = RowView::new(&r.for_display(&cfg(true)), &Bar::default());

        assert_eq!(hidden.text, "cli starting", "the message is kept");
        let pretty = hidden.body_json.as_deref().expect("the body still expands");
        assert!(pretty.contains(REDACTED), "and shows the placeholder: {pretty}");
        assert!(!rendered(&hidden).contains(canary), "payload leaked: {}", rendered(&hidden));
        assert!(!rendered(&hidden).contains("2.3.0"), "the whole element goes: {}", rendered(&hidden));

        // the opt-in is the only switch, and it switches everything back
        assert!(rendered(&shown).contains(canary), "opt-in must show it: {}", rendered(&shown));
        assert!(rendered(&shown).contains("2.3.0"));
    }

    /// A gated attribute is masked in the chips and in the expanded bag while
    /// the row stays readable: event name, tool name and the numeric length
    /// companion are exactly what the views chart, and they leak nothing.
    #[test]
    fn gated_attributes_are_masked_in_chips_and_in_the_expansion() {
        let attrs = r#"{"event.name":"user_prompt","prompt":"the private prompt text","prompt_length":22,"tool_name":"bash"}"#;
        let r = log_row("claude_code.user_prompt", attrs);
        let v = RowView::new(&r.for_display(&cfg(false)), &Bar::default());

        let chips: Vec<(&str, &str)> = v.chips.iter().map(|c| (c.k.as_str(), c.v.as_str())).collect();
        assert!(chips.contains(&("tool_name", "bash")), "preferred key keeps its chip: {chips:?}");
        assert!(chips.contains(&("prompt_length", "22")), "companion survives: {chips:?}");
        let prompt = chips.iter().find(|(k, _)| *k == "prompt").expect("the key is still listed");
        assert_eq!(prompt.1, REDACTED);

        let pretty = v.attrs_json.as_deref().expect("the bag still expands");
        assert!(pretty.contains(&format!("\"prompt\": \"{REDACTED}\"")), "{pretty}");
        assert!(pretty.contains("user_prompt") && pretty.contains("\"prompt_length\": 22"));

        let all = rendered(&v);
        assert!(!all.contains("private prompt"), "the text must not survive anywhere: {all}");
        // and the opt-in puts it back verbatim
        let shown = RowView::new(&r.for_display(&cfg(true)), &Bar::default());
        assert!(rendered(&shown).contains("private prompt"), "opt-in must show it");
    }

    /// Plain-text bodies are not JSON, so only the two display masks apply
    /// (§5): the address and the home directory go, the words stay.
    #[test]
    fn plain_text_rows_keep_their_words_and_lose_their_addresses() {
        let r = log_row("wrote app.rs for jane.doe@corp.example", "{}");
        let v = RowView::new(&r.for_display(&cfg(false)), &Bar::default());
        assert_eq!(v.text, "wrote app.rs for [redacted-email@example.invalid]");
        let shown = RowView::new(&r.for_display(&cfg(true)), &Bar::default());
        assert!(shown.text.contains("jane.doe@corp.example"));
    }
}

/// One rendered log line.
#[derive(Clone)]
pub struct LogRow {
    pub time: String,
    pub service: String,
    pub severity: String,
    pub body: String,
    pub log_attributes: String,
    pub trace_id: String,
    pub span_id: String,
}

impl LogRow {
    /// The row as a page may display it: the two columns that carry stored
    /// text (body, attributes) through the shared render redactor
    /// ([`crate::agents::redact_cell`], plan §5) unless the operator opted in
    /// with `LOTEL_AGENT_SHOW_CONTENT`. Identity columns are untouched, so the
    /// row still links to its own trace, and the store itself is never
    /// rewritten — this is a display copy.
    pub(crate) fn for_display(&self, cfg: &Config) -> Self {
        Self {
            time: self.time.clone(),
            service: self.service.clone(),
            severity: self.severity.clone(),
            body: cfg.display(&self.body),
            log_attributes: cfg.display(&self.log_attributes),
            trace_id: self.trace_id.clone(),
            span_id: self.span_id.clone(),
        }
    }
}

/// Shared query for the page and the tail fragment endpoint.
pub(crate) fn fetch(db: &Db, f: &Bar) -> anyhow::Result<Vec<LogRow>> {
    let cutoff = f.cutoff();
    let limit = f.limit();
    let q = f.get("q");
    let service = f.get("service");
    let severity = f.get("severity");
    let (conds, attr_params) = f.attr_conds(&LOG_COLS);

    let mut sql = "SELECT time_unix_nano, service_name, severity_text, body, COALESCE(log_attributes, ''), COALESCE(trace_id, ''), COALESCE(span_id, '')
         FROM otel.main.otlp_logs
         WHERE time_unix_nano >= CAST(? AS TIMESTAMP)
           AND (? = '' OR body ILIKE '%' || ? || '%')
           AND (? = '' OR service_name = ?)
           AND (? = '' OR upper(severity_text) = upper(?))"
        .to_string();
    for c in &conds {
        sql.push_str(&format!(" AND {c}"));
    }
    sql.push_str(&format!(" ORDER BY time_unix_nano DESC LIMIT {limit}"));

    let mut params: Vec<&dyn duckdb::ToSql> =
        vec![&cutoff, &q, &q, &service, &service, &severity, &severity];
    params.extend(attr_params.iter().map(|s| s as &dyn duckdb::ToSql));

    let (_, rows) = db.query_rows_with(&sql, &params)?;
    Ok(rows
        .into_iter()
        .map(|r| LogRow {
            time: r[0].clone(),
            service: r[1].clone(),
            severity: r[2].clone(),
            body: r[3].clone(),
            log_attributes: r[4].clone(),
            trace_id: r[5].clone(),
            span_id: r[6].clone(),
        })
        .collect())
}

pub(crate) fn option_values(db: &Db, column: &str) -> Vec<String> {
    // column comes from our own code, never user input
    db.query_rows(&format!(
        "SELECT DISTINCT {column} FROM otel.main.otlp_logs WHERE {column} IS NOT NULL AND {column} != '' ORDER BY 1"
    ))
    .map(|(_, rows)| rows.into_iter().map(|r| r[0].clone()).collect())
    .unwrap_or_default()
}

/// One bar of a facet.
pub(crate) struct FacetValue {
    pub label: String,
    pub count: String,
    pub width: u32,
    pub on: bool,
    pub href: String,
}

/// A facet column: the top values under the current
/// filters, each a link that toggles that filter.
pub(crate) struct Facet {
    pub title: &'static str,
    pub values: Vec<FacetValue>,
}

/// Top values of severity_text / service_name over the current window,
/// under the current free-text and attribute filters. A facet ignores its
/// own dimension's filter — while filtered to ERROR you still see the
/// sibling severities, so one click can switch or clear.
pub(crate) fn facets(db: &Db, f: &Bar) -> Vec<Facet> {
    let cutoff = f.cutoff();
    let q = f.get("q");
    let (conds, attr_params) = f.attr_conds(&LOG_COLS);
    [("Severity", "severity", "severity_text"), ("Service", "service", "service_name")]
        .into_iter()
        .map(|(title, param, column)| {
            // a facet keeps the SIBLING's filter in the WHERE and reads its
            // OWN filter only to mark the active row
            let (active, other_column, other) = if param == "severity" {
                (f.get("severity"), "service_name", f.get("service"))
            } else {
                (f.get("service"), "severity_text", f.get("severity"))
            };
            // column/param/other_column are our own constants, never input
            // — the severity sibling compares like fetch does, case-insensitively
            let other_cond = if param == "service" {
                format!("upper({other_column}) = upper(?)")
            } else {
                format!("{other_column} = ?")
            };
            let mut sql = format!(
                "SELECT {column}, count(*) FROM otel.main.otlp_logs
                 WHERE time_unix_nano >= CAST(? AS TIMESTAMP)
                   AND (? = '' OR body ILIKE '%' || ? || '%')
                   AND (? = '' OR {other_cond})"
            );
            for c in &conds {
                sql.push_str(&format!(" AND {c}"));
            }
            sql.push_str(&format!(
                "
                   AND {column} IS NOT NULL AND {column} != ''
                 GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 6"
            ));
            let mut params: Vec<&dyn duckdb::ToSql> =
                vec![&cutoff, &q, &q, &other, &other];
            params.extend(attr_params.iter().map(|s| s as &dyn duckdb::ToSql));
            let rows = db
                .query_rows_with(&sql, &params)
                .map(|(_, r)| r)
                .unwrap_or_default();
            let top = rows
                .first()
                .and_then(|r| r[1].parse::<f64>().ok())
                .unwrap_or(0.0)
                .max(1.0);
            let values = rows
                .into_iter()
                .map(|r| {
                    let on = active.eq_ignore_ascii_case(&r[0]);
                    let width =
                        ((r[1].parse::<f64>().unwrap_or(0.0) / top) * 100.0).round() as u32;
                    FacetValue {
                        href: f.href_toggling("/logs", param, &r[0], on),
                        label: r[0].clone(),
                        count: r[1].clone(),
                        width,
                        on,
                    }
                })
                .collect();
            Facet { title, values }
        })
        .collect()
}

/// A BM25-ranked hit from the sealed segments (sidecar columns are rendered
/// straight from the index — no join back to the segment).
pub(crate) struct RankRow {
    pub time: String,
    pub service: String,
    pub severity: String,
    pub body: String,
    pub score: String,
    pub trace_id: String,
}

/// Top matches for `q` via capped_fts over the sealed, indexed segments.
/// Err means "not ready" (no indexed segments yet); the caller degrades to a
/// note, never an error page — the LIKE list below still covers everything.
/// The hit's body is the index's own copy, so it goes through the same
/// display redactor the row list uses (§5) before it is rendered.
pub(crate) fn fetch_ranked(db: &Db, f: &Bar, cfg: &Config) -> anyhow::Result<Vec<RankRow>> {
    let cutoff = f.cutoff();
    let service = f.get("service");
    let severity = f.get("severity");
    let q = f.get("q");
    // table-function arguments must be constant at bind time, so q is quoted,
    // not parameterized; every user filter below still binds normally
    let q_sql = crate::db::quote(q);
    let (conds, attr_params) = f.attr_conds_aliased(&LOG_COLS, Some("lotel_a"));

    let mut sql = format!(
        "SELECT f.time_unix_nano, f.service_name, f.severity_text, f.body, COALESCE(f.trace_id, ''), f.score
         FROM capped_fts('otel', 'otlp_logs', {q_sql}, 64) f
         WHERE f.time_unix_nano >= ?
           AND (? = '' OR f.service_name = ?)
           AND (? = '' OR upper(f.severity_text) = upper(?))"
    );
    let mut params: Vec<&dyn duckdb::ToSql> =
        vec![&cutoff, &service, &service, &severity, &severity];
    if !conds.is_empty() {
        // the sidecar carries no attribute columns, so an attribute filter
        // narrows the ranked list through a semijoin against the store —
        // EXISTS keeps one entry per hit even if identical rows differ only
        // in the filtered bag
        sql.push_str(
            " AND EXISTS (SELECT 1 FROM otel.main.otlp_logs lotel_a \
               WHERE CAST(lotel_a.time_unix_nano AS VARCHAR) = CAST(f.time_unix_nano AS VARCHAR) \
                 AND COALESCE(lotel_a.service_name, '') = COALESCE(f.service_name, '') \
                 AND COALESCE(lotel_a.severity_text, '') = COALESCE(f.severity_text, '') \
                 AND COALESCE(lotel_a.body, '') = COALESCE(f.body, '') \
                 AND COALESCE(lotel_a.trace_id, '') = COALESCE(f.trace_id, '')",
        );
        for c in &conds {
            sql.push_str(&format!(" AND {c}"));
        }
        sql.push(')');
        params.extend(attr_params.iter().map(|s| s as &dyn duckdb::ToSql));
    }
    sql.push_str(" ORDER BY f.score DESC LIMIT 8");

    let (_, rows) = db.query_rows_with(&sql, &params)?;
    Ok(rows
        .into_iter()
        .map(|r| RankRow {
            time: r[0].clone(),
            service: r[1].clone(),
            severity: r[2].clone(),
            body: render_body(&cfg.display(&r[3]), "").text,
            score: r[5]
                .parse::<f64>()
                .map(|s| format!("{s:.2}"))
                .unwrap_or_else(|_| r[5].clone()),
            trace_id: r[4].clone(),
        })
        .collect())
}

// ——————————————————————————————————————————— readable JSON bodies ———————————————————————————————————————————
// body and log_attributes arrive as VARCHAR: plain text, or JSON — an object,
// or the `[message, "<payload>"]` array the agent CLIs (opencode, Claude
// Code) send. When the body parses as an object or array the row shows its
// message as the main text and the remaining fields as compact key=value
// chips, and a <details> (no JS) expands to the pretty-printed JSON.
// Non-JSON bodies render exactly as before: text only, no chips, no expansion.
// Bodies above PARSE_LIMIT are never parsed — they render as clipped text, so
// one runaway body can't stall the page.

/// Largest body/attributes JSON we attempt to parse, per row, in bytes.
const PARSE_LIMIT: usize = 1 << 20;
/// Main text shown before an ellipsis, in chars.
const TEXT_LIMIT: usize = 2_000;
/// One chip value, in chars.
const CHIP_LIMIT: usize = 96;
/// Pretty-printed JSON inside the row's <details>, in chars.
const PRE_LIMIT: usize = 20_000;
/// Chips per row — more never fit on the line anyway.
const MAX_CHIPS: usize = 4;
/// The keys an object body may carry its message under, by preference.
const MESSAGE_KEYS: [&str; 2] = ["message", "msg"];
/// Attribute keys that earn first claim on the chip budget, best-known first
/// (the Claude Code rows are unreadable without tool_name). Everything else
/// follows in the store's sorted order.
const PREFERRED_ATTR_KEYS: [&str; 4] = ["tool_name", "success", "duration_ms", "model"];

/// Where a chip's key lives: body fields filter through `body.`, attribute
/// fields through the bare key (see crate::filters).
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum ChipSrc {
    Body,
    Attrs,
}

/// One compact `key=value` chip under the main text.
pub(crate) struct Chip {
    pub k: String,
    pub v: String,
    pub src: ChipSrc,
}

/// What a row displays: main text, its chip line, and the pretty-printed
/// blobs the <details> reveals (None = nothing to expand).
struct BodyView {
    text: String,
    chips: Vec<Chip>,
    body_json: Option<String>,
    attrs_json: Option<String>,
}

/// Clip at a char boundary (never mid-multibyte) and mark the cut.
fn clip(s: &str, limit: usize) -> String {
    match s.char_indices().nth(limit) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Parse if small enough; only objects and arrays count as a JSON body —
/// bare scalars (`42`, `true`) are just text, same as today.
fn parse_json(s: &str) -> Option<Value> {
    if s.is_empty() || s.len() > PARSE_LIMIT {
        return None;
    }
    serde_json::from_str::<Value>(s)
        .ok()
        .filter(|v| v.is_array() || v.is_object())
}

/// A JSON value as compact chip text. When `decode` is set, a string that is
/// itself JSON is decoded one level, so opencode's payload strings read as
/// JSON, not as a wall of escapes; decoded content is never re-scanned.
fn chip_value(v: &Value, decode: bool) -> String {
    let raw = match v {
        Value::String(s)
            if decode && s.len() <= PARSE_LIMIT =>
        {
            match serde_json::from_str::<Value>(s) {
                Ok(inner) if inner.is_object() || inner.is_array() => {
                    serde_json::to_string(&inner).unwrap_or_else(|_| s.clone())
                }
                _ => s.clone(),
            }
        }
        // strings are the text they are — never their quoted JSON form
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    clip(&raw, CHIP_LIMIT)
}

/// Object entries as chips — in map order (the store writes keys sorted,
/// serde keeps them sorted, so the row order is stable).
fn entry_chips(map: &Map<String, Value>, decode: bool, src: ChipSrc) -> Vec<Chip> {
    map.iter()
        .map(|(k, v)| Chip {
            k: k.clone(),
            v: chip_value(v, decode),
            src,
        })
        .collect()
}

/// Split one row's body (+ its log_attributes) into main text, chips and
/// the pretty-printed blobs for the expansion.
fn render_body(body: &str, attrs: &str) -> BodyView {
    let parsed = parse_json(body);
    let (text, mut chips) = match &parsed {
        Some(Value::Array(items)) => {
            // The first string element is the message; the rest become fields,
            // with string elements that are themselves JSON expanded into theirs.
            let main = items.iter().position(Value::is_string);
            let text = match main {
                Some(i) => clip(items[i].as_str().unwrap_or_default(), TEXT_LIMIT),
                None => clip(body, TEXT_LIMIT),
            };
            let mut chips = Vec::new();
            for (i, e) in items.iter().enumerate() {
                if main == Some(i) {
                    continue;
                }
                // direct object elements sit one level under the body, so
                // their string values may decode; content decoded from a
                // string element may not — that would be two levels.
                let fields = match e {
                    Value::Object(m) => Some((m.clone(), true)),
                    Value::String(s) if s.len() <= PARSE_LIMIT => {
                        serde_json::from_str::<Value>(s)
                            .ok()
                            .and_then(|v| v.as_object().cloned())
                            .map(|m| (m, false))
                    }
                    _ => None,
                };
                match fields {
                    Some((m, decode)) => chips.extend(entry_chips(&m, decode, ChipSrc::Body)),
                    None => chips.push(Chip {
                        k: format!("#{i}"),
                        v: chip_value(e, true),
                        src: ChipSrc::Body,
                    }),
                }
            }
            (text, chips)
        }
        Some(Value::Object(map)) => {
            let key = MESSAGE_KEYS
                .iter()
                .copied()
                .find(|k| map.get(*k).is_some_and(|v| v.is_string()));
            let text = match key {
                Some(k) => clip(map[k].as_str().unwrap_or_default(), TEXT_LIMIT),
                None => clip(&serde_json::to_string(map).unwrap_or_default(), TEXT_LIMIT),
            };
            let chips = map
                .iter()
                .filter(|(k, _)| key != Some(k.as_str()))
                .map(|(k, v)| Chip {
                    k: k.clone(),
                    v: chip_value(v, true),
                    src: ChipSrc::Body,
                })
                .collect();
            (text, chips)
        }
        _ => (clip(body, TEXT_LIMIT), Vec::new()),
    };

    // log_attributes fill the chip budget after the body's own fields:
    // best-known keys first, then the store's sorted remainder.
    let parsed_attrs = parse_json(attrs);
    if let Some(Value::Object(map)) = &parsed_attrs {
        let mut attr_chips = entry_chips(map, true, ChipSrc::Attrs);
        attr_chips.sort_by(|a, b| {
            let rank = |c: &Chip| {
                PREFERRED_ATTR_KEYS
                    .iter()
                    .position(|k| *k == c.k.as_str())
                    .unwrap_or(PREFERRED_ATTR_KEYS.len())
            };
            (rank(a), &a.k).cmp(&(rank(b), &b.k))
        });
        for c in attr_chips {
            if chips.len() >= MAX_CHIPS {
                break;
            }
            chips.push(c);
        }
    }
    chips.truncate(MAX_CHIPS);

    let pretty = |v: &Value| clip(&serde_json::to_string_pretty(v).unwrap_or_default(), PRE_LIMIT);
    BodyView {
        text,
        chips,
        body_json: parsed.as_ref().map(pretty),
        attrs_json: parsed_attrs.as_ref().map(pretty),
    }
}

/// The plain link that adds a chip's filter to the bar (like the facet
/// links). Positional chips (`#1`) address nothing the filter language reads,
/// so they stay unlinked; a clipped value filters by substring instead of
/// exact equality, since its tail is hidden.
fn chip_href(c: &Chip, bar: &Bar) -> Option<String> {
    use crate::filters::{AttrOp, AttrSource, canonical_of};
    if c.k.starts_with('#') {
        return None;
    }
    let source = match c.src {
        ChipSrc::Body => AttrSource::Body,
        ChipSrc::Attrs => AttrSource::Row,
    };
    let (op, v) = match c.v.strip_suffix('…') {
        Some(clipped) => (AttrOp::Has, clipped),
        None => (AttrOp::Eq, c.v.as_str()),
    };
    Some(bar.href_add_filter("/logs", &canonical_of(source, op, &c.k, v)))
}

/// FNV-1a over the body: enough entropy to tell two same-named rows apart
/// in the live tail's open-details bookkeeping, cheap enough per row.
fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf2_9ce4_8422_2325, |acc, b| {
        (acc ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

/// A row with its body pre-rendered, so the view only stitches strings.
struct RowView {
    time: String,
    service: String,
    severity: String,
    text: String,
    chips: Vec<ChipView>,
    body_json: Option<String>,
    attrs_json: Option<String>,
    trace_id: String,
    /// stable identity for the live tail: unchanged rows keep their open
    /// <details> across a swap (see ui::LIVE_JS)
    data_key: String,
}

/// One chip with the href that activates its filter.
struct ChipView {
    k: String,
    v: String,
    href: Option<String>,
}

impl RowView {
    fn new(r: &LogRow, bar: &Bar) -> Self {
        let v = render_body(&r.body, &r.log_attributes);
        Self {
            time: r.time.clone(),
            service: r.service.clone(),
            severity: r.severity.clone(),
            text: v.text,
            chips: v
                .chips
                .iter()
                .map(|c| ChipView {
                    k: c.k.clone(),
                    v: c.v.clone(),
                    href: chip_href(c, bar),
                })
                .collect(),
            body_json: v.body_json,
            attrs_json: v.attrs_json,
            trace_id: r.trace_id.clone(),
            data_key: format!("{}|{}|{:016x}", r.time, r.span_id, fnv1a(&r.body)),
        }
    }
}

/// The table rows — also served bare by `/api/logs` for the tail poller.
#[component]
pub(crate) async fn log_rows(cx: &Cx, rows: Vec<LogRow>, bar: Bar) -> Result<impl View> {
    // Content is hidden here, not in the store (§5): the page and the tail
    // fragment render the same masked copy, so neither can leak what the
    // other one shows.
    let cfg = context::<Config>(cx);
    let rows: Vec<RowView> = rows
        .iter()
        .map(|r| RowView::new(&r.for_display(cfg), &bar))
        .collect();
    Ok(view! {
        for row in rows {
            <tr>
                <td class="muted">(row.time)</td>
                <td>(row.service)</td>
                <td><span class=(format!("badge {}", sev_class(&row.severity)))>(row.severity)</span></td>
                <td class="logbody">
                    if row.body_json.is_some() || row.attrs_json.is_some() {
                        <details class="logx" data-key=(row.data_key)>
                            <summary class="logtext">(row.text.clone())</summary>
                            if let Some(j) = &row.body_json {
                                <div class="lj-k">"body"</div>
                                <pre>(j.clone())</pre>
                            }
                            if let Some(a) = &row.attrs_json {
                                <div class="lj-k">"log_attributes"</div>
                                <pre>(a.clone())</pre>
                            }
                        </details>
                    } else {
                        <span class="logtext">(row.text.clone())</span>
                    }
                    if !row.chips.is_empty() {
                        <span class="logchips">
                            for c in &row.chips {
                                if let Some(h) = &c.href {
                                    <a class="kchip" href=(h.clone())><b>(c.k.clone())</b>"="(c.v.clone())</a>
                                } else {
                                    <span class="kchip"><b>(c.k.clone())</b>"="(c.v.clone())</span>
                                }
                            }
                        </span>
                    }
                </td>
                <td>
                    if !row.trace_id.is_empty() {
                        <a class="trace-link mono" href=(format!("/traces?trace={}", row.trace_id))>"trace"</a>
                    }
                </td>
            </tr>
        }
    })
}

/// The facet bar row: label, proportional bar, count — click toggles.
#[component]
pub(crate) async fn facet_panel(cx: &Cx, facets: Vec<Facet>) -> Result<impl View> {
    Ok(view! {
        <div class="facets">
            for fac in facets {
                <div class="facet">
                    <h3>(fac.title)</h3>
                    if fac.values.is_empty() {
                        <p class="muted">"No data in this window."</p>
                    }
                    for v in fac.values {
                        <a class=(if v.on { "facet-row on" } else { "facet-row" }) href=(v.href)>
                            <span class="facet-k">(v.label)</span>
                            <span class="facet-bar"><i style=(format!("width:{}%", v.width))></i></span>
                            <span class="facet-n">(thousands(&v.count))</span>
                        </a>
                    }
                </div>
            }
        </div>
    })
}

/// The BM25 panel: ranked hits, or an honest note when the index has nothing
/// (open segments are unindexed by design — the tail list below still covers
/// them).
#[component]
pub(crate) async fn ranked_panel(cx: &Cx, rows: Vec<RankRow>, note: Option<String>) -> Result<impl View> {
    Ok(view! {
        <div class="card ranked">
            <div class="ranked-head">
                <span class="ranked-title">"Best matches (BM25)"</span>
                <span class="muted">"from the sealed, indexed segments"</span>
            </div>
            if let Some(text) = note {
                <p class="muted">(text)</p>
            }
            for r in rows {
                <a class="ranked-row" href=(if r.trace_id.is_empty() { String::new() } else { format!("/traces?trace={}", r.trace_id) })>
                    <span class="muted mono ranked-time">(r.time)</span>
                    <span class="badge score">(r.score)</span>
                    <span class=(format!("badge {}", sev_class(&r.severity)))>(r.severity)</span>
                    <span class="ranked-svc">(r.service)</span>
                    <span class="logbody">(r.body)</span>
                </a>
            }
        </div>
    })
}

#[page]
async fn logs(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let cfg = context::<Config>(cx);
    let bar = Bar::parse(uri(cx).query().unwrap_or(""));

    let rows = fetch(db, &bar).unwrap_or_default();
    // An empty list with a range narrower than all: the window itself is the
    // likely suspect (the owner hit chip + 1h = empty page), so offer the
    // same query at range=all. Inside the tbody, so the live tail's swap
    // clears the hint as soon as rows arrive.
    let empty_hint = if rows.is_empty() && bar.range_is_narrow() {
        Some(bar.href("/logs", &[("range", "all")]))
    } else {
        None
    };
    let services = option_values(db, "service_name");
    let severities = option_values(db, "severity_text");
    let facet_groups = facets(db, &bar);
    let fields = profile_logs(db, &bar, cfg);

    let q = bar.get("q").to_string();
    let (ranked_rows, ranked_note) = if q.is_empty() {
        (Vec::new(), None)
    } else {
        match fetch_ranked(db, &bar, cfg) {
            Ok(rs) if !rs.is_empty() => (rs, None),
            Ok(_) => (
                Vec::new(),
                Some(
                    "Nothing indexed for this query yet: fresh rows are still being sealed; \
                     the list below matches on raw text meanwhile."
                        .to_string(),
                ),
            ),
            Err(_) => (
                Vec::new(),
                Some(
                    "Search index unavailable: the list below still matches on raw text."
                        .to_string(),
                ),
            ),
        }
    };

    let severity = bar.get("severity").to_string();
    let service = bar.get("service").to_string();
    let range = bar.get("range").to_string();
    let raw_f = bar.raw_f();
    let attr_errors = bar.attr_errors();

    Ok(view! {
        <h1>"Logs"</h1>
        <p class="sub">"Recent log records from the capped store. Tail updates in place."</p>

        <form class="filters" method="get" action="/logs">
            <label>"Body contains"
                <input type="search" name="q" placeholder="body contains…" value=(q.clone()) />
            </label>
            service_field(options: services, current: service)
            <label>"Severity"
                <select name="severity">
                    <option value="" if severity.is_empty() { selected="selected" }>"all"</option>
                    for s in &severities {
                        <option value=(s.clone()) if *s == severity { selected="selected" }>(s.clone())</option>
                    }
                </select>
            </label>
            range_field(current: range)
            attr_field(raw: raw_f, errors: attr_errors)
            <button type="submit">"Apply"</button>
            <label class="tail">
                <input type="checkbox" id="tail-toggle" checked="checked" />
                "live tail"
            </label>
        </form>

        if !q.is_empty() {
            ranked_panel(rows: ranked_rows, note: ranked_note)
        }

        facet_panel(facets: facet_groups)

        fields_panel(panel: fields)

        <table>
            <thead>
                <tr><th>"time"</th><th>"service"</th><th>"severity"</th><th>"body"</th><th></th></tr>
            </thead>
            <tbody id="logrows">
                if let Some(h) = &empty_hint {
                    empty_range_hint(colspan: 5, noun: "logs", href: h.clone())
                }
                log_rows(rows: rows, bar: bar.clone())
            </tbody>
        </table>

        live_refresh(topic: "logs".to_string(), target: "logrows".to_string(), toggle: "tail-toggle".to_string(), cadence: 3000)
    })
}

// ————————————————————————————————————————————— DB fixtures ——————————————————————————————————————————————
// One filter round-trip per page against real capped tables: a shared
// insert helper, then key=value on this page's columns must return exactly
// the expected rows.

#[cfg(test)]
pub(crate) mod db_tests {
    use super::{facets, fetch};
    use crate::{config::Config, db::Db, filters::Bar};
    use std::fs;

    pub(crate) fn temp_db(tag: &str) -> Db {
        let dir = std::env::temp_dir().join(format!("lotel-filters-test-{tag}"));
        let _ = fs::remove_dir_all(&dir);
        let cfg = Config {
            host: "127.0.0.1".into(),
            port: 0,
            data_dir: dir,
            extension_dir: None,
            ttl_logs: "7 days".into(),
            ttl_traces: "3 days".into(),
            ttl_metrics: "30 days".into(),
            max_bytes: "64MB".into(),
            otlp_addr: "127.0.0.1:0".into(),
            ingest_token: None,
            agent_show_content: false,
        };
        Db::open(&cfg).expect("open temp db")
    }

    /// The log-side store: three rows whose bodies cover plain text, a JSON
    /// object, and the opencode `[message, "<payload>"]` array.
    pub(crate) fn seed_logs(db: &Db) {
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE otel.main.otlp_logs(
                    time_unix_nano TIMESTAMP, trace_id VARCHAR, span_id VARCHAR,
                    service_name VARCHAR, severity_text VARCHAR, body VARCHAR,
                    resource_attributes VARCHAR, log_attributes VARCHAR)
                 WITH (time_col = 'time_unix_nano', ttl = '7 days', max_bytes = '64MB');
                 INSERT INTO otel.main.otlp_logs
                    (time_unix_nano, trace_id, span_id, service_name, severity_text, body, resource_attributes, log_attributes)
                 VALUES
                    ('2026-11-17 10:00:00', 't1', 's1', 'claude-code', 'INFO',
                     'claude_code.tool_result', '{\"service.name\":\"claude-code\",\"deployment.environment\":\"dev\"}',
                     '{\"tool_name\":\"bash\",\"success\":true,\"duration_ms\":1500}'),
                    ('2026-11-17 10:00:01', 't2', 's2', 'opencode', 'INFO',
                     '[\"cli starting\",\"{\\\"version\\\":\\\"2.3.0\\\"}\"]', '{\"service.name\":\"opencode\"}', '{}'),
                    ('2026-11-17 10:00:02', '', 's3', 'cart', 'WARN',
                     'plain text that is not JSON at all', '{}', '{}');",
            )?;
            Ok(())
        })
        .expect("seed logs");
    }

    fn bodies(tag: &str, query: &str) -> Vec<String> {
        let db = temp_db(tag);
        seed_logs(&db);
        let rows = fetch(&db, &Bar::parse(query)).expect("filtered fetch");
        let mut out: Vec<String> = rows.into_iter().map(|r| r.body).collect();
        out.sort();
        let _ = fs::remove_dir_all(&db.data_dir);
        out
    }

    #[test]
    fn bare_key_filters_the_rows_own_attributes() {
        assert_eq!(
            bodies("bare", "f=tool_name%3Dbash"),
            vec!["claude_code.tool_result".to_string()]
        );
        // the numeric attribute extracts as its text
        assert_eq!(
            bodies("bare-num", "f=duration_ms%3D1500"),
            vec!["claude_code.tool_result".to_string()]
        );
        // a value that matches nothing returns nothing — not an error
        assert!(bodies("bare-none", "f=tool_name%3Dnope").is_empty());
    }

    #[test]
    fn negation_and_substring_operators() {
        // != narrows to rows that HAVE the key with a different value
        assert_eq!(
            bodies("neg", "f=tool_name%21%3Dbash"),
            Vec::<String>::new(),
            "only one row has tool_name and it is bash"
        );
        assert_eq!(
            bodies("neg-none", "f=session.id%7Eses"),
            Vec::<String>::new(),
            "no row has session.id at all"
        );
        assert_eq!(
            bodies("has", "f=tool_name%7Eba"),
            vec!["claude_code.tool_result".to_string()]
        );
    }

    #[test]
    fn resource_prefix_reads_resource_attributes() {
        assert_eq!(
            bodies("res", "f=resource.deployment.environment%3Ddev"),
            vec!["claude_code.tool_result".to_string()]
        );
        assert_eq!(
            bodies("res-svc", "f=resource.service.name%3Dopencode"),
            vec!["[\"cli starting\",\"{\\\"version\\\":\\\"2.3.0\\\"}\"]".to_string()]
        );
    }

    #[test]
    fn body_prefix_reads_object_bodies_and_decoded_array_payloads() {
        // JSON object body (J1's message field)
        assert_eq!(
            bodies("obj", "f=body.message%3Dnothing"),
            Vec::<String>::new(),
            "no seeded object body"
        );
        // the array body's payload string decoded one level — the exact
        // field the chips show
        assert_eq!(
            bodies("arr", "f=body.version%3D2.3.0"),
            vec!["[\"cli starting\",\"{\\\"version\\\":\\\"2.3.0\\\"}\"]".to_string()]
        );
        // a plain-text body never matches and never errors
        assert!(bodies("arr-plain", "f=body.anything%3Dtext").is_empty());
    }

    #[test]
    fn filters_and_together_with_the_first_class_fields() {
        // attribute filter AND severity AND service
        assert_eq!(
            bodies("and", "f=tool_name%3Dbash&severity=info&service=claude-code"),
            vec!["claude_code.tool_result".to_string()]
        );
        assert!(
            bodies("and-no", "f=tool_name%3Dbash&severity=WARN").is_empty(),
            "AND must reject the row"
        );
    }

    #[test]
    fn unparsable_filters_narrow_nothing_and_never_error() {
        // junk is skipped (shown next to the input), valid lines still apply
        let rows = {
            let db = temp_db("logs-page-junk");
            seed_logs(&db);
            let out = fetch(&db, &Bar::parse("f=total-junk&f=tool_name%3Dbash"));
            let _ = fs::remove_dir_all(&db.data_dir);
            out.expect("junk filter must not 500")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "claude_code.tool_result");
    }

    #[test]
    fn facets_keep_their_own_filter_off_the_where_and_on_the_rows() {
        let db = temp_db("logs-facets");
        seed_logs(&db);
        let fac = facets(&db, &Bar::parse("severity=info"));

        let sev = fac.iter().find(|x| x.title == "Severity").expect("severity facet");
        let vals: Vec<(&str, &str, &str, bool)> = sev
            .values
            .iter()
            .map(|v| (v.label.as_str(), v.count.as_str(), v.href.as_str(), v.on))
            .collect();
        // own filter off the WHERE: WARN keeps its count even though the
        // page is narrowed to info
        assert!(
            vals.iter().any(|(l, c, _, _)| *l == "WARN" && *c == "1"),
            "WARN stays offered with its true count: {vals:?}"
        );
        // own filter only marks the active row (case-insensitively)
        assert!(
            vals.iter().any(|(l, _, _, on)| *l == "INFO" && *on),
            "the chosen value is highlighted: {vals:?}"
        );
        let warn = vals.iter().find(|(l, ..)| *l == "WARN").expect("WARN row");
        assert_eq!(warn.2, "/logs?severity=WARN", "toggling replaces own");
        // a sibling param rides along in the chip hrefs…
        let both = Bar::parse("severity=info&f=tool_name%3Dbash")
            .href_toggling("/logs", "severity", "WARN", false);
        assert_eq!(both, "/logs?severity=WARN&f=tool_name%3Dbash");
        // …and toggling the selected row off clears just that param
        let off = Bar::parse("severity=info&service=opencode")
            .href_toggling("/logs", "severity", "info", true);
        assert_eq!(off, "/logs?service=opencode");

        // the service facet keeps severity in its hrefs (sibling) and
        // carries no service of its own (own filter off the links)
        let svc = facets(&db, &Bar::parse("severity=info"));
        let svc = svc.iter().find(|x| x.title == "Service").expect("service facet");
        let oc = svc
            .values
            .iter()
            .find(|v| v.label == "opencode")
            .expect("opencode");
        assert_eq!(oc.href, "/logs?severity=info&service=opencode");
        assert!(!oc.on);
        let _ = fs::remove_dir_all(&db.data_dir);
    }
}
