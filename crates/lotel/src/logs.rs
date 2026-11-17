//! Logs explorer: filtered, tail-able view over otel.main.otlp_logs, with
//! top-value facets and a BM25 "best matches" panel (capped_fts) when
//! a text query is active.

use serde::Deserialize;
use serde_json::{Map, Value};

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{View, component, view},
};

use crate::{
    db::Db,
    ui::{live_refresh, sev_class, thousands},
};

/// Query-string filters (`?q=&service=&severity=&range=&limit=`).
#[derive(Debug, Default, Deserialize)]
pub struct LogFilters {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub severity: String,
    /// `1h` | `6h` | `24h` | `7d` | anything else = all time
    #[serde(default)]
    pub range: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

impl LogFilters {
    /// (cutoff timestamp for the WHERE, validated row cap)
    fn bind(&self) -> (String, usize) {
        let hours = match self.range.as_str() {
            "1h" => Some(1),
            "6h" => Some(6),
            "24h" => Some(24),
            "7d" => Some(24 * 7),
            _ => None,
        };
        let cutoff = match hours {
            Some(h) => chrono::Utc::now() - chrono::Duration::hours(h),
            None => chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
        };
        let cutoff = cutoff.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string();
        (cutoff, self.limit.unwrap_or(200).clamp(1, 2000))
    }

    /// Percent-encode one query-string component (filters, facet values).
    fn enc(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }

