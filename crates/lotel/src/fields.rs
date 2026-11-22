//! Fields panel (T2b): what the bar's rows actually carry.
//!
//! Telemetry attribute bags differ per SDK and version (Claude Code's
//! `cost_usd` on events, Codex's `conversation.id`, opencode's
//! `opencode.run`), so the panel lists every key the filter bar can address
//! in the page's columns — bare (the row's own bag), `resource.`, and
//! `body.` on logs — with coverage, JSON type mix, approximate distinct
//! count, numeric stats and top values. Keys are links the bar understands:
//! each top value adds `key=value` (like the chips), each numeric key links
//! into Explore under its `@col:key` form.
//!
//! * **Bounded**: only the latest [`PROFILE_ROWS`] rows of the current
//!   selection (same range, service, free text and `f` filters as the page's
//!   table) are profiled, and the panel says so.
//! * **Privacy first**: every bag goes through `Config::display` (the single
//!   redaction path) *before* a key or value is read from it, so a
//!   `CONTENT_KEYS` value never leaves the store on this panel — the counts
//!   and the key survive, the value reads `[REDACTED]`. Profiling therefore
//!   walks the masked bags in Rust: extracting a single value SQL-side
//!   would strip away the key context the redactor needs, and the bind-everything
//!   rule stays satisfied a fortiori — no attribute key or value ever enters the
//!   SQL text, which only ever carries the bar's bound `?` parameters.

use std::collections::HashMap;

use serde_json::{Map, Value};

use topcoat::{
    Result,
    context::Cx,
    view::{View, component, view},
};

use crate::{
    config::Config,
    db::Db,
    filters::{AttrSource, Bar, LOG_COLS, canonical_of},
    traces::TraceFilters,
    ui::thousands,
};

/// Rows the panel profiles at most — the latest N of the current selection.
pub(crate) const PROFILE_ROWS: usize = 5_000;
/// Top values listed per string key.
const TOP_VALUES: usize = 5;
/// The distinct-value tally stops growing here; beyond it the count reads
/// "~N+" — a `session.id` over 5,000 rows must not grow a map per row.
const DISTINCT_CAP: usize = 1_000;
/// Top values clip at this width; a clipped value filters by substring
/// (its tail is hidden), exactly like the row chips' links.
const VALUE_CLIP: usize = 48;

/// Names that make a numeric key a "Measure": the numbers an agent operator
/// reads first — cost, tokens, latency, sizes, counts.
const MEASURE_HINTS: &[&str] = &["cost", "usd", "token", "duration", "_ms", "bytes", "count", "size"];
/// Measures shown above the list (page 1 only); the rest join the list.
const MEASURES_SHOWN: usize = 8;
/// Keys per page of the list (`fp`, 1-based).
pub(crate) const PAGE_KEYS: usize = 20;

/// Which page the panel sits on: drives the table, the value links' path and
/// the Explore dataset/column of the numeric links.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Page {
    Logs,
    Traces,
}

impl Page {
    fn path(self) -> &'static str {
        match self {
            Page::Logs => "/logs",
            Page::Traces => "/traces",
        }
    }
    fn dataset(self) -> &'static str {
        match self {
            Page::Logs => "logs",
            Page::Traces => "traces",
        }
    }
    /// The Explore attribute column the row bag's keys ride in.
    fn row_col(self) -> &'static str {
        match self {
            Page::Logs => "log_attributes",
            Page::Traces => "span_attributes",
        }
    }
}

/// One displayed top value with its row count and the bar link that filters
/// to it (built from the displayed value, the way `query::facets` builds its
/// drill links).
#[derive(Debug, Clone)]
pub(crate) struct TopValue {
    pub v: String,
    pub count: String,
    pub href: String,
}

/// min / median / max / sum over a numeric key's number-typed values.
#[derive(Debug, Clone)]
pub(crate) struct Numeric {
    pub min: String,
    pub med: String,
    pub max: String,
    pub sum: String,
}

/// One profiled key as the panel lists it.
#[derive(Debug, Clone)]
pub(crate) struct FieldStat {
    /// the key in bar syntax: bare, `resource.` or `body.` prefixed
    pub label: String,
    /// rows of the profiled window carrying the key, and the % of them
    pub rows: u64,
    pub pct: u32,
    pub width: u32,
    /// the JSON type mix, e.g. `number 8 · string 42`
    pub types: String,
    /// approximate distinct count ("12" or "1000+")
    pub distinct: String,
    /// set when the key carries number-typed values
    pub numeric: Option<Numeric>,
    /// top values — listed for keys carrying text/bool/object values
    pub top: Vec<TopValue>,
    /// numeric keys open Explore grouped by their `@col:key`
    pub explore: Option<String>,
    /// the one-line summary shown before the key is opened
    pub summary: String,
    /// every sampled value was "", null, 0, false, {} or [] — hidden unless `fe=1`
    pub empty: bool,
    /// bar prefix order: row bag, body, resource
    source_rank: u8,
}

/// The whole panel: what it was profiled from, the Measures band, the
/// filtered key list and the window of it this page shows, plus the
/// panel's own controls (`ff` name filter, `fp` page, `fe` show empty).
/// Those three params ride in the URL next to the bar's, which ignores them.
#[derive(Debug)]
pub(crate) struct Panel {
    pub rows: usize,
    /// shown above the list, page 1 only
    pub measures: Vec<FieldStat>,
    /// every non-measure key that passed the filter (and the empty rule)
    pub keys: Vec<FieldStat>,
    /// this page's window into `keys`
    pub start: usize,
    pub end: usize,
    pub prev: Option<String>,
    pub next: Option<String>,
    /// "N empty fields hidden" (or "hide empty fields") and the link flipping `fe`
    pub empty_toggle: Option<(String, String)>,
    /// the filter box: its value, form action and the params it carries
    pub filter: String,
    pub action: String,
    pub keep: Vec<(String, String)>,
    /// rendered open when the URL carries any panel param
    pub open: bool,
}