    /// Link that toggles `param` to `value` (or off, when already `on`),
    /// keeping the other active filters.
    fn href_toggling(&self, param: &str, value: &str, on: bool) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.q.is_empty() {
            parts.push(format!("q={}", Self::enc(&self.q)));
        }
        if !self.range.is_empty() {
            parts.push(format!("range={}", Self::enc(&self.range)));
        }
        for (name, active) in [("service", &self.service), ("severity", &self.severity)] {
            if name == param {
                if !on {
                    parts.push(format!("{name}={}", Self::enc(value)));
                }
            } else if !active.is_empty() {
                parts.push(format!("{name}={}", Self::enc(active)));
            }
        }
        if parts.is_empty() {
            "/logs".to_string()
        } else {
            format!("/logs?{}", parts.join("&"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LogFilters;

    #[test]
    fn all_time_cutoff_is_epoch() {
        let f = LogFilters::default();
        let (cutoff, limit) = f.bind();
        assert_eq!(cutoff, "1970-01-01 00:00:00");
        assert_eq!(limit, 200);
    }

    #[test]
    fn range_and_limit_are_clamped() {
        let f = LogFilters { range: "24h".into(), limit: Some(9999), ..Default::default() };
        let (cutoff, limit) = f.bind();
        assert_ne!(cutoff, "1970-01-01 00:00:00");
        assert_eq!(limit, 2000);

        let f = LogFilters { limit: Some(0), ..Default::default() };
        assert_eq!(f.bind().1, 1);
    }

    #[test]
    fn encoding_covers_the_boring_and_the_nasty() {
        assert_eq!(LogFilters::enc("cart"), "cart");
        assert_eq!(LogFilters::enc("cache miss"), "cache%20miss");
        assert_eq!(LogFilters::enc("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn facet_links_keep_the_rest_of_the_query() {
        let f = LogFilters {
            q: "time out".into(),
            service: "cart".into(),
            severity: "ERROR".into(),
            range: "6h".into(),
            ..Default::default()
        };
        // turning one facet off drops it, keeps the others
        assert_eq!(
            f.href_toggling("severity", "ERROR", true),
            "/logs?q=time%20out&range=6h&service=cart"
        );
        // switching to a value replaces the previous one
        assert_eq!(
            f.href_toggling("severity", "WARN", false),
            "/logs?q=time%20out&range=6h&service=cart&severity=WARN"
        );
        // no filters at all yields a clean path
        let empty = LogFilters::default();
        assert_eq!(empty.href_toggling("service", "cart", false), "/logs?service=cart");
        assert_eq!(empty.href_toggling("service", "cart", true), "/logs");
    }

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
}

/// One rendered log line.
pub struct LogRow {
    pub time: String,
    pub service: String,
    pub severity: String,
    pub body: String,
    pub log_attributes: String,
    pub trace_id: String,
}

/// Shared query for the page and the tail fragment endpoint.
pub(crate) fn fetch(db: &Db, f: &LogFilters) -> anyhow::Result<Vec<LogRow>> {
    let (cutoff, limit) = f.bind();
    let (_, rows) = db.query_rows_with(
        &format!(
            "SELECT time_unix_nano, service_name, severity_text, body, COALESCE(log_attributes, ''), COALESCE(trace_id, '')
             FROM otel.main.otlp_logs
             WHERE time_unix_nano >= CAST(? AS TIMESTAMP)
               AND (? = '' OR body ILIKE '%' || ? || '%')
               AND (? = '' OR service_name = ?)
               AND (? = '' OR upper(severity_text) = upper(?))
             ORDER BY time_unix_nano DESC
             LIMIT {limit}"
        ),
        &[
            &cutoff, &f.q, &f.q, &f.service, &f.service, &f.severity, &f.severity,
        ],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| LogRow {
            time: r[0].clone(),
            service: r[1].clone(),
            severity: r[2].clone(),
            body: r[3].clone(),
            log_attributes: r[4].clone(),
            trace_id: r[5].clone(),
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

/// Top values of severity_text / service_name over the current window.
/// A facet ignores its own dimension's filter — while filtered to ERROR you
/// still see the sibling severities, so one click can switch or clear.
pub(crate) fn facets(db: &Db, f: &LogFilters) -> Vec<Facet> {
    let (cutoff, _) = f.bind();
    [("Severity", "severity", "severity_text"), ("Service", "service", "service_name")]
        .into_iter()
        .map(|(title, param, column)| {
            let (active, other_param, other_column) = if param == "severity" {
                (f.severity.as_str(), "service", "service_name")
            } else {
                (f.service.as_str(), "severity", "severity_text")
            };
            let other_active = if other_param == "service" { &f.service } else { &f.severity };
            // column/param/other_column are our own constants, never input
            let sql = format!(
                "SELECT {column}, count(*) FROM otel.main.otlp_logs
                 WHERE time_unix_nano >= CAST(? AS TIMESTAMP)
                   AND (? = '' OR body ILIKE '%' || ? || '%')
                   AND (? = '' OR {other_column} = ?)
                   AND {column} IS NOT NULL AND {column} != ''
                 GROUP BY 1 ORDER BY 2 DESC, 1 LIMIT 6"
            );
            let rows = db
                .query_rows_with(&sql, &[&cutoff, &f.q, &f.q, other_active, other_active])
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
                        href: f.href_toggling(param, &r[0], on),
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
pub(crate) fn fetch_ranked(db: &Db, f: &LogFilters) -> anyhow::Result<Vec<RankRow>> {
    let (cutoff, _) = f.bind();
    // table-function arguments must be constant at bind time, so q is quoted,
    // not parameterized; every user filter below still binds normally
    let q = crate::db::quote(&f.q);
    let (_, rows) = db.query_rows_with(
        &format!(
            "SELECT time_unix_nano, service_name, severity_text, body, COALESCE(trace_id, ''), score
             FROM capped_fts('otel', 'otlp_logs', {q}, 64)
             WHERE time_unix_nano >= ?
               AND (? = '' OR service_name = ?)
               AND (? = '' OR upper(severity_text) = upper(?))
             ORDER BY score DESC
             LIMIT 8"
        ),
        &[&cutoff, &f.service, &f.service, &f.severity, &f.severity],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| RankRow {
            time: r[0].clone(),
            service: r[1].clone(),
            severity: r[2].clone(),
            // same readable body as the table: the message, not the JSON shell
            body: render_body(&r[3], "").text,
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

/// One compact `key=value` chip under the main text.
pub(crate) struct Chip {
    pub k: String,
    pub v: String,
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
fn entry_chips(map: &Map<String, Value>, decode: bool) -> Vec<Chip> {
    map.iter()
        .map(|(k, v)| Chip {
            k: k.clone(),
            v: chip_value(v, decode),
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
                    Some((m, decode)) => chips.extend(entry_chips(&m, decode)),
                    None => chips.push(Chip {
                        k: format!("#{i}"),
                        v: chip_value(e, true),
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
        let mut attr_chips = entry_chips(map, true);
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

/// A row with its body pre-rendered, so the view only stitches strings.
struct RowView {
    time: String,
    service: String,
    severity: String,
    text: String,
    chips: Vec<Chip>,
    body_json: Option<String>,
    attrs_json: Option<String>,
    trace_id: String,
}

impl RowView {
    fn new(r: LogRow) -> Self {
        let v = render_body(&r.body, &r.log_attributes);
        Self {
            time: r.time,
            service: r.service,
            severity: r.severity,
            text: v.text,
            chips: v.chips,
            body_json: v.body_json,
            attrs_json: v.attrs_json,
            trace_id: r.trace_id,
        }
    }
}

/// The table rows — also served bare by `/api/logs` for the tail poller.
#[component]
pub(crate) async fn log_rows(cx: &Cx, rows: Vec<LogRow>) -> Result<impl View> {
    let rows: Vec<RowView> = rows.into_iter().map(RowView::new).collect();
    Ok(view! {
        for row in rows {
            <tr>
                <td class="muted">(row.time)</td>
                <td>(row.service)</td>
                <td><span class=(format!("badge {}", sev_class(&row.severity)))>(row.severity)</span></td>
                <td class="logbody">
                    if row.body_json.is_some() || row.attrs_json.is_some() {
                        <details class="logx">
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
                                <span class="kchip"><b>(c.k.clone())</b>"="(c.v.clone())</span>
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
    let f = parse_query_params::<LogFilters>(cx).unwrap_or_default();

    let rows = fetch(db, &f).unwrap_or_default();
    let services = option_values(db, "service_name");
    let severities = option_values(db, "severity_text");
    let facet_groups = facets(db, &f);

    let (ranked_rows, ranked_note) = if f.q.is_empty() {
        (Vec::new(), None)
    } else {
        match fetch_ranked(db, &f) {
            Ok(rs) if !rs.is_empty() => (rs, None),
            Ok(_) => (
                Vec::new(),
                Some(
                    "Nothing indexed for this query yet — fresh rows are still being sealed; \
                     the list below matches on raw text meanwhile."
                        .to_string(),
                ),
            ),
            Err(_) => (
                Vec::new(),
                Some(
                    "Search index unavailable — the list below still matches on raw text."
                        .to_string(),
                ),
            ),
        }
    };

    Ok(view! {
        <h1>"Logs"</h1>
        <p class="sub">"Recent log records from the capped store. Tail updates in place."</p>

        <form class="filters" method="get" action="/logs">
            <label>"Search"
                <input type="search" name="q" placeholder="body contains…" value=(f.q.clone()) />
            </label>
            <label>"Service"
                <select name="service">
                    <option value="" if f.service.is_empty() { selected="selected" }>"all"</option>
                    for s in &services {
                        <option value=(s.clone()) if *s == f.service { selected="selected" }>(s.clone())</option>
                    }
                </select>
            </label>
            <label>"Severity"
                <select name="severity">
                    <option value="" if f.severity.is_empty() { selected="selected" }>"all"</option>
                    for s in &severities {
                        <option value=(s.clone()) if *s == f.severity { selected="selected" }>(s.clone())</option>
                    }
                </select>
            </label>
            <label>"Time range"
                <select name="range">
                    <option value="1h" if f.range == "1h" { selected="selected" }>"1 hour"</option>
                    <option value="6h" if f.range == "6h" { selected="selected" }>"6 hours"</option>
                    <option value="24h" if f.range == "24h" { selected="selected" }>"24 hours"</option>
                    <option value="7d" if f.range == "7d" { selected="selected" }>"7 days"</option>
                    <option value="all" if !["1h", "6h", "24h", "7d"].contains(&f.range.as_str()) { selected="selected" }>"all"</option>
                </select>
            </label>
            <button type="submit">"Apply"</button>
            <label class="tail">
                <input type="checkbox" id="tail-toggle" checked="checked" />
                "live tail"
            </label>
        </form>

        if !f.q.is_empty() {
            ranked_panel(rows: ranked_rows, note: ranked_note)
        }

        facet_panel(facets: facet_groups)

        <table>
            <thead>
                <tr><th>"time"</th><th>"service"</th><th>"severity"</th><th>"body"</th><th></th></tr>
            </thead>
            <tbody id="logrows">
                log_rows(rows: rows)
            </tbody>
        </table>

        live_refresh(topic: "logs".to_string(), target: "logrows".to_string(), toggle: "tail-toggle".to_string(), cadence: 3000)
    })
}