// ————————————————————————————————————————————— profiling ——————————————————————————————————————————————

/// Tally for one key inside one source bag. Kind indices: 0 number, 1 bool,
/// 2 string, 3 object.
#[derive(Default)]
struct Agg {
    rows: u64,
    /// values other than "", null, 0, false, {} and []
    meaningful: u64,
    kinds: [u64; 4],
    values: HashMap<String, u64>,
    values_full: bool,
    nums: Vec<f64>,
}

impl Agg {
    fn record(&mut self, v: &Value) {
        self.rows += 1;
        if !is_blank(v) {
            self.meaningful += 1;
        }
        // a string that reads as a number is one: Claude Code sends some
        // measures (duration_ms) as "1922"
        let num = match v {
            Value::Number(_) => v.as_f64(),
            Value::String(s) => numeric_str(s),
            _ => None,
        };
        match (v, num) {
            (_, Some(f)) => {
                self.kinds[0] += 1;
                self.nums.push(f);
            }
            (Value::Number(_), None) => self.kinds[0] += 1,
            (Value::Bool(_), _) => self.kinds[1] += 1,
            (Value::String(_), _) => self.kinds[2] += 1,
            _ => self.kinds[3] += 1, // object, array, null — all "object" to the mix
        }
        let text = value_text(v);
        if self.values.len() < DISTINCT_CAP || self.values.contains_key(&text) {
            *self.values.entry(text).or_default() += 1;
        } else {
            self.values_full = true;
        }
    }
}

/// A string holding a finite number ("1922", " 0.5 "), as that number.
fn numeric_str(s: &str) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    t.parse::<f64>().ok().filter(|f| f.is_finite())
}

/// A value that says nothing: "", null, 0 (or "0"), false, {} and [].
fn is_blank(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.trim().is_empty() || numeric_str(s) == Some(0.0),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// A JSON value as the value text the panel counts and lists. Strings are
/// the text they are; anything else keeps its compact JSON form.
fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

type Aggs = HashMap<String, Agg>;

/// Members of one masked row/resource bag, counted into `aggs`.
fn walk_object(map: &Map<String, Value>, aggs: &mut Aggs) {
    for (k, v) in map {
        aggs.entry(k.clone()).or_default().record(v);
    }
}

/// Profile one attribute bag: the row's own or the resource bag. The text
/// goes through the single redaction path BEFORE any key or value is read.
fn profile_bag(text: &str, cfg: &Config, aggs: &mut Aggs) {
    let masked = cfg.display(text);
    if let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&masked) {
        walk_object(&map, aggs);
    }
}

/// Profile one log body the way the bar addresses it (`AttrSource::Body`):
/// an object's own members, or — for the `[message, "<payload>"]` shape —
/// the members of each element decoded exactly one level, the first string
/// element being the message (the chips' split).
fn profile_body(text: &str, cfg: &Config, aggs: &mut Aggs) {
    if text.is_empty() {
        return;
    }
    // The `cli starting` shape is read from the raw row: its payload keys
    // are listed with their counts, but every payload value is content — the
    // one real prompt leak in the capture — so the private default
    // lists each value as REDACTED. That is exactly the rule redact_cell
    // applies to the body whole, taken one level deeper; the opt-in shows
    // the values again.
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text)
        && items
            .first()
            .and_then(Value::as_str)
            .is_some_and(crate::agents::is_cli_starting)
    {
        let show = cfg.agent_show_content;
        for e in items.iter().skip(1) {
            let obj = match e {
                Value::Object(m) => Some(m.clone()),
                Value::String(s) => serde_json::from_str::<Value>(s)
                    .ok()
                    .and_then(|v| v.as_object().cloned()),
                _ => None,
            };
            if let Some(m) = obj {
                for (k, v) in m {
                    let hidden;
                    let v = if show {
                        &v
                    } else {
                        hidden = Value::String(crate::agents::REDACTED.to_string());
                        &hidden
                    };
                    aggs.entry(k.clone()).or_default().record(v);
                }
            }
        }
        return;
    }
    let masked = cfg.display(text);
    match serde_json::from_str::<Value>(&masked) {
        Ok(Value::Object(map)) => walk_object(&map, aggs),
        Ok(Value::Array(items)) => {
            let main = items.iter().position(Value::is_string);
            for (i, e) in items.iter().enumerate() {
                if main == Some(i) {
                    continue;
                }
                match e {
                    Value::Object(m) => walk_object(m, aggs),
                    Value::String(s) => {
                        // The payload string was not walked by the bag-level
                        // mask (it reached it as a string), so it goes
                        // through the same `display` again at its own level —
                        // still the one masking path, applied wherever a
                        // JSON level is decoded.
                        if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&cfg.display(s))
                        {
                            walk_object(&m, aggs);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

fn fmt_num(x: f64) -> String {
    if !x.is_finite() {
        return "—".to_string();
    }
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{x:.0}")
    } else {
        format!("{x:.4}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// min / median / max / sum over the key's number-typed values.
fn numeric_stats(nums: &mut [f64]) -> Numeric {
    nums.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let med = match nums.len() {
        0 => f64::NAN,
        n if n % 2 == 1 => nums[n / 2],
        n => (nums[n / 2 - 1] + nums[n / 2]) / 2.0,
    };
    let sum = nums.iter().sum();
    Numeric {
        min: fmt_num(nums.first().copied().unwrap_or(f64::NAN)),
        med: fmt_num(med),
        max: fmt_num(nums.last().copied().unwrap_or(f64::NAN)),
        sum: fmt_num(sum),
    }
}

fn types_label(kinds: &[u64; 4]) -> String {
    const NAMES: [&str; 4] = ["number", "bool", "string", "object"];
    NAMES
        .iter()
        .zip(kinds)
        .filter(|(_, n)| **n > 0)
        .map(|(name, n)| format!("{name} {n}"))
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The Explore URL grouping the dataset by this key's `@col:key` read —
/// the same URL shape `query::build_url` produces, nothing new.
fn explore_url(bar: &Bar, page: Page, col: &str) -> String {
    let group = serde_json::to_string(&vec![col]).unwrap_or_else(|_| "[]".into());
    format!(
        "/query?dataset={}&range={}&measures={}&filters={}&group={}&tab=",
        Bar::enc(page.dataset()),
        Bar::enc(bar.get("range")),
        Bar::enc(r#"["count"]"#),
        Bar::enc("[]"),
        Bar::enc(&group),
    )
}

fn clip(s: &str, limit: usize) -> String {
    match s.char_indices().nth(limit) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Turn one source's tallies into sorted stats: coverage desc, then label.
fn stats_of(
    aggs: Aggs,
    source: AttrSource,
    prefix: &str,
    total: usize,
    bar: &Bar,
    page: Page,
) -> Vec<FieldStat> {
    let mut out: Vec<FieldStat> = aggs
        .into_iter()
        .map(|(key, agg)| {
            let label = format!("{prefix}{key}");
            let pct = if total == 0 {
                0
            } else {
                ((agg.rows as f64 / total as f64) * 100.0).round() as u32
            };
            let numeric = if agg.kinds[0] > 0 {
                let mut nums = agg.nums;
                Some(numeric_stats(&mut nums))
            } else {
                None
            };
            let numeric_first = agg.kinds[0] * 2 >= agg.rows;
            let top = if numeric_first {
                Vec::new()
            } else {
                let mut vals: Vec<(&String, &u64)> = agg.values.iter().collect();
                vals.sort_by(|a, b| (b.1, a.0).cmp(&(a.1, b.0)));
                vals.into_iter()
                    .take(TOP_VALUES)
                    .map(|(v, c)| {
                        let shown = clip(v, VALUE_CLIP);
                        // The link is built from the displayed value, the way
                        // query.rs's facets() does; a clipped value filters
                        // by substring like the chips.
                        let (op, value) = match shown.strip_suffix('…') {
                            Some(tail) => (crate::filters::AttrOp::Has, tail),
                            None => (crate::filters::AttrOp::Eq, shown.as_str()),
                        };
                        TopValue {
                            href: bar
                                .href_add_filter(page.path(), &canonical_of(source, op, &key, value)),
                            v: shown,
                            count: c.to_string(),
                        }
                    })
                    .collect()
            };
            let explore = match (numeric_first, source) {
                (true, AttrSource::Row) => {
                    Some(explore_url(bar, page, &format!("@{}:{}", page.row_col(), key)))
                }
                (true, AttrSource::Resource) => Some(explore_url(
                    bar,
                    page,
                    &format!("@resource_attributes:{key}"),
                )),
                // Explore has no body column to address; the value links carry
                // the body keys.
                _ => None,
            };
            let distinct = format!("{}{}", agg.values.len(), if agg.values_full { "+" } else { "" });
            let summary = match (&numeric, top.first()) {
                (Some(n), _) if numeric_first => format!("sum {} · median {}", n.sum, n.med),
                (_, Some(t)) if distinct == "1" => format!("{} ×{}", t.v, thousands(&t.count)),
                (_, Some(t)) => format!("{} ×{} · {distinct} distinct", t.v, thousands(&t.count)),
                _ => types_label(&agg.kinds),
            };
            FieldStat {
                label,
                rows: agg.rows,
                pct,
                width: pct.clamp(if agg.rows > 0 { 1 } else { 0 }, 100),
                types: types_label(&agg.kinds),
                distinct,
                numeric,
                top,
                explore,
                summary,
                empty: agg.meaningful == 0,
                source_rank: match source {
                    AttrSource::Row => 0,
                    AttrSource::Body => 1,
                    AttrSource::Resource => 2,
                },
            }
        })
        .collect();
    out.sort_by(|a, b| (b.rows, &a.label).cmp(&(b.rows, &b.label)));
    out
}

/// Assemble the panel from one selection's bag columns (row bag, resource
/// bag, and the body on logs), then apply the panel's own controls from the
/// bar's URL: `ff` (key-name filter), `fe=1` (list empty keys too) and `fp`
/// (page of the list).
fn build(rows: Vec<Vec<String>>, bar: &Bar, cfg: &Config, page: Page) -> Panel {
    let total = rows.len();
    let mut row_aggs: Aggs = HashMap::new();
    let mut res_aggs: Aggs = HashMap::new();
    let mut body_aggs: Aggs = HashMap::new();
    for r in &rows {
        profile_bag(&r[0], cfg, &mut row_aggs);
        profile_bag(&r[1], cfg, &mut res_aggs);
        if let Some(b) = r.get(2) {
            profile_body(b, cfg, &mut body_aggs);
        }
    }
    let mut sources: Vec<(AttrSource, &'static str, Aggs)> = vec![
        (AttrSource::Row, "", row_aggs),
        (AttrSource::Resource, "resource.", res_aggs),
    ];
    if page == Page::Logs {
        sources.push((AttrSource::Body, "body.", body_aggs));
    }

    // value links leave the page number behind: a new filter starts at page 1
    let link_bar = Bar::parse(bar.href_without("", &["fp"]).trim_start_matches('?'));
    let filter = bar.get("ff").trim().to_string();
    let needle = filter.to_lowercase();
    let show_empty = bar.get("fe") == "1";

    let mut hidden_empty = 0;
    let mut listed: Vec<FieldStat> = Vec::new();
    for (source, prefix, aggs) in sources {
        for f in stats_of(aggs, source, prefix, total, &link_bar, page) {
            if !needle.is_empty() && !f.label.to_lowercase().contains(&needle) {
                continue;
            }
            if f.empty && !show_empty {
                hidden_empty += 1;
                continue;
            }
            listed.push(f);
        }
    }

    // Measures: numeric keys whose name reads like one, cost and tokens first
    let (mut measures, mut keys): (Vec<FieldStat>, Vec<FieldStat>) = listed
        .into_iter()
        .partition(|f| f.explore.is_some() && measure_hint(&f.label));
    measures.sort_by(|a, b| {
        (measure_rank(&a.label), b.rows, &a.label).cmp(&(measure_rank(&b.label), a.rows, &b.label))
    });
    if measures.len() > MEASURES_SHOWN {
        keys.extend(measures.split_off(MEASURES_SHOWN));
    }
    keys.sort_by(|a, b| (a.source_rank, b.rows, &a.label).cmp(&(b.source_rank, a.rows, &b.label)));

    let pages = keys.len().div_ceil(PAGE_KEYS).max(1);
    let fp = bar.get("fp").parse::<usize>().unwrap_or(1).clamp(1, pages);
    let start = (fp - 1) * PAGE_KEYS;
    let end = (start + PAGE_KEYS).min(keys.len());
    let page_href = |n: usize| {
        let v = if n == 1 { String::new() } else { n.to_string() };
        format!("{}#fields", bar.href(page.path(), &[("fp", &v)]))
    };
    let empty_toggle = if show_empty {
        Some((
            "hide empty fields".to_string(),
            format!("{}#fields", bar.href(page.path(), &[("fe", ""), ("fp", "")])),
        ))
    } else if hidden_empty > 0 {
        Some((
            format!(
                "{hidden_empty} empty field{} hidden",
                if hidden_empty == 1 { "" } else { "s" }
            ),
            format!("{}#fields", bar.href(page.path(), &[("fe", "1"), ("fp", "")])),
        ))
    } else {
        None
    };
    Panel {
        rows: total,
        measures: if fp == 1 { measures } else { Vec::new() },
        start,
        end,
        prev: (fp > 1).then(|| page_href(fp - 1)),
        next: (fp < pages).then(|| page_href(fp + 1)),
        keys,
        empty_toggle,
        filter,
        action: format!("{}#fields", page.path()),
        keep: bar
            .pairs()
            .iter()
            .filter(|(k, v)| k != "ff" && k != "fp" && !v.is_empty())
            .cloned()
            .collect(),
        open: bar.pairs().iter().any(|(k, _)| k == "ff" || k == "fp" || k == "fe"),
    }
}

/// cost/usd first, then tokens, then the other measure names.
fn measure_rank(label: &str) -> u8 {
    let l = label.to_lowercase();
    if l.contains("cost") || l.contains("usd") {
        0
    } else if l.contains("token") {
        1
    } else {
        2
    }
}

fn measure_hint(label: &str) -> bool {
    let l = label.to_lowercase();
    MEASURE_HINTS.iter().any(|h| l.contains(h))
}

/// Fetch the page's selection (same WHERE as the page's own table query, so
/// the panel profiles exactly the rows the bar selects) and profile it.
pub(crate) fn profile_logs(db: &Db, bar: &Bar, cfg: &Config) -> Panel {
    let cutoff = bar.cutoff();
    let q = bar.get("q");
    let service = bar.get("service");
    let severity = bar.get("severity");
    let (conds, attr_params) = bar.attr_conds(&LOG_COLS);

    // the logs page's fetch() WHERE, verbatim — same selection, same ? order
    let mut sql = String::from(
        "SELECT COALESCE(log_attributes, ''), COALESCE(resource_attributes, ''), COALESCE(body, '')
         FROM otel.main.otlp_logs
         WHERE time_unix_nano >= CAST(? AS TIMESTAMP)
           AND (? = '' OR body ILIKE '%' || ? || '%')
           AND (? = '' OR service_name = ?)
           AND (? = '' OR upper(severity_text) = upper(?))",
    );
    for c in &conds {
        sql.push_str(&format!(" AND {c}"));
    }
    sql.push_str(&format!(
        " ORDER BY time_unix_nano DESC LIMIT {PROFILE_ROWS}"
    ));

    let mut params: Vec<&dyn duckdb::ToSql> =
        vec![&cutoff, &q, &q, &service, &service, &severity, &severity];
    params.extend(attr_params.iter().map(|s| s as &dyn duckdb::ToSql));
    let rows = db.query_rows_with(&sql, &params).map(|(_, r)| r).unwrap_or_default();
    build(rows, bar, cfg, Page::Logs)
}

/// The traces selection: the feed's own window — time range plus the span
/// name, status, min-duration and attribute conditions the feed applies.
pub(crate) fn profile_traces(db: &Db, bar: &Bar, cfg: &Config) -> Panel {
    let tf = TraceFilters::from(bar.clone());
    let (conds, params) = tf.conds();
    let cutoff = bar.cutoff();
    let mut sql = String::from(
        "SELECT COALESCE(span_attributes, ''), COALESCE(resource_attributes, '')
         FROM otel.main.otlp_traces
         WHERE start_time_unix_nano >= CAST(? AS TIMESTAMP)",
    );
    for c in &conds {
        sql.push_str(&format!(" AND {c}"));
    }
    sql.push_str(&format!(
        " ORDER BY start_time_unix_nano DESC LIMIT {PROFILE_ROWS}"
    ));
    let mut bind: Vec<&dyn duckdb::ToSql> = vec![&cutoff];
    bind.extend(params.iter().map(|s| s as &dyn duckdb::ToSql));
    let rows = db.query_rows_with(&sql, &bind).map(|(_, r)| r).unwrap_or_default();
    build(rows, bar, cfg, Page::Traces)
}

// ——————————————————————————————————————————————— view ————————————————————————————————————————————————

/// The Fields panel: a `<details>` (works with JS off). Closed it is one
/// line; open, a name filter, the Measures band, then one compact line per
/// key (itself a `<details>` holding the stats, value links and the Explore
/// link), 20 keys a page, empty keys hidden behind a link.
#[component]
pub(crate) async fn fields_panel(cx: &Cx, panel: Panel) -> Result<impl View> {
    let listed = panel.keys.len() + panel.measures.len();
    let range = if panel.keys.is_empty() {
        String::new()
    } else {
        format!(
            "fields {}–{} of {}",
            panel.start + 1,
            panel.end,
            thousands(&panel.keys.len().to_string())
        )
    };
    let page: Vec<FieldStat> = panel.keys.into_iter().skip(panel.start).take(panel.end - panel.start).collect();
    Ok(view! {
        <details class="card fields" id="fields" if panel.open { open="open" }>
            <summary>
                <b>"Fields"</b>
                <span class="muted">
                    (format!(
                        "{} with values · from the latest {} rows · {} in this selection",
                        thousands(&listed.to_string()),
                        thousands(&PROFILE_ROWS.to_string()),
                        thousands(&panel.rows.to_string()),
                    ))
                </span>
            </summary>
            <form class="ffilter" method="get" action=(panel.action.clone())>
                for (k, v) in &panel.keep {
                    <input type="hidden" name=(k.clone()) value=(v.clone()) />
                }
                <input type="search" name="ff" value=(panel.filter.clone()) placeholder="filter fields, e.g. token" aria-label="Filter fields by name" />
                <button type="submit">"Filter"</button>
            </form>
            if listed == 0 {
                <p class="muted">"No fields with values in this selection."</p>
            }
            if !panel.measures.is_empty() {
                <h3>"Measures"</h3>
                for f in &panel.measures {
                    field_line(f: f.clone())
                }
            }
            if !page.is_empty() {
                <h3>"All fields"</h3>
                for f in &page {
                    field_line(f: f.clone())
                }
            }
            <p class="fpager">
                if let Some(h) = &panel.prev {
                    <a href=(h.clone())>"previous"</a>
                }
                <span class="muted">(range)</span>
                if let Some(h) = &panel.next {
                    <a href=(h.clone())>"next"</a>
                }
                if let Some((text, h)) = &panel.empty_toggle {
                    <a class="muted" href=(h.clone())>(text.clone())</a>
                }
            </p>
        </details>
    })
}

/// One key: a closed line (name, coverage, summary) that opens on its stats,
/// its top values as filter links and, for numbers, the Explore link.
#[component]
async fn field_line(cx: &Cx, f: FieldStat) -> Result<impl View> {
    let meta = match &f.numeric {
        Some(n) => format!(
            "{} · {} distinct · min {} · median {} · max {} · sum {}",
            f.types, f.distinct, n.min, n.med, n.max, n.sum
        ),
        None => format!("{} · {} distinct", f.types, f.distinct),
    };
    Ok(view! {
        <details class="fline">
            <summary>
                <span class="facet-k mono">(f.label.clone())</span>
                <span class="facet-bar"><i style=(format!("width:{}%", f.width))></i></span>
                <span class="facet-n">(format!("{}%", f.pct))</span>
                <span class="fsum">(f.summary.clone())</span>
            </summary>
            <div class="fmeta">(format!("{} rows · {meta}", thousands(&f.rows.to_string())))</div>
            if let Some(e) = &f.explore {
                <a class="fval" href=(e.clone())><span>"chart it in Explore"</span></a>
            }
            if !f.top.is_empty() {
                <div class="fvals">
                    for t in &f.top {
                        <a class="fval" href=(t.href.clone())>
                            <span>(t.v.clone())</span>
                            <span class="facet-n">(thousands(&t.count))</span>
                        </a>
                    }
                </div>
            }
        </details>
    })
}

/// The panel entry for a key, whatever group it landed in — the tests'
/// lookup against the built (never rendered) panel.
#[cfg(test)]
pub(crate) fn find<'a>(panel: &'a Panel, label: &str) -> Option<&'a FieldStat> {
    panel.measures.iter().chain(panel.keys.iter()).find(|f| f.label == label)
}

#[cfg(test)]
mod tests {
    use super::{Page, Panel, build, explore_url, find};
    use crate::{agents::REDACTED, config::Config, filters::Bar};
    use std::path::PathBuf;

    fn cfg(show_content: bool) -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 0,
            data_dir: PathBuf::from("/tmp/lotel-fields-test"),
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

    /// Profile (row_attributes, resource_attributes, body) tuples with an
    /// empty bar — the page filter is exercised against real tables in
    /// db_tests, here it is the panel itself under test.
    fn profile(rows: Vec<(&str, &str, &str)>, show: bool) -> Panel {
        build(
            rows.into_iter().map(|(a, r, b)| vec![a.into(), r.into(), b.into()]).collect(),
            &Bar::parse(""),
            &cfg(show),
            Page::Logs,
        )
    }

    fn dump(p: &Panel) -> String {
        format!("{p:?}")
    }

    #[test]
    fn numeric_keys_get_stats_and_lead_as_measures() {
        let p = profile(
            vec![
                (r#"{"duration_ms":10,"model":"opus"}"#, "{}", ""),
                (r#"{"duration_ms":20,"model":"opus"}"#, "{}", ""),
                (r#"{"duration_ms":60,"model":"sonnet"}"#, "{}", ""),
            ],
            false,
        );
        assert_eq!(p.rows, 3);
        let d = find(&p, "duration_ms").expect("duration_ms profiled");
        assert_eq!((d.rows, d.pct), (3, 100));
        assert_eq!(d.types, "number 3");
        let n = d.numeric.as_ref().expect("numeric stats");
        assert_eq!((n.min.as_str(), n.med.as_str(), n.max.as_str(), n.sum.as_str()), ("10", "20", "60", "90"));
        assert!(
            d.explore.as_deref().unwrap().contains("%40log_attributes%3Aduration_ms"),
            "numeric keys open Explore on @col:key: {:?}",
            d.explore
        );
        assert_eq!(p.measures[0].label, "duration_ms", "measures lead the panel");
        // a string key lists top values as bar links, never Explore
        let m = find(&p, "model").unwrap();
        assert_eq!((m.top[0].v.as_str(), m.top[0].count.as_str()), ("opus", "2"));
        assert!(m.top[0].href.contains("f=model%3Dopus"), "{}", m.top[0].href);
        assert!(m.explore.is_none());
    }

    #[test]
    fn content_keys_keep_counts_values_are_masked() {
        let p = profile(
            vec![
                (r#"{"prompt":"canary-synthetic-prompt-7c1","prompt_length":18,"session.id":"s1"}"#, "{}", ""),
                (r#"{"session.id":"s1"}"#, "{}", ""),
            ],
            false,
        );
        let pr = find(&p, "prompt").expect("the key is listed…");
        assert_eq!(pr.rows, 1, "…with its count");
        assert_eq!(pr.top[0].v, REDACTED, "…its value is the mask");
        assert!(pr.top[0].href.contains("REDACTED"), "the link rides the displayed value too");
        assert!(
            !dump(&p).contains("canary-synthetic") && !dump(&p).contains("7c1"),
            "no prompt text on the panel: {}",
            dump(&p)
        );
        let pl = find(&p, "prompt_length").expect("the numeric companion survives");
        assert!(pl.top.is_empty(), "a numeric key shows stats, not top values");
        let n = pl.numeric.as_ref().expect("its stats are real");
        assert_eq!((n.min.as_str(), n.max.as_str()), ("18", "18"));
    }

    #[test]
    fn cli_starting_payload_lists_keys_hides_values_until_opt_in() {
        let body = r#"["cli starting","{\"cmd\":\"canary-command-9b2\",\"tool_input\":{\"secret\":\"s3cr3t\"}}"]"#;
        let p = profile(vec![(r#"{"role":"cli"}"#, "{}", body)], false);
        let cmd = find(&p, "body.cmd").expect("payload keys are listed with counts");
        assert_eq!(cmd.rows, 1);
        assert_eq!(cmd.top[0].v, REDACTED);
        assert!(find(&p, "body.tool_input").is_some(), "content keys are listed too");
        let d = dump(&p);
        assert!(!d.contains("canary-command") && !d.contains("s3cr3t"), "payload stayed hidden: {d}");
        // content opt-in: the operator sees what was sent
        let shown = profile(vec![(r#"{"role":"cli"}"#, "{}", body)], true);
        assert_eq!(find(&shown, "body.cmd").unwrap().top[0].v, "canary-command-9b2");
    }

    #[test]
    fn resource_and_body_prefixes_group_and_link_like_the_bar() {
        let p = profile(
            vec![
                ("{}", r#"{"service.name":"cart","opencode.run":"r7"}"#, r#"{"queue":"cart"}"#),
                ("{}", r#"{"service.name":"cart"}"#, ""),
            ],
            false,
        );
        let s = find(&p, "resource.service.name").unwrap();
        assert_eq!((s.rows, s.pct), (2, 100));
        assert_eq!(s.types, "string 2");
        assert!(s.top[0].href.contains("f=resource.service.name%3Dcart"), "{}", s.top[0].href);
        let q = find(&p, "body.queue").unwrap();
        assert_eq!(q.top[0].v, "cart");
        assert!(q.top[0].href.contains("f=body.queue%3Dcart"), "body keys filter with the body. prefix");
        // the list runs row bag, then body, then resource keys
        let labels: Vec<&str> = p.keys.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["body.queue", "resource.service.name", "resource.opencode.run"]);
    }

    #[test]
    fn bool_and_object_values_take_their_place_in_the_mix() {
        let p = profile(vec![(r#"{"has_hooks":true,"ctx":{"a":1}}"#, "{}", "")], false);
        assert_eq!(find(&p, "has_hooks").unwrap().types, "bool 1");
        let ctx = find(&p, "ctx").unwrap();
        assert_eq!(ctx.types, "object 1");
        assert_eq!(ctx.top[0].v, r#"{"a":1}"#);
    }

    #[test]
    fn junk_and_plain_text_never_profile_as_keys() {
        let p = profile(vec![("not json", "null", "plain text line"), ("", "", "")], false);
        assert_eq!(p.rows, 2, "the rows still count");
        assert!(p.measures.is_empty() && p.keys.is_empty(), "but nothing is addressable: {p:?}");
    }

    #[test]
    fn top_value_links_round_trip_values_that_look_like_syntax() {
        let p = profile(vec![(r#"{"token":"a=b"}"#, "{}", "")], false);
        let href = &find(&p, "token").unwrap().top[0].href;
        assert!(href.contains("f=token%3Da%3Db"), "value keeps its '=' through the link: {href}");
    }

    fn profile_with(rows: Vec<&str>, query: &str) -> Panel {
        build(
            rows.into_iter().map(|a| vec![a.into(), "{}".into(), String::new()]).collect(),
            &Bar::parse(query),
            &cfg(false),
            Page::Logs,
        )
    }

    #[test]
    fn empty_keys_hide_until_asked_for() {
        let rows = vec![
            r#"{"model":"opus","cache_5m_tokens":0,"note":"","flag":false,"ctx":{},"tags":[],"gone":null}"#,
            r#"{"model":"opus","cache_5m_tokens":"0"}"#,
        ];
        let p = profile_with(rows.clone(), "");
        let listed: Vec<&str> = p.measures.iter().chain(&p.keys).map(|f| f.label.as_str()).collect();
        assert_eq!(listed, ["model"], "only the key with a value is listed");
        let (text, href) = p.empty_toggle.clone().expect("the hidden ones are announced");
        assert_eq!(text, "6 empty fields hidden");
        assert!(href.contains("fe=1"), "{href}");
        assert!(!p.open, "a plain page keeps the panel closed");

        let all = profile_with(rows, "fe=1");
        assert_eq!(all.measures.len() + all.keys.len(), 7, "fe=1 lists them all");
        assert_eq!(all.empty_toggle.as_ref().unwrap().0, "hide empty fields");
        assert!(all.open, "the panel stays open on its own params");
    }

    #[test]
    fn name_filter_narrows_the_list_and_keeps_the_bar() {
        let rows = vec![r#"{"input_tokens":5,"output_tokens":7,"model":"opus","tool_name":"Bash"}"#];
        let p = profile_with(rows, "service=claude-code&f=model%3Dopus&ff=TOKEN&fp=3");
        let listed: Vec<&str> = p.measures.iter().chain(&p.keys).map(|f| f.label.as_str()).collect();
        assert_eq!(listed, ["input_tokens", "output_tokens"], "case-insensitive substring on the name");
        assert_eq!(p.filter, "TOKEN");
        let keep: Vec<(&str, &str)> = p.keep.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(keep, [("service", "claude-code"), ("f", "model=opus")], "the form re-sends the bar, not ff/fp");
        assert!(p.open);
    }

    #[test]
    fn the_list_pages_twenty_keys_at_a_time() {
        let obj: Vec<String> = (0..45).map(|i| format!(r#""k{i:02}":"v""#)).collect();
        let row = format!("{{{}}}", obj.join(","));
        let p1 = profile_with(vec![&row], "range=all");
        assert_eq!((p1.keys.len(), p1.start, p1.end), (45, 0, 20));
        assert!(p1.prev.is_none());
        let next = p1.next.clone().expect("a next page");
        assert!(next.contains("fp=2") && next.contains("range=all"), "{next}");

        let p3 = profile_with(vec![&row], "range=all&fp=3");
        assert_eq!((p3.start, p3.end), (40, 45));
        assert!(p3.next.is_none());
        assert!(p3.prev.as_deref().unwrap().contains("fp=2"));
        // page 2's previous link drops fp rather than writing fp=1
        let p2 = profile_with(vec![&row], "range=all&fp=2");
        assert!(!p2.prev.as_deref().unwrap().contains("fp="), "{:?}", p2.prev);
        // out of range clamps to the last page; value links start over at page 1
        let p9 = profile_with(vec![&row], "fp=9");
        assert_eq!(p9.start, 40);
        assert!(!p9.keys[0].top[0].href.contains("fp="), "{}", p9.keys[0].top[0].href);
    }

    #[test]
    fn numeric_strings_count_as_numbers() {
        let p = profile_with(vec![r#"{"duration_ms":"1922"}"#, r#"{"duration_ms":10}"#], "");
        let d = find(&p, "duration_ms").unwrap();
        assert_eq!(d.types, "number 2");
        let n = d.numeric.as_ref().unwrap();
        assert_eq!((n.min.as_str(), n.max.as_str(), n.sum.as_str()), ("10", "1922", "1932"));
        assert_eq!(d.summary, "sum 1932 · median 966");
    }

    #[test]
    fn measures_put_cost_then_tokens_first() {
        let p = profile_with(
            vec![
                r#"{"agent_path_count":1,"duration_ms":5,"output_tokens":3,"cost_usd":0.1}"#,
                r#"{"agent_path_count":1,"duration_ms":6}"#,
            ],
            "",
        );
        let order: Vec<&str> = p.measures.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(order, ["cost_usd", "output_tokens", "agent_path_count", "duration_ms"]);
    }

    #[test]
    fn explore_url_keeps_the_query_pages_format() {
        let bar = Bar::parse("range=24h");
        let u = explore_url(&bar, Page::Traces, "@span_attributes:ttft_ms");
        assert!(u.starts_with("/query?dataset=traces&range=24h&measures="), "{u}");
        assert!(u.contains("group=%5B%22%40span_attributes%3Attft_ms%22%5D"), "{u}");
        assert!(u.ends_with("&tab="), "same param set build_url emits: {u}");
    }
}

#[cfg(test)]
mod db_tests {
    use super::{find, profile_logs, profile_traces};
    use crate::{
        agents::REDACTED,
        config::Config,
        db::{Db, quote},
        filters::Bar,
        logs::db_tests::temp_db,
    };
    use std::fs;

    fn cfg(show_content: bool) -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 0,
            data_dir: std::path::PathBuf::from("/tmp/lotel-fields-test"),
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

    /// Load committed OTLP JSON fixtures from `testdata/agent/` the way a
    /// store holds them: the bundled otlp extension's `read_otlp_*` readers,
    /// INSERT…SELECT into the canonical v0.5 capped tables.
    fn fixture_db(tag: &str, fixtures: &[&str]) -> Db {
        let db = temp_db(tag);
        db.with_conn(|conn| {
            let ext = db.ext_dir.join("otlp.duckdb_extension");
            if !ext.exists() {
                fs::write(&ext, crate::ingest::OTLP_EXTENSION)?;
            }
            conn.execute_batch(&format!("LOAD {};", quote(&ext.to_string_lossy())))?;
            let ddl = crate::ingest::TABLES
                .replace("{ttl_logs}", "30 days")
                .replace("{ttl_traces}", "30 days")
                .replace("{ttl_metrics}", "30 days")
                .replace("{max_bytes}", "64MB");
            conn.execute_batch(&ddl)?;
            for f in fixtures {
                let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../testdata/agent/").to_string() + f;
                let (table, read) = if f.ends_with("logs.json") {
                    ("otlp_logs", "read_otlp_logs")
                } else {
                    ("otlp_traces", "read_otlp_traces")
                };
                conn.execute_batch(&format!(
                    "INSERT INTO otel.main.{table} SELECT * FROM {read}({})",
                    quote(&path)
                ))?;
            }
            Ok(())
        })
        .expect("load agent fixtures");
        db
    }

    #[test]
    fn fixture_keys_coverage_and_types_are_found() {
        let db = fixture_db("fields-fx-logs", &["claude-opencode-logs.json"]);
        let p = profile_logs(&db, &Bar::parse(""), &cfg(false));
        assert_eq!(p.rows, 201, "every fixture row profiles");

        let s = find(&p, "session.id").expect("Claude Code's session key");
        assert_eq!((s.rows, s.pct), (181, 90), "carried by 181 of 201 rows");
        assert_eq!(s.types, "string 181");
        assert_eq!(s.distinct, "3");

        let sn = find(&p, "resource.service.name").expect("resource bag under its prefix");
        assert_eq!((sn.rows, sn.pct), (201, 100));

        // ints and numeric strings both read as numbers
        let d = find(&p, "duration_ms").expect("mixed-type key");
        assert!(d.types.starts_with("number") && !d.types.contains("string"), "{}", d.types);
        assert!(d.numeric.is_some(), "the numbers get stats");

        let h = find(&p, "has_hooks").expect("a bool key");
        assert_eq!(h.types, "bool 44");

        // Measures: cost leads, then tokens, above plain keys.
        let measures: Vec<&str> = p.measures.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(measures[0], "cost_usd", "{measures:?}");
        for want in ["input_tokens", "output_tokens"] {
            assert!(measures.contains(&want), "measure {want} missing from {measures:?}");
        }
        assert!(measures.len() <= 8, "the band is capped: {measures:?}");
        assert!(find(&p, "ttft_ms").is_some(), "overflow measures stay in the list");
        assert!(!measures.contains(&"session.id"));

        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn numeric_keys_get_min_median_max_sum() {
        let db = fixture_db("fields-fx-num", &["claude-opencode-logs.json"]);
        let p = profile_logs(&db, &Bar::parse("f=event.name=api_request"), &cfg(false));
        assert_eq!(p.rows, 8, "the api_request events");
        let c = find(&p, "cost_usd").expect("the event cost attr");
        assert_eq!((c.rows, c.pct), (8, 100));
        assert_eq!(c.types, "number 8");
        let n = c.numeric.as_ref().expect("min/median/max/sum on numbers");
        let parse = |s: &str| s.parse::<f64>().expect("a number");
        assert!(parse(&n.min) > 0.0 && parse(&n.min) <= parse(&n.med), "min ≤ med: {n:?}");
        assert!(parse(&n.med) <= parse(&n.max) && parse(&n.max) <= parse(&n.sum), "med ≤ max ≤ sum: {n:?}");
        let t = find(&p, "input_tokens").expect("token counts");
        assert!(t.numeric.as_ref().unwrap().sum.parse::<f64>().unwrap() > 0.0);
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn content_key_values_are_masked_counts_survive() {
        let db = fixture_db("fields-fx-content", &["claude-opencode-logs.json"]);
        // one synthetic row with an un-redacted prompt (invented here; the
        // fixture's own prompt values are already placeholders)
        db.with_conn(|conn| {
            Ok(conn.execute_batch(
                r#"INSERT INTO otel.main.otlp_logs (time_unix_nano, service_name, log_attributes)
                   VALUES ('2026-11-18 10:00:00', 'claude-code',
                           '{"session.id":"ses_fields","event.name":"user_prompt","prompt":"canary-synthetic-prompt-7c1","prompt_length":27}')"#,
            )?)
        })
        .expect("seed prompt row");
        let p = profile_logs(&db, &Bar::parse("f=session.id=ses_fields"), &cfg(false));
        let pr = find(&p, "prompt").expect("the content key is listed");
        assert_eq!(pr.rows, 1);
        assert_eq!(pr.top[0].v, REDACTED);
        let all = format!("{p:?}");
        assert!(!all.contains("canary-synthetic"), "the prompt never reaches the panel: {all}");
        let pl = find(&p, "prompt_length").expect("its numeric companion does");
        let n = pl.numeric.as_ref().expect("counted as a number");
        assert_eq!((n.min.as_str(), n.max.as_str()), ("27", "27"));
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn filters_narrow_the_profile() {
        let db = fixture_db("fields-fx-filter", &["claude-opencode-logs.json"]);
        let all = profile_logs(&db, &Bar::parse(""), &cfg(false));
        assert_eq!(all.rows, 201);

        // the page's service parameter and the bar's f filters both bite
        let cc = profile_logs(&db, &Bar::parse("service=claude-code"), &cfg(false));
        assert_eq!(cc.rows, 181);
        assert_eq!(find(&cc, "session.id").unwrap().pct, 100, "only its rows remain");
        assert!(find(&cc, "role").is_none(), "the opencode-only key left the profile");

        let api = profile_logs(&db, &Bar::parse("f=event.name=api_request"), &cfg(false));
        assert_eq!(api.rows, 8);
        assert_eq!(find(&api, "cost_usd").unwrap().rows, 8);
        assert!(find(&api, "tool_name").is_none(), "tool_result's keys left with its rows");
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn panel_params_never_reach_the_page_filters() {
        let db = fixture_db("fields-fx-params", &["claude-opencode-logs.json"]);
        let plain = Bar::parse("service=claude-code");
        let with = Bar::parse("service=claude-code&ff=token&fp=2&fe=1");
        let rows = |b: &Bar| crate::logs::fetch(&db, b).expect("fetch").len();
        assert_eq!(rows(&plain), rows(&with), "the table ignores ff/fp/fe");
        assert!(with.attrs().is_empty(), "they are not attribute filters");
        assert_eq!(profile_logs(&db, &with, &cfg(false)).rows, profile_logs(&db, &plain, &cfg(false)).rows);
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn traces_profile_bags_resources_and_explore_links() {
        let db = fixture_db(
            "fields-fx-traces",
            &["claude-opencode-traces.json", "codex-traces.json"],
        );
        let p = profile_traces(&db, &Bar::parse(""), &cfg(false));
        assert_eq!(p.rows, 926, "both span fixtures");

        let run = find(&p, "resource.opencode.run").expect("opencode's only session key");
        assert_eq!(run.rows, 72);
        assert!(run.top[0].href.contains("f=resource.opencode.run%3D"), "{}", run.top[0].href);

        let d = find(&p, "duration_ms").expect("span durations");
        assert!(p.measures.iter().any(|f| f.label == "duration_ms"));
        assert!(d.explore.as_deref().unwrap().contains("%40span_attributes%3Aduration_ms"));

        let tid = find(&p, "thread.id").expect("Codex's span session key");
        assert!(tid.types.contains("number") && tid.types.contains("string"), "{}", tid.types);

        // the bar narrows spans too: one service, one attribute
        let cc = profile_traces(&db, &Bar::parse("f=session.id=none-of-these"), &cfg(false));
        assert_eq!(cc.rows, 0);
        assert!(cc.measures.is_empty() && cc.keys.is_empty());
        let _ = fs::remove_dir_all(&db.data_dir);
    }
}
