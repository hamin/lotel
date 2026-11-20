//! The coding-agent views' query layer: Claude Code, OpenAI Codex and opencode
//! telemetry read out of the six signal tables (`docs/M-agents-plan.md` §2 is
//! what the store really holds, §3 what each view reads).
//!
//! SQL builders plus the shared render redactor (§5), and the first agent
//! page: T3's `/sessions` list (§3.1). The timeline, spend, tools and setup
//! pages (T4–T7) join it here.
//!
//! Three rules the whole module lives by:
//!
//! * **The shared filter model filters.** Every builder takes a [`Bar`] and
//!   appends its `attr_conds_aliased` conditions, so an agent page filters
//!   exactly like the logs and traces pages. Keys and values bind as `?`
//!   parameters and never reach the SQL text (the J0 rule): builders write
//!   through [`Sql`]/[`Where`], which append a `?` and its value together, and
//!   the tests assert the two counts agree and that no typed text appears in
//!   the statement.
//! * **Measured data beats documented data** (§2). The event name rides in
//!   `log_attributes."event.name"` for *both* Claude Code (the OTLP
//!   `event_name` column is empty on every row) and Codex (there it holds a
//!   Rust source location), so [`event_name_expr`] reads the attribute first.
//!   `success` is a string on events and a boolean on spans, so
//!   [`success_expr`] compares text case-insensitively.
//! * **Content is never displayed** (§5). [`redact_cell`] is the single
//!   redactor every renderer that can surface stored text runs, and
//!   [`CONTENT_KEYS`] its single source of truth.

use serde_json::{Map, Value};

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, request::uri},
    view::{BoxView, View, ViewExt, component, view},
};

use crate::{
    config::Config,
    db::Db,
    filters::{
        AttrCols, AttrOp, AttrSource, Bar, attr_field, canonical_of, empty_range_hint, range_field,
        service_field,
    },
    ui::{cell, content_marker, short_ttl, thousands},
};

/// What a masked value becomes: the same placeholder
/// `scripts/agent-fixture.sh` writes when it dumps fixtures, so a redacted
/// fixture and a redacted render read identically.
pub const REDACTED: &str = "[REDACTED]";

/// The attribute keys whose values are prompt text, tool input/output, or
/// anything else an agent may put its contents into — the shared redactor's
/// single source of truth (§5). Everything under one of these keys becomes
/// [`REDACTED`]; the numeric length/size companions (`prompt_length`,
/// `tool_input_size_bytes`, …) are deliberately absent, because a count leaks
/// nothing and it is the count the views chart.
///
/// The list is the gated attributes the official docs document (§1): Claude
/// Code's `prompt`/`prompt_text` (`OTEL_LOG_USER_PROMPTS`), `response`
/// (`OTEL_LOG_ASSISTANT_RESPONSES`), `tool_input`/`tool_parameters`
/// (`OTEL_LOG_TOOL_DETAILS`), the `tool.output` span event's
/// `content`/`output`/`diff` (`OTEL_LOG_TOOL_CONTENT`), and the
/// detailed-beta-only content attrs (`new_context`, `system_reminders`,
/// `system_prompt_preview`, `user_system_prompt`, `response.model_output`) —
/// plus the spellings §5 adds for the other agents and for commands.
pub const CONTENT_KEYS: &[&str] = &[
    // prompts and responses
    "prompt",
    "prompt_text",
    "user_prompt",
    "response",
    "response.model_output",
    // tool input and output
    "tool_input",
    "tool_parameters",
    "tool_output",
    "content",
    "output",
    "diff",
    "new_context",
    // the system prompt and everything injected into it
    "system_prompt_preview",
    "user_system_prompt",
    "system_reminders",
    // commands, in every spelling the agents use
    "full_command",
    "bash_command",
    "command",
];

/// The keys a session id can ride in, in the order the views trust them:
/// Claude Code's own `session.id` first (docs §1, present on every captured
/// row), then Codex's `conversation.id` (logs and its session spans) and
/// `thread_id`/`turn.id` on spans (§2, the owner's capture), then opencode's
/// `opencode.run`.
///
/// Not `thread.id`: on Codex spans it is the OS thread number ("9", "11"),
/// carried by ~600 of the fixture's 670 spans; as a session key it invented
/// a "session" per worker thread.
///
/// `opencode.run` is a *resource* attribute and the only per-run key opencode
/// has (open question 2 ✅), so it comes last: a row carrying a real session
/// id groups by that, and one carrying only a run id groups by the run.
pub const SESSION_KEYS: &[&str] = &[
    "session.id",
    "gen_ai.conversation.id",
    "conversation.id",
    "thread_id",
    "turn.id",
    "turn_id",
    "opencode.run",
];

/// The metric label the cost and token rollups join on. Only Claude Code
/// attaches `session.id` to its metric points (§2), so the metric side of the
/// session and spend rollups groups by this one key.
pub const SESSION_ID_PATH: &str = r#"$."session.id""#;

/// The event-name key and its JSON path (§2 diff 1).
pub const EVENT_NAME_KEY: &str = "event.name";
pub const EVENT_NAME_PATH: &str = r#"$."event.name""#;

/// The tool-name spellings: Claude Code's `tool_name` on its tool spans and
/// `tool_result`/`tool_decision` events, the `_safe` form the detailed beta
/// sends alongside it, and Codex's `tool` label (INFERRED — the auth-401
/// capture emitted no tool calls; open question 4).
pub const TOOL_NAME_KEYS: &[&str] = &["tool_name", "gen_ai.tool.name", "tool_name_safe", "tool"];

/// The success key: the strings `"true"`/`"false"` on events, a boolean on
/// spans (§2 diff 2).
pub const SUCCESS_KEY: &str = "success";

/// The metric names `/spend` reads. The Claude Code names are measured (§2);
/// `codex.turn.token_usage` is INFERRED — the auth-401 capture emitted no
/// metrics at all, so it must be pinned against the logged-in Codex run before
/// T5's Codex column is trusted (open questions 3 and 7).
/// Sum metric points that may be added up: deltas (temporality 1, what
/// Claude Code sends by default) and points with no temporality recorded.
/// A cumulative point (2) is a running total — summing those would count the
/// same spend again on every export.
pub const SUMMABLE: &str = "COALESCE(aggregation_temporality, 0) <> 2";

pub const SPEND_METRIC_NAMES: &[&str] = &[
    "claude_code.cost.usage",
    "claude_code.token.usage",
    "claude_code.active_time.total",
    "codex.turn.token_usage", // INFERRED
];

/// The event that backs the spend view's fallback branch (§3.3). The store
/// keeps the un-prefixed name — the 12 event names measured in §2 carry no
/// `claude_code.` prefix.
pub const API_REQUEST_EVENT: &str = "api_request";

/// The spans `/tools` groups, and the Codex metric that stands in when no tool
/// spans arrived. Both Codex names are INFERRED (open questions 4 and 7).
/// The span that names a tool call (Claude Code) — or names it and carries
/// its outcome (Codex, INFERRED). See [`tool_health_query`].
pub const TOOL_SPAN_NAMES: &[&str] = &["claude_code.tool", "codex.tool.call"];
/// Claude Code's child of `claude_code.tool`: the outcome and execution time.
pub const TOOL_EXECUTION_SPAN: &str = "claude_code.tool.execution";
/// The key both tool spans carry, pairing a call with its execution.
pub const TOOL_USE_ID_KEY: &str = "tool_use_id";
pub const TOOL_METRIC_NAME: &str = "codex.tool.call";
pub const TOOL_DECISION_EVENT: &str = "tool_decision";
pub const TOOL_REJECT_DECISION: &str = "reject";

/// Logs' attribute columns for the agent views. An attribute filter never
/// reads a body here: the agent views address attributes, and the one body
/// that carries content is masked at render by [`redact_cell`].
pub const LOG_COLS: AttrCols = AttrCols {
    row: "log_attributes",
    resource: "resource_attributes",
    body: None,
};

/// Traces' attribute columns (the same bags the traces page reads).
pub const TRACE_COLS: AttrCols = AttrCols {
    row: "span_attributes",
    resource: "resource_attributes",
    body: None,
};

/// Metrics' attribute columns: the point's own labels plus the resource bag.
pub const METRIC_COLS: AttrCols = AttrCols {
    row: "metric_attributes",
    resource: "resource_attributes",
    body: None,
};

/// The JSON member path for a key, as a bound parameter (never SQL text) —
/// the same escaping `filters.rs` uses, so a dotted key addresses one member.
pub fn attr_path(key: &str) -> String {
    format!("$.\"{}\"", key.replace('"', "\\\""))
}

fn paths(keys: &[&str]) -> Vec<String> {
    keys.iter().map(|k| attr_path(k)).collect()
}

/// Qualify a column with the branch's table alias, as `filters.rs` does.
fn qualify(col: &str, alias: Option<&str>) -> String {
    match alias {
        Some(a) => format!("{a}.{col}"),
        None => col.to_string(),
    }
}

/// One `json_valid`-guarded extraction, its path left as a `?`: the store
/// keeps the bags as VARCHAR, so text that is not JSON must never reach
/// `json_extract_string`.
fn extract(col: &str) -> String {
    format!("(CASE WHEN json_valid({col}) THEN json_extract_string({col}, ?) END)")
}

/// The session id of a row, from `col`: `COALESCE` over [`SESSION_KEYS`], each
/// read with one `?` for its path. Pair with [`session_params`], which returns
/// those paths in `?` order.
///
/// One column at a time: the agents keep the id in different bags (Claude Code
/// and Codex in the row's own attributes, opencode only in the resource bag),
/// so a view that must cover all three reads both bags — see
/// [`session_expr_bags`] and [`session_attr_cols`].
///
/// The bag is parsed once: `json_extract_string` over the list of paths, then
/// the first non-NULL. Seven separate `json_valid`/extract pairs per bag
/// parsed the same text fourteen times a row, and at a few hundred thousand
/// spans that ran the sessions rollup out of a 1 GB memory limit on a
/// ten-core machine (agent-stress at N=500).
pub fn session_expr(col: &str) -> String {
    let marks = vec!["?"; SESSION_KEYS.len()].join(", ");
    format!(
        "(CASE WHEN json_valid({col}) THEN \
         list_filter(json_extract_string({col}, [{marks}]), lambda x: x IS NOT NULL)[1] END)"
    )
}

/// The `?` parameters [`session_expr`] leaves in its text.
pub fn session_params() -> Vec<String> {
    paths(SESSION_KEYS)
}

/// The session id of a row read from either attribute bag of `cols`
/// (qualified by `alias`): `COALESCE(<row bag keys>, <resource bag keys>)`, so
/// one expression serves Claude Code, Codex and opencode alike.
pub fn session_expr_bags(cols: &AttrCols, alias: Option<&str>) -> (String, Vec<String>) {
    let sql = format!(
        "COALESCE({}, {})",
        session_expr(&qualify(cols.row, alias)),
        session_expr(&qualify(cols.resource, alias))
    );
    let mut params = session_params();
    params.extend(session_params());
    (sql, params)
}

/// The session-id predicate for a page whose rows may carry the id under any
/// agent's key: one condition per bag — "any [`SESSION_KEYS`] spelling in
/// this bag equals the id", each bag parsed once — for `cols`' row and
/// resource bags, to be joined with `OR` by the caller. That is what makes one `f=session.id=<sid>` link match Claude Code
/// (row bag), Codex (row bag) and opencode (resource bag) rather than only the
/// agent whose spelling the page happened to guess.
///
/// The body bag is not searched: no agent keeps a session id in a log body.
/// Bind [`session_attr_params`], which returns the id twice, every key path,
/// then the id again for each condition, matching the `?` order of the text.
///
/// The `contains` guard is a cheap pre-filter in front of the JSON parse: an
/// id made only of `[A-Za-z0-9._:-]` is written verbatim by any JSON encoder,
/// so a bag whose text lacks it cannot hold it under any key. Any other id
/// (one a JSON encoder might escape) skips the guard and is matched by the
/// parse alone, so the pre-filter never changes what matches.
pub fn session_attr_cols(cols: &AttrCols, alias: Option<&str>) -> Vec<String> {
    let marks = vec!["?"; SESSION_KEYS.len()].join(", ");
    [cols.row, cols.resource]
        .iter()
        .map(|col| {
            let col = qualify(col, alias);
            format!(
                "((NOT regexp_full_match(?, '[A-Za-z0-9._:-]+') OR contains({col}, ?)) \
                 AND (CASE WHEN json_valid({col}) THEN \
                 list_contains(json_extract_string({col}, [{marks}]), ?) END))"
            )
        })
        .collect()
}

/// The `?` parameters for [`session_attr_cols`]: per condition, the key paths
/// followed by the session id.
pub fn session_attr_params(cols: &AttrCols, sid: &str) -> Vec<String> {
    let mut out = Vec::with_capacity((SESSION_KEYS.len() + 3) * 2);
    for _ in [cols.row, cols.resource] {
        out.extend([sid.to_string(), sid.to_string()]);
        out.extend(paths(SESSION_KEYS));
        out.push(sid.to_string());
    }
    out
}

/// The event name of a log row, **attribute-first** (§2 diff 1):
/// `COALESCE(<event.name attr>, NULLIF(event_name, ''))`. Claude Code leaves
/// the OTLP `event_name` column empty on every row and Codex puts a Rust
/// source location in it, so trusting the column first would name nothing.
///
/// Leaves one `?` for the path; bind [`EVENT_NAME_PATH`].
pub fn event_name_expr(alias: Option<&str>) -> String {
    format!(
        "COALESCE({}, NULLIF({}, ''))",
        extract(&qualify("log_attributes", alias)),
        qualify("event_name", alias)
    )
}

/// The `?` parameters [`event_name_expr`] leaves in its text.
pub fn event_name_params() -> Vec<String> {
    vec![EVENT_NAME_PATH.to_string()]
}

/// The tool a span or event is about: `COALESCE` over [`TOOL_NAME_KEYS`], one
/// `?` per path. Pair with [`tool_name_params`].
pub fn tool_name_expr(col: &str) -> String {
    let parts: Vec<String> = TOOL_NAME_KEYS.iter().map(|_| extract(col)).collect();
    format!("COALESCE({})", parts.join(", "))
}

/// The `?` parameters [`tool_name_expr`] leaves in its text.
pub fn tool_name_params() -> Vec<String> {
    paths(TOOL_NAME_KEYS)
}

/// The `success` of a span or event as comparable text: `lower()` of the
/// extracted value, defaulting to `'true'` when the row carries none (the
/// Claude Code tool span documents no `success` attribute at all, §1).
///
/// Events send `"true"`/`"false"` as strings and spans send booleans (§2 diff
/// 2), which is why callers compare this text rather than a JSON type. Leaves
/// one `?` for the path; bind [`success_params`].
pub fn success_expr(col: &str) -> String {
    format!("lower(COALESCE({}, 'true'))", extract(col))
}

/// The `?` parameters [`success_expr`] leaves in its text.
pub fn success_params() -> Vec<String> {
    vec![attr_path(SUCCESS_KEY)]
}

/// A statement under construction. Text and parameters are appended together,
/// so a `?` cannot be written without its value being bound: the J0 rule made
/// structural. `finish`'s check can only fail if someone writes text around
/// the two, which is what its tests cover.
#[derive(Default)]
struct Sql {
    text: String,
    params: Vec<String>,
}

impl Sql {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, s: &str) -> &mut Self {
        self.text.push_str(s);
        self
    }

    /// Bind values to the `?` already written in the text, in order. For a
    /// projection whose paths and literals were spelled out up front; `finish`
    /// refuses a statement with any `?` left over.
    fn fill<I: IntoIterator<Item = String>>(&mut self, values: I) -> &mut Self {
        let need = self.text.matches('?').count() - self.params.len();
        let mut values = values.into_iter();
        for _ in 0..need {
            self.params.push(
                values
                    .next()
                    .unwrap_or_else(|| panic!("fewer values than `?` in {}", self.text)),
            );
        }
        assert!(
            values.next().is_none(),
            "more values than `?` in {}",
            self.text
        );
        self
    }

    /// Append a helper expression and the parameters it left behind, keeping
    /// them in the order their `?` appear in the text.
    fn expr(&mut self, sql: &str, params: &[String]) -> &mut Self {
        assert_eq!(
            sql.matches('?').count(),
            params.len(),
            "expression and parameters disagree:\n{sql}"
        );
        self.text.push_str(sql);
        self.params.extend_from_slice(params);
        self
    }

    /// The conditions, appended in text order with their parameters.
    fn where_all(&mut self, w: &Where) -> &mut Self {
        if w.is_empty() {
            return self;
        }
        self.push(" WHERE ");
        for (i, cond) in w.conds.iter().enumerate() {
            if i > 0 {
                self.push(" AND ");
            }
            let params = w.params[i].as_slice();
            assert_eq!(
                cond.matches('?').count(),
                params.len(),
                "condition and parameters disagree: {cond}"
            );
            self.push(cond);
            self.params.extend_from_slice(params);
        }
        self
    }

    fn finish(self) -> (String, Vec<String>) {
        debug_assert_eq!(
            self.text.matches('?').count(),
            self.params.len(),
            "unbound `?` in {}",
            self.text
        );
        (self.text, self.params)
    }
}

/// Conditions of one `WHERE`, each paired with its own parameters so the two
/// can never drift apart from the `?` order.
#[derive(Default)]
struct Where {
    conds: Vec<String>,
    params: Vec<Vec<String>>,
}

impl Where {
    fn new() -> Self {
        Self::default()
    }

    fn is_empty(&self) -> bool {
        self.conds.is_empty()
    }

    /// `cond` with one bound value, e.g. `("service_name = ?", "claude-code")`.
    fn one(&mut self, col: &str, value: &str) -> &mut Self {
        self.conds.push(format!("({col} = ?)"));
        self.params.push(vec![value.to_string()]);
        self
    }

    /// A fragment with as many `?` as it is given parameters.
    fn add(&mut self, cond: &str, params: &[String]) -> &mut Self {
        assert_eq!(
            cond.matches('?').count(),
            params.len(),
            "condition and parameters disagree: {cond}"
        );
        self.conds.push(cond.to_string());
        self.params.push(params.to_vec());
        self
    }

    /// A helper expression compared to one bound value.
    fn expr_eq(&mut self, expr: &str, expr_params: &[String], value: &str) -> &mut Self {
        let mut params = expr_params.to_vec();
        params.push(value.to_string());
        self.add(&format!("({expr} = ?)"), &params)
    }

    /// `("name" IN (?, ?, …))` over the metric and span names.
    fn in_list(&mut self, column: &str, names: &[&str]) -> &mut Self {
        let list = vec!["?"; names.len()].join(", ");
        let params = names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        self.add(&format!("({column} IN ({list}))"), &params)
    }

    /// The bar's attribute filters, exactly as `filters.rs` builds them.
    fn bar(&mut self, bar: &Bar, cols: &AttrCols, alias: Option<&str>) -> &mut Self {
        let (conds, params) = bar.attr_conds_aliased(cols, alias);
        // one condition per filter; `filters.rs` emits one `?` set each
        let mut at = 0usize;
        for cond in conds {
            let n = cond.matches('?').count();
            self.add(&cond, &params[at..at + n]);
            at += n;
        }
        debug_assert_eq!(at, params.len(), "bar params went unused");
        self
    }

    /// The `service=…` first-class param (§3): empty means "every agent
    /// service" and filters nothing — the seed's `cart`/`checkout` rows carry
    /// no session keys, so they drop out of the agent views on their own.
    /// The bar's time range on `time_col` — nothing when it is `all` (the
    /// default), so an unset range never narrows an agent view.
    fn since(&mut self, bar: &Bar, time_col: &str, alias: Option<&str>) -> &mut Self {
        if bar.range_is_narrow() {
            self.add(
                &format!("{} >= CAST(? AS TIMESTAMP)", qualify(time_col, alias)),
                &[bar.cutoff()],
            );
        }
        self
    }

    fn service(&mut self, service: &str, alias: Option<&str>) -> &mut Self {
        if !service.is_empty() {
            self.one(&qualify("service_name", alias), service);
        }
        self
    }
}

/// One signal's arm of the `/sessions` union: which table, under what alias,
/// which time column, and the signal tag the outer query counts by.
struct Branch<'a> {
    alias: &'a str,
    table: &'a str,
    cols: &'a AttrCols,
    time_col: &'a str,
    sig: &'a str,
}

/// `/sessions` (§3.1): one row per agent session — id, service, event count,
/// span count, first and last seen, and the cost/token rollups.
///
/// Each arm of the `ev` union carries its own service and [`Bar`] conditions
/// (the bar filters a session's rows in whichever signal they live in; the
/// session shows up when any arm matches) and reads its session id from both
/// of its attribute bags. The cost/token rollups stay unfiltered so a
/// session's total is its total whatever the bar is doing to the row list.
/// The row cap comes from [`Bar::limit`] and is bound, not interpolated.
pub fn sessions_query(bar: &Bar, service: &str) -> (String, Vec<String>) {
    let branches = [
        Branch {
            alias: "lg",
            table: "otel.main.otlp_logs",
            cols: &LOG_COLS,
            time_col: "time_unix_nano",
            sig: "logs",
        },
        Branch {
            alias: "tr",
            table: "otel.main.otlp_traces",
            cols: &TRACE_COLS,
            time_col: "start_time_unix_nano",
            sig: "spans",
        },
        Branch {
            alias: "mt",
            table: "otel.main.otlp_metrics_sum",
            cols: &METRIC_COLS,
            time_col: "time_unix_nano",
            sig: "metrics",
        },
    ];

    let mut sql = Sql::new();
    sql.push("WITH ev AS (");
    for (i, b) in branches.iter().enumerate() {
        if i > 0 {
            sql.push(" UNION ALL ");
        }
        let (sid, sid_params) = session_expr_bags(b.cols, Some(b.alias));
        sql.push("SELECT ").expr(&sid, &sid_params).push(&format!(
            " AS sid, '{}' AS sig, {}.service_name AS service_name, {}.{} AS t FROM {} {}",
            b.sig, b.alias, b.alias, b.time_col, b.table, b.alias
        ));
        let mut w = Where::new();
        w.service(service, Some(b.alias))
            .since(bar, b.time_col, Some(b.alias))
            .bar(bar, b.cols, Some(b.alias));
        sql.where_all(&w);
    }
    sql.push(&format!(
        "), cost AS (SELECT json_extract_string(metric_attributes, ?) AS sid, \
         sum(double_value) AS usd FROM otel.main.otlp_metrics_sum WHERE (\"name\" = ?) AND {SUMMABLE} GROUP BY 1)\
         , tok AS (SELECT json_extract_string(metric_attributes, ?) AS sid, \
         sum(double_value) FILTER (WHERE (json_extract_string(metric_attributes, ?) = ?)) AS tin, \
         sum(double_value) FILTER (WHERE (json_extract_string(metric_attributes, ?) = ?)) AS tout \
         FROM otel.main.otlp_metrics_sum WHERE (\"name\" = ?) AND {SUMMABLE} GROUP BY 1) \
         SELECT ev.sid AS sid, any_value(ev.service_name) AS svc, count(*) AS events, \
         count(*) FILTER (WHERE ev.sig = 'spans') AS spans, min(ev.t) AS first_seen, \
         max(ev.t) AS last_seen, any_value(c.usd) AS usd, any_value(t.tin) AS tok_in, \
         any_value(t.tout) AS tok_out FROM ev LEFT JOIN cost c USING (sid) \
         LEFT JOIN tok t USING (sid) WHERE (ev.sid IS NOT NULL) \
         GROUP BY ev.sid ORDER BY last_seen DESC LIMIT CAST(? AS BIGINT)",
    ));
    sql.fill([
        SESSION_ID_PATH.to_string(),
        SPEND_METRIC_NAMES[0].to_string(),
        SESSION_ID_PATH.to_string(),
        attr_path("type"),
        "input".to_string(),
        attr_path("type"),
        "output".to_string(),
        SPEND_METRIC_NAMES[1].to_string(),
        bar.limit().to_string(),
    ]);
    sql.finish()
}

/// `/sessions?session=<id>`'s event band (§3.2): the session's log rows as
/// event name + time + its own attribute bag, oldest first. The page renders
/// the bag minus [`CONTENT_KEYS`], every cell through [`redact_cell`].
pub fn session_events_query(bar: &Bar, sid: &str) -> (String, Vec<String>) {
    let mut sql = Sql::new();
    sql.push("SELECT ")
        .expr(&event_name_expr(Some("lg")), &event_name_params())
        .push(
            " AS ev, lg.time_unix_nano AS t, COALESCE(lg.log_attributes, '{}') AS attrs, \
              COALESCE(lg.body, '') AS body \
              FROM otel.main.otlp_logs lg",
        );
    let mut w = Where::new();
    w.add(
        &format!(
            "({})",
            session_attr_cols(&LOG_COLS, Some("lg")).join(" OR ")
        ),
        &session_attr_params(&LOG_COLS, sid),
    )
    .bar(bar, &LOG_COLS, Some("lg"));
    sql.where_all(&w).push(" ORDER BY lg.time_unix_nano");
    sql.push(&format!(" LIMIT {EVENTS_SHOWN}"));
    sql.finish()
}

/// The event band's row cap: a long session still renders, and the band says
/// when it is showing the first N.
pub(crate) const EVENTS_SHOWN: usize = 1000;

/// `/spend` (§3.3): day × model × developer from the metric points — cost,
/// input/output/cache tokens, active seconds.
pub fn spend_metrics_query(bar: &Bar, service: &str) -> (String, Vec<String>) {
    let mut sql = Sql::new();
    sql.push(
        "SELECT date_trunc('day', time_unix_nano) AS d, \
         json_extract_string(metric_attributes, ?) AS model, \
         json_extract_string(metric_attributes, ?) AS dev, \
         sum(double_value) FILTER (WHERE (\"name\" = ?)) AS usd, \
         sum(double_value) FILTER (WHERE (\"name\" = ?) AND (json_extract_string(metric_attributes, ?) = ?)) AS tok_in, \
         sum(double_value) FILTER (WHERE (\"name\" = ?) AND (json_extract_string(metric_attributes, ?) = ?)) AS tok_out, \
         sum(double_value) FILTER (WHERE (\"name\" = ?) AND (json_extract_string(metric_attributes, ?) LIKE ?)) AS tok_cache, \
         sum(double_value) FILTER (WHERE (\"name\" = ?)) AS active_s FROM otel.main.otlp_metrics_sum",
    );
    sql.fill([
        attr_path("model"),
        attr_path("user.id"),
        SPEND_METRIC_NAMES[0].to_string(),
        SPEND_METRIC_NAMES[1].to_string(),
        attr_path("type"),
        "input".to_string(),
        SPEND_METRIC_NAMES[1].to_string(),
        attr_path("type"),
        "output".to_string(),
        SPEND_METRIC_NAMES[1].to_string(),
        attr_path("type"),
        "cache%".to_string(),
        SPEND_METRIC_NAMES[2].to_string(),
    ]);
    let mut w = Where::new();
    w.service(service, None)
        .in_list("\"name\"", SPEND_METRIC_NAMES)
        .add(SUMMABLE, &[])
        .since(bar, "time_unix_nano", None)
        .bar(bar, &METRIC_COLS, None);
    sql.where_all(&w)
        .push(" GROUP BY 1, 2, 3 ORDER BY d DESC, usd DESC NULLS LAST");
    sql.finish()
}

/// `/spend`'s fallback branch (§3.3): the same day × model × developer totals
/// read off the `api_request` events, so a logs-only capture still shows cost.
/// The page runs it for the days the metric branch left empty and badges them
/// "from events".
pub fn spend_events_query(bar: &Bar, service: &str) -> (String, Vec<String>) {
    let mut sql = Sql::new();
    sql.push(
        "SELECT date_trunc('day', lg.time_unix_nano) AS d, \
         json_extract_string(lg.log_attributes, ?) AS model, \
         json_extract_string(lg.log_attributes, ?) AS dev, \
         sum(CAST(json_extract_string(lg.log_attributes, ?) AS DOUBLE)) AS usd, \
         sum(CAST(json_extract_string(lg.log_attributes, ?) AS DOUBLE)) AS tok_in, \
         sum(CAST(json_extract_string(lg.log_attributes, ?) AS DOUBLE)) AS tok_out \
         FROM otel.main.otlp_logs lg",
    );
    sql.fill([
        attr_path("model"),
        attr_path("user.id"),
        attr_path("cost_usd"),
        attr_path("input_tokens"),
        attr_path("output_tokens"),
    ]);
    let mut w = Where::new();
    w.service(service, Some("lg"))
        .expr_eq(
            &event_name_expr(Some("lg")),
            &event_name_params(),
            API_REQUEST_EVENT,
        )
        .since(bar, "time_unix_nano", Some("lg"))
        .bar(bar, &LOG_COLS, Some("lg"));
    sql.where_all(&w)
        .push(" GROUP BY 1, 2, 3 ORDER BY d DESC, usd DESC NULLS LAST");
    sql.finish()
}

/// `/tools` (§3.4): calls, failures and latency percentiles per tool, from
/// the tool spans.
///
/// Measured on Claude Code (T1): a tool call is two spans. `claude_code.tool`
/// carries the name (`tool_name` / `gen_ai.tool.name`) and its child
/// `claude_code.tool.execution` carries the outcome (`success`) — and the
/// execution time, without the wait for the user's approval. Both carry the
/// call's `tool_use_id`, so each named call left-joins its execution span on
/// (trace, tool_use_id): outcome and latency come from the execution when it
/// is there, from the named span otherwise. A Codex `codex.tool.call` span
/// (INFERRED, [`TOOL_SPAN_NAMES`]) carries both on itself and simply finds no
/// execution partner.
pub fn tool_health_query(bar: &Bar, service: &str) -> (String, Vec<String>) {
    let mut sql = Sql::new();
    sql.push("WITH t AS (SELECT trace_id, ")
        .expr(&tool_name_expr("span_attributes"), &tool_name_params())
        .push(" AS tool, ")
        .expr(&extract("span_attributes"), &[attr_path(TOOL_USE_ID_KEY)])
        .push(" AS use_id, duration_time_unix_nano AS dur, ")
        .expr(&success_expr("span_attributes"), &success_params())
        .push(" AS ok FROM otel.main.otlp_traces");
    let mut w = Where::new();
    w.service(service, None)
        .in_list("\"name\"", TOOL_SPAN_NAMES)
        .since(bar, "start_time_unix_nano", None)
        .bar(bar, &TRACE_COLS, None);
    sql.where_all(&w);
    sql.push("), x AS (SELECT trace_id, ")
        .expr(&extract("span_attributes"), &[attr_path(TOOL_USE_ID_KEY)])
        .push(" AS use_id, duration_time_unix_nano AS dur, ")
        .expr(&success_expr("span_attributes"), &success_params())
        .push(" AS ok FROM otel.main.otlp_traces");
    let mut wx = Where::new();
    wx.one("\"name\"", TOOL_EXECUTION_SPAN);
    sql.where_all(&wx);
    sql.push(
        ") SELECT t.tool AS tool, count(*) AS calls, \
         count(*) FILTER (WHERE COALESCE(x.ok, t.ok) <> 'true') AS fails, \
         quantile_cont(COALESCE(x.dur, t.dur) / 1e6, 0.5) AS p50_ms, \
         quantile_cont(COALESCE(x.dur, t.dur) / 1e6, 0.95) AS p95_ms, \
         max(COALESCE(x.dur, t.dur)) / 1e6 AS max_ms \
         FROM t LEFT JOIN x ON x.trace_id = t.trace_id AND x.use_id = t.use_id \
         GROUP BY 1 ORDER BY calls DESC, tool",
    );
    sql.finish()
}

/// `/tools`' fallback (§3.4): the Codex tool counters, for when no tool spans
/// arrived. INFERRED — the auth-401 capture emitted no metrics at all (open
/// question 7). The histogram side (`codex.tool.call.duration_ms` quantiles
/// from `bucket_counts` + `explicit_bounds`) is deliberately not built here:
/// nothing has emitted that metric yet, so T6 adds it once T0's logged-in Codex
/// run pins the names.
pub fn tool_metrics_query(bar: &Bar, service: &str) -> (String, Vec<String>) {
    let mut sql = Sql::new();
    sql.push("SELECT ")
        .expr(&tool_name_expr("metric_attributes"), &tool_name_params())
        .push(" AS tool, sum(double_value) AS calls, sum(double_value) FILTER (WHERE ")
        .expr(&success_expr("metric_attributes"), &success_params())
        .push(" <> 'true') AS fails FROM otel.main.otlp_metrics_sum");
    let mut w = Where::new();
    w.service(service, None)
        .one("\"name\"", TOOL_METRIC_NAME)
        .since(bar, "time_unix_nano", None)
        .bar(bar, &METRIC_COLS, None);
    sql.where_all(&w).push(" GROUP BY 1 ORDER BY calls DESC");
    sql.finish()
}

/// `/tools`' second block (§3.4): how often each tool's use was rejected,
/// counted from the `tool_decision` events.
pub fn tool_decisions_query(bar: &Bar, service: &str) -> (String, Vec<String>) {
    let mut sql = Sql::new();
    sql.push("SELECT ")
        .expr(
            &tool_name_expr(&qualify("log_attributes", Some("lg"))),
            &tool_name_params(),
        )
        .push(" AS tool, count(*) AS rejected FROM otel.main.otlp_logs lg");
    let mut w = Where::new();
    w.service(service, Some("lg"))
        .expr_eq(
            &event_name_expr(Some("lg")),
            &event_name_params(),
            TOOL_DECISION_EVENT,
        )
        .expr_eq(
            &extract(&qualify("log_attributes", Some("lg"))),
            &[attr_path("decision")],
            TOOL_REJECT_DECISION,
        )
        .since(bar, "time_unix_nano", Some("lg"))
        .bar(bar, &LOG_COLS, Some("lg"));
    sql.where_all(&w).push(" GROUP BY 1 ORDER BY 2 DESC");
    sql.finish()
}

// ————————————————————————————————— pages ———————————————————————————————————

/// One row of `/sessions`: the typed, render-ready projection of
/// [`sessions_query`]'s SELECT. The query's `first_seen` stays in SQL — the
/// list only shows "last seen" (§3.1).
#[derive(Clone, Debug, Default)]
pub(crate) struct SessionRow {
    pub(crate) sid: String,
    pub(crate) service: String,
    pub(crate) events: String,
    pub(crate) spans: String,
    pub(crate) last_seen: String,
    pub(crate) usd: String,
    pub(crate) tok_in: String,
    pub(crate) tok_out: String,
}

/// Run [`sessions_query`] and project its rows. Values arrive still raw: the
/// renderer runs every displayed cell through `Config::display` (§5).
pub(crate) fn fetch_sessions(db: &Db, bar: &Bar, service: &str) -> anyhow::Result<Vec<SessionRow>> {
    let (sql, params) = sessions_query(bar, service);
    let bind: Vec<&dyn duckdb::ToSql> = params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    let (_, rows) = db.query_rows_with(&sql, &bind)?;
    Ok(rows
        .into_iter()
        .map(|r| SessionRow {
            sid: cell(&r, 0),
            service: cell(&r, 1),
            events: cell(&r, 2),
            spans: cell(&r, 3),
            last_seen: cell(&r, 5),
            usd: cell(&r, 6),
            tok_in: cell(&r, 7),
            tok_out: cell(&r, 8),
        })
        .collect())
}

/// The bar's service options across all three signals the session union
/// reads — a session can live in any of them, so the select must offer what
/// each store holds.
fn agent_services(db: &Db) -> Vec<String> {
    db.query_rows(
        "SELECT DISTINCT service_name FROM (
             SELECT service_name FROM otel.main.otlp_logs
             UNION ALL SELECT service_name FROM otel.main.otlp_traces
             UNION ALL SELECT service_name FROM otel.main.otlp_metrics_sum)
         WHERE service_name IS NOT NULL AND service_name != '' ORDER BY 1",
    )
    .map(|(_, rows)| rows.into_iter().map(|r| cell(&r, 0)).collect())
    .unwrap_or_default()
}

/// A numeric that arrived as stored text (DuckDB renders doubles as "40.0")
/// in the shape the table wants: integers without a dot, decimals without
/// trailing zeros. Blanks (a `LEFT JOIN` miss) and non-numbers (an error
/// placeholder) pass through untouched.
fn num_cell(s: &str) -> String {
    match s.parse::<f64>() {
        Ok(v) if v == v.trunc() && v.abs() < 1e15 => format!("{}", v as i64),
        Ok(_) => s.trim_end_matches('0').trim_end_matches('.').to_string(),
        Err(_) => s.to_string(),
    }
}

/// The USD cell (§3.1: `.num`): a `$` prefix, blank when the session has no
/// cost rollup.
fn usd_cell(s: &str) -> String {
    match s.parse::<f64>() {
        // cents once there are any; sub-cent sessions keep 4 places so they
        // don't all read "$0.00"
        Ok(v) if v.abs() >= 0.01 => format!("${v:.2}"),
        Ok(0.0) => "$0".to_string(),
        Ok(v) => format!("${}", format!("{v:.4}").trim_end_matches('0').trim_end_matches('.')),
        Err(_) => s.to_string(),
    }
}

/// A token count: thousands-grouped (`"56922.0"` → `"56,922"`).
fn token_cell(s: &str) -> String {
    thousands(&num_cell(s))
}

/// The session id in its truncated mono form (§3.1): long ids show their
/// first 8 characters; the whole value stays in the link and the `title`.
fn short_id(sid: &str) -> String {
    let chars: Vec<char> = sid.chars().collect();
    if chars.len() <= 14 {
        sid.to_string()
    } else {
        chars[..8].iter().collect::<String>() + "…"
    }
}

/// Last seen as relative text (§3.1): `"42s ago"`, `"5m ago"`, `"3h ago"`,
/// `"12d ago"`. The store keeps UTC wall time, so the comparison is against
/// UTC now. Anything unparsable (an error placeholder, epoch junk) renders
/// as stored.
fn ago(ts: &str) -> String {
    let Ok(t) = chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S%.f") else {
        return ts.to_string();
    };
    let secs = (chrono::Utc::now().naive_utc() - t).num_seconds();
    if secs < 0 {
        "now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3_600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3_600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// One `/sessions` row (§3.1): the truncated mono id linking to the
/// session's timeline, a service badge, events, spans, USD, tokens in / out,
/// last seen relative time, and a logs link pre-filtered to the session
/// (`f=session.id=<sid>` via [`Bar::href_add_filter`]). Every stored cell
/// goes through `Config::display` — a session id or service name is stored
/// text like any other (§5). `cfg` rides as a prop, not from context, so the
/// render tests call this straight.
#[derive(Clone, Debug)]
pub(crate) struct SessionCells {
    pub(crate) timeline: String,
    pub(crate) logs: String,
    /// `/traces` pre-filtered to the session; empty when it has no spans.
    pub(crate) traces: String,
    pub(crate) sid: String,
    pub(crate) label: String,
    pub(crate) service: String,
    pub(crate) events: String,
    pub(crate) spans: String,
    pub(crate) usd: String,
    pub(crate) tokens: String,
    pub(crate) last: String,
}

pub(crate) fn session_cells(row: &SessionRow, bar: &Bar, cfg: &Config) -> SessionCells {
    let sid = cfg.display(&row.sid);
    SessionCells {
        // links carry the displayed id: a masked value can't leak via its href
        timeline: bar.href("/sessions", &[("session", sid.as_str())]),
        logs: bar.href_add_filter(
            "/logs",
            &canonical_of(AttrSource::Row, AttrOp::Eq, "session.id", &sid),
        ),
        traces: if matches!(row.spans.as_str(), "" | "0") {
            String::new()
        } else {
            bar.href_add_filter(
                "/traces",
                &canonical_of(AttrSource::Row, AttrOp::Eq, "session.id", &sid),
            )
        },
        label: short_id(&sid),
        sid,
        service: cfg.display(&row.service),
        events: thousands(&cfg.display(&row.events)),
        spans: thousands(&cfg.display(&row.spans)),
        usd: usd_cell(&cfg.display(&row.usd)),
        tokens: match (row.tok_in.is_empty(), row.tok_out.is_empty()) {
            (true, true) => String::new(),
            _ => format!(
                "{} / {}",
                token_cell(&cfg.display(&row.tok_in)),
                token_cell(&cfg.display(&row.tok_out))
            ),
        },
        last: ago(&cfg.display(&row.last_seen)),
    }
}

#[component]
async fn session_row(cx: &Cx, c: SessionCells) -> Result<impl View> {
    Ok(view! {
        <tr>
            <td><a class="mono" href=(c.timeline.clone()) title=(c.sid.clone())>(c.label.clone())</a></td>
            <td><span class="badge">(c.service.clone())</span></td>
            <td class="num">(c.events.clone())</td>
            <td class="num">(c.spans.clone())</td>
            <td class="num">(c.usd.clone())</td>
            <td class="num">(c.tokens.clone())</td>
            <td class="muted">(c.last.clone())</td>
            <td>
                <a href=(c.logs.clone())>"logs"</a>
                if !c.traces.is_empty() { " " <a href=(c.traces.clone())>"traces"</a> }
            </td>
        </tr>
    })
}

/// The `/sessions` empty-state row: a fresh lotel has no agent sessions yet,
/// and the way to get some is the setup page's copy-paste env vars.
#[component]
pub(crate) async fn sessions_empty_row(cx: &Cx, colspan: usize) -> Result<impl View> {
    Ok(view! {
        <tr>
            <td colspan=(colspan.to_string()) class="muted">
                "No agent sessions in this view. "
                <a href="/setup">"Set up Claude Code or Codex"</a>
                " to send telemetry here, or clear the filters above."
            </td>
        </tr>
    })
}

// ————————————————————————————————— session timeline (§3.2) —————————————————————————————————

/// Event-band chips worth a glance, in this order: what ran, what it cost,
/// how it went. Everything else is in the row's expandable bag.
const EVENT_CHIP_KEYS: &[&str] = &[
    "tool_name", "gen_ai.tool.name", "decision", "success", "error", "status_code",
    "model", "duration_ms", "cost_usd", "input_tokens", "output_tokens",
    "cache_read_tokens", "cache_creation_tokens", "prompt_length", "source",
];

/// One event-band row, every displayed cell already through
/// `Config::display` (§5).
#[derive(Clone, Debug)]
pub(crate) struct EventCells {
    pub(crate) time: String,
    pub(crate) name: String,
    pub(crate) chips: Vec<(String, String)>,
    /// the whole attribute bag, pretty and masked, for the <details>
    pub(crate) bag: String,
    /// how many consecutive same-name, chipless events this row stands for
    /// (startup bursts: a dozen `hook_registered` in a row read as one line)
    pub(crate) repeat: usize,
}

/// Fold runs of consecutive events that share a name and have no chips into
/// their first row with a count; anything with chips (a tool call, an API
/// request) always keeps its own row.
pub(crate) fn fold_repeats(events: Vec<EventCells>) -> Vec<EventCells> {
    let mut out: Vec<EventCells> = Vec::with_capacity(events.len());
    for e in events {
        match out.last_mut() {
            Some(prev) if prev.name == e.name && prev.chips.is_empty() && e.chips.is_empty() => {
                prev.repeat += 1;
            }
            _ => out.push(e),
        }
    }
    out
}

/// Shape one [`session_events_query`] row (`ev, t, attrs, body`) for the band.
/// Chips come only from [`EVENT_CHIP_KEYS`] and never from a
/// [`CONTENT_KEYS`] key, whatever the opt-in says — a chip is a glance, not a
/// content viewer. A row with no event name (opencode) is named by its body's
/// message (element 0 of the `[message, payload]` array), masked like any
/// other stored text.
pub(crate) fn event_cells(row: &[String], cfg: &Config) -> EventCells {
    let get = |i: usize| row.get(i).cloned().unwrap_or_default();
    let attrs: Map<String, Value> = match serde_json::from_str::<Value>(&get(2)) {
        Ok(Value::Object(m)) => m,
        _ => Map::new(),
    };
    let chips = EVENT_CHIP_KEYS
        .iter()
        .filter(|k| !CONTENT_KEYS.contains(k))
        .filter_map(|k| {
            let v = attrs.get(*k)?;
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Null => return None,
                other => other.to_string(),
            };
            let shown = cfg.display(&text);
            // money reads as money: cents, not float noise
            let shown = if *k == "cost_usd" { usd_cell(&shown) } else { shown };
            (!text.is_empty()).then(|| (k.to_string(), shown))
        })
        .collect();
    let name = match get(0) {
        n if !n.is_empty() => n,
        _ => {
            let body = get(3);
            let msg = match serde_json::from_str::<Value>(&body) {
                Ok(Value::Array(a)) => a.first().and_then(|v| v.as_str()).unwrap_or("").to_string(),
                _ => body.chars().take(80).collect(),
            };
            if msg.is_empty() { "event".to_string() } else { msg }
        }
    };
    let bag = if attrs.is_empty() {
        String::new()
    } else {
        cfg.display(&serde_json::to_string_pretty(&Value::Object(attrs)).unwrap_or_default())
    };
    EventCells {
        time: cfg.display(&get(1)),
        name: cfg.display(&name),
        chips,
        bag,
        repeat: 1,
    }
}

/// The session's events, oldest first, shaped for the band.
pub(crate) fn fetch_session_events(db: &Db, bar: &Bar, sid: &str, cfg: &Config) -> Vec<EventCells> {
    let (sql, params) = session_events_query(bar, sid);
    let bind: Vec<&dyn duckdb::ToSql> = params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
    db.query_rows_with(&sql, &bind)
        .map(|(_, rows)| fold_repeats(rows.iter().map(|r| event_cells(r, cfg)).collect()))
        .unwrap_or_default()
}

/// The event band: one line per log event — time, the event name, its glance
/// chips — expandable to the masked attribute bag.
#[component]
pub(crate) async fn event_band(cx: &Cx, events: Vec<EventCells>) -> Result<impl View> {
    Ok(view! {
        <table class="events">
            <thead><tr><th>"time"</th><th>"event"</th></tr></thead>
            <tbody>
                for e in events {
                    <tr>
                        <td class="muted mono">(e.time.clone())</td>
                        <td class="logbody">
                            if e.bag.is_empty() {
                                <span class="badge">(e.name.clone())</span>
                                if e.repeat > 1 { <span class="muted">(format!(" ×{}", e.repeat))</span> }
                            } else {
                                <details class="logx">
                                    <summary class="logtext">
                                        <span class="badge">(e.name.clone())</span>
                                        if e.repeat > 1 { <span class="muted">(format!(" ×{}", e.repeat))</span> }
                                    </summary>
                                    <pre>(e.bag.clone())</pre>
                                </details>
                            }
                            if !e.chips.is_empty() {
                                <span class="logchips">
                                    for (k, v) in &e.chips {
                                        <span class="kchip"><b>(k.clone())</b>"="(v.clone())</span>
                                    }
                                </span>
                            }
                        </td>
                    </tr>
                }
            </tbody>
        </table>
    })
}

/// `/sessions?session=<id>` (§3.2): the session's header, its spans as one
/// waterfall (a tree per trace, one shared time window), the selected span's
/// card, then the event band.
#[component]
pub(crate) async fn session_timeline(
    cx: &Cx,
    sid: String,
    head: Option<SessionCells>,
    spans: Vec<crate::traces::WaterfallSpan>,
    total_ns: i64,
    selected: Option<crate::traces::WaterfallSpan>,
    span_logs: u64,
    events: Vec<EventCells>,
    back: String,
) -> Result<impl View> {
    let cfg = context::<Config>(cx);
    let spans: Vec<_> = spans.iter().map(|s| crate::traces::span_for_display(s, cfg)).collect();
    let selected = selected.as_ref().map(|s| crate::traces::span_for_display(s, cfg));
    let shown_sid = cfg.display(&sid);
    let span_href = format!("/sessions?session={}&span=", Bar::enc(&shown_sid));
    let capped = events.iter().map(|e| e.repeat).sum::<usize>() >= EVENTS_SHOWN;
    Ok(view! {
        <h1>"Session"</h1>
        <p class="sub meta">
            <span class="mono seg">(shown_sid.clone())</span>
            if let Some(h) = &head {
                <span class="seg">(h.service.clone())</span>
                <span class="seg">(h.events.clone())" events"</span>
                <span class="seg">(h.spans.clone())" spans"</span>
                if !h.usd.is_empty() { <span class="seg">(h.usd.clone())</span> }
                if !h.tokens.is_empty() { <span class="seg">(h.tokens.clone())" tokens in / out"</span> }
                <span class="seg">"last seen "(h.last.clone())</span>
                <span class="seg"><a href=(h.logs.clone())>"logs"</a></span>
                if !h.traces.is_empty() { <span class="seg"><a href=(h.traces.clone())>"traces"</a></span> }
            }
        </p>
        <p><a class="trace-link" href=(back)>"← all sessions"</a></p>

        if head.is_none() && spans.is_empty() && events.is_empty() {
            <p class="muted">"No data for this session in this view (it may have aged out, or the filters exclude it)."</p>
        }

        if !spans.is_empty() {
            <h2>"Spans"</h2>
            crate::traces::waterfall(spans: spans, total_ns: total_ns, href: span_href)
            if let Some(s) = selected {
                <p class="meta">
                    <a href=(format!("/traces?trace={}&span={}", Bar::enc(&s.trace_id), Bar::enc(&s.span_id)))>"open in its trace"</a>
                </p>
                crate::traces::span_card(s: s.clone(), logs: if span_logs > 0 {
                    Some((crate::correlate::logs_href(&s.trace_id, &s.span_id), crate::correlate::logs_label(span_logs)))
                } else { None })
            }
        }

        if !events.is_empty() {
            <h2>"Events"</h2>
            if capped {
                <p class="muted">(format!("Showing the first {EVENTS_SHOWN} events of this session."))</p>
            }
            event_band(events: events)
        }
    })
}

/// `/sessions` (§3.1, §4): one row per agent session — Claude Code's
/// `session.id`, Codex's `conversation.id`/`thread_id`, opencode's
/// `opencode.run` — under the shared [`Bar`], with the retention line every
/// agent page carries and the `content: shown` marker whenever the opt-in
/// has it (§5). `?session=<sid>[&span=<id>]` opens that session's
/// timeline instead ([`session_timeline`], §3.2).
#[page("/sessions")]
pub(crate) async fn sessions_page(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let cfg = context::<Config>(cx);
    let bar = Bar::parse(uri(cx).query().unwrap_or(""));
    let service = bar.get("service").to_string();
    let open = bar.get("session").to_string();

    if !open.is_empty() {
        // The header row only needs this session: narrow the rollup with the
        // cross-agent session filter rather than rolling up every session.
        let one = bar.href_add_filter("", &canonical_of(AttrSource::Row, AttrOp::Eq, "session.id", &open));
        let narrowed = Bar::parse(one.split_once('?').map_or("", |(_, q)| q));
        let rows = fetch_sessions(db, &narrowed, &service).unwrap_or_default();
        let head = rows.iter().find(|r| r.sid == open).map(|r| session_cells(r, &bar, cfg));
        let (spans, total_ns) = crate::traces::fetch_session_waterfall(db, &open).unwrap_or_default();
        let span = bar.get("span");
        let selected = spans.iter().find(|s| s.span_id == span).cloned();
        let span_logs = selected
            .as_ref()
            .map(|s| crate::correlate::log_counts(db, &s.trace_id, &s.span_id).1)
            .unwrap_or(0);
        let events = fetch_session_events(db, &bar, &open, cfg);
        let back = bar.href_without("/sessions", &["session", "span"]);
        return Ok(view! {
            session_timeline(
                sid: open.clone(),
                head: head,
                spans: spans,
                total_ns: total_ns,
                selected: selected,
                span_logs: span_logs,
                events: events,
                back: back,
            )
        }
        .boxed());
    }
    // A failed rollup says so; it never reads as "no sessions".
    let (rows, load_err) = split_err(fetch_sessions(db, &bar, &service));
    // The logs/traces contract: empty under a window narrower than all
    // offers the same query at range=all; the setup pointer shows whenever
    // there is nothing to list.
    let empty = rows.is_empty() && load_err.is_none();
    let range_href =
        (empty && bar.range_is_narrow()).then(|| bar.href("/sessions", &[("range", "all")]));
    let services = agent_services(db);
    let cells: Vec<SessionCells> = rows.iter().map(|r| session_cells(r, &bar, cfg)).collect();

    let range = bar.get("range").to_string();
    let raw_f = bar.raw_f();
    let attr_errors = bar.attr_errors();
    let shown = cfg.agent_show_content;

    Ok(view! {
        <h1>"Sessions"</h1>
        <p class="sub">
            "What your coding agents did — one row per Claude Code, Codex and opencode session."
        </p>
        <p class="meta">
            (format!(
                "logs {} · traces {} · metrics {} · prompt and tool content is never shown; \
                 anything an agent sends is forgotten after {}/{}",
                short_ttl(&cfg.ttl_logs),
                short_ttl(&cfg.ttl_traces),
                short_ttl(&cfg.ttl_metrics),
                short_ttl(&cfg.ttl_logs),
                short_ttl(&cfg.ttl_traces),
            ))
            content_marker(shown: shown)
        </p>

        <form class="filters" method="get" action="/sessions">
            service_field(options: services, current: service)
            range_field(current: range)
            attr_field(raw: raw_f, errors: attr_errors)
            <button type="submit">"Apply"</button>
        </form>

        load_error(what: "sessions", err: load_err)
        <table>
            <thead>
                <tr>
                    <th>"session"</th>
                    <th>"service"</th>
                    <th>"events"</th>
                    <th>"spans"</th>
                    <th>"usd"</th>
                    <th>"tokens in / out"</th>
                    <th>"last seen"</th>
                    <th></th>
                </tr>
            </thead>
            <tbody>
                if let Some(h) = &range_href {
                    empty_range_hint(colspan: 8, noun: "sessions", href: h.clone())
                }
                if empty {
                    sessions_empty_row(colspan: 8)
                }
                for c in &cells {
                    session_row(c: c.clone())
                }
            </tbody>
        </table>
    }
    .boxed())
}

/// A page's rows, or none plus the error text when the query failed.
fn split_err<T>(r: anyhow::Result<Vec<T>>) -> (Vec<T>, Option<String>) {
    match r {
        Ok(rows) => (rows, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    }
}

/// The red box a page shows in place of its rows when the query behind them
/// failed (out of memory, a broken segment, …), so a failure never passes
/// for an empty store.
#[component]
async fn load_error(cx: &Cx, what: &'static str, err: Option<String>) -> Result<impl View> {
    Ok(view! {
        if let Some(e) = &err {
            <div class="qerr"><b>(format!("Couldn't load {what}"))</b><pre>(e.clone())</pre></div>
        }
    })
}

// ————————————————————————————————— spend (§3.3) —————————————————————————————————

/// One `/spend` row: a day × model × developer total, still raw. `from_events`
/// marks a day the metric branch had no cost for, read off the `api_request`
/// events instead.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SpendRow {
    pub(crate) day: String,
    pub(crate) model: String,
    pub(crate) dev: String,
    pub(crate) usd: String,
    pub(crate) tok_in: String,
    pub(crate) tok_out: String,
    pub(crate) tok_cache: String,
    pub(crate) active_s: String,
    pub(crate) from_events: bool,
}

/// Both §3.3 branches, merged: every metric row, then the event rows for the
/// days on which no metric row carries a cost (a logs-only capture, or a
/// sender with metrics off). Newest day first, priciest first within a day.
pub(crate) fn fetch_spend(db: &Db, bar: &Bar, service: &str) -> anyhow::Result<Vec<SpendRow>> {
    let run = |(sql, params): (String, Vec<String>)| -> anyhow::Result<Vec<Vec<String>>> {
        let bind: Vec<&dyn duckdb::ToSql> = params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
        Ok(db.query_rows_with(&sql, &bind)?.1)
    };
    let mut rows: Vec<SpendRow> = run(spend_metrics_query(bar, service))?
        .into_iter()
        .map(|r| SpendRow {
            day: r[0].clone(),
            model: r[1].clone(),
            dev: r[2].clone(),
            usd: r[3].clone(),
            tok_in: r[4].clone(),
            tok_out: r[5].clone(),
            tok_cache: r[6].clone(),
            active_s: r[7].clone(),
            from_events: false,
        })
        .collect();
    let costed: std::collections::HashSet<String> =
        rows.iter().filter(|r| !r.usd.is_empty()).map(|r| r.day.clone()).collect();
    // a failed fallback (no logs table yet) leaves the metric rows standing
    let events = run(spend_events_query(bar, service)).unwrap_or_default();
    rows.extend(events.into_iter().filter(|r| !costed.contains(&r[0])).map(|r| SpendRow {
        day: r[0].clone(),
        model: r[1].clone(),
        dev: r[2].clone(),
        usd: r[3].clone(),
        tok_in: r[4].clone(),
        tok_out: r[5].clone(),
        tok_cache: String::new(),
        active_s: String::new(),
        from_events: true,
    }));
    let usd = |r: &SpendRow| r.usd.parse::<f64>().unwrap_or(-1.0);
    rows.sort_by(|a, b| b.day.cmp(&a.day).then(usd(b).total_cmp(&usd(a))));
    Ok(rows)
}

/// Totals over the shown rows, in the order the strip shows them:
/// usd, tokens in, out, cache, active seconds.
pub(crate) fn spend_totals(rows: &[SpendRow]) -> [f64; 5] {
    let n = |s: &str| s.parse::<f64>().unwrap_or(0.0);
    rows.iter().fold([0.0; 5], |mut t, r| {
        t[0] += n(&r.usd);
        t[1] += n(&r.tok_in);
        t[2] += n(&r.tok_out);
        t[3] += n(&r.tok_cache);
        t[4] += n(&r.active_s);
        t
    })
}

/// Seconds as the table reads them: "45s", "12m", "3h 05m".
fn active_cell(s: &str) -> String {
    let Ok(v) = s.parse::<f64>() else { return s.to_string() };
    let secs = v.round() as i64;
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// One `/spend` row as displayed, every stored cell through `Config::display`.
#[derive(Clone, Debug)]
pub(crate) struct SpendCells {
    pub(crate) day: String,
    pub(crate) model: String,
    pub(crate) dev: String,
    pub(crate) dev_full: String,
    pub(crate) usd: String,
    pub(crate) tok_in: String,
    pub(crate) tok_out: String,
    pub(crate) tok_cache: String,
    pub(crate) active: String,
    pub(crate) from_events: bool,
}

pub(crate) fn spend_cells(r: &SpendRow, cfg: &Config) -> SpendCells {
    let dev = cfg.display(&r.dev);
    SpendCells {
        day: cfg.display(&r.day).chars().take(10).collect(),
        model: match cfg.display(&r.model) {
            m if m.is_empty() => "—".to_string(),
            m => m,
        },
        dev: short_id(&dev),
        dev_full: dev,
        usd: usd_cell(&cfg.display(&r.usd)),
        tok_in: token_cell(&cfg.display(&r.tok_in)),
        tok_out: token_cell(&cfg.display(&r.tok_out)),
        tok_cache: token_cell(&cfg.display(&r.tok_cache)),
        active: if r.active_s.is_empty() { String::new() } else { active_cell(&cfg.display(&r.active_s)) },
        from_events: r.from_events,
    }
}

#[component]
async fn spend_row(cx: &Cx, c: SpendCells) -> Result<impl View> {
    Ok(view! {
        <tr>
            <td class="mono">(c.day.clone())</td>
            <td>(c.model.clone())</td>
            <td><span class="mono" title=(c.dev_full.clone())>(c.dev.clone())</span></td>
            <td class="num">(c.usd.clone())</td>
            <td class="num">(c.tok_in.clone())</td>
            <td class="num">(c.tok_out.clone())</td>
            <td class="num">(c.tok_cache.clone())</td>
            <td class="num">(c.active.clone())</td>
            <td>
                if c.from_events {
                    <span class="badge" title="no cost metric for this day: summed from api_request events">"from events"</span>
                }
            </td>
        </tr>
    })
}

/// `/spend` (§3.3): what the agents cost, by day × model × developer, from
/// the cost/token/active-time metrics — and from the `api_request` events on
/// days without them, badged "from events". A totals strip over the rows on
/// show, under the shared [`Bar`].
#[page("/spend")]
pub(crate) async fn spend_page(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let cfg = context::<Config>(cx);
    let bar = Bar::parse(uri(cx).query().unwrap_or(""));
    let service = bar.get("service").to_string();

    let (rows, load_err) = split_err(fetch_spend(db, &bar, &service));
    let totals = spend_totals(&rows);
    let any_events = rows.iter().any(|r| r.from_events);
    let empty = rows.is_empty() && load_err.is_none();
    let range_href =
        (empty && bar.range_is_narrow()).then(|| bar.href("/spend", &[("range", "all")]));
    let cells: Vec<SpendCells> = rows.iter().map(|r| spend_cells(r, cfg)).collect();
    let services = agent_services(db);
    let range = bar.get("range").to_string();
    let raw_f = bar.raw_f();
    let attr_errors = bar.attr_errors();
    let shown = cfg.agent_show_content;
    let t = totals.map(|v| v.to_string());
    let has_active = totals[4] > 0.0;

    Ok(view! {
        <h1>"Spend"</h1>
        <p class="sub">"What your coding agents cost — by day, model and developer."</p>
        <p class="meta">
            (format!(
                "from the cost and token metrics (kept {}), or the api_request events (kept {}) on days without them",
                short_ttl(&cfg.ttl_metrics),
                short_ttl(&cfg.ttl_logs),
            ))
            content_marker(shown: shown)
        </p>

        <form class="filters" method="get" action="/spend">
            service_field(options: services, current: service)
            range_field(current: range)
            attr_field(raw: raw_f, errors: attr_errors)
            <button type="submit">"Apply"</button>
        </form>

        if !empty {
            <p class="sub meta spend-total">
                <span class="seg"><b>(usd_cell(&t[0]))</b>" total"</span>
                <span class="seg">(token_cell(&t[1]))" in"</span>
                <span class="seg">(token_cell(&t[2]))" out"</span>
                <span class="seg">(token_cell(&t[3]))" cache"</span>
                if has_active { <span class="seg">(active_cell(&t[4]))" active"</span> }
                if any_events { <span class="seg muted">"some days from events"</span> }
            </p>
        }

        load_error(what: "spend", err: load_err)
        <table>
            <thead>
                <tr>
                    <th>"day"</th>
                    <th>"model"</th>
                    <th>"developer"</th>
                    <th>"usd"</th>
                    <th>"tokens in"</th>
                    <th>"out"</th>
                    <th>"cache"</th>
                    <th>"active"</th>
                    <th></th>
                </tr>
            </thead>
            <tbody>
                if let Some(h) = &range_href {
                    empty_range_hint(colspan: 9, noun: "spend", href: h.clone())
                }
                if empty {
                    <tr>
                        <td colspan="9" class="muted">
                            "No agent spend in this view. "
                            <a href="/setup">"Set up Claude Code"</a>
                            " to send its cost metrics here, or clear the filters above."
                        </td>
                    </tr>
                }
                for c in &cells {
                    spend_row(c: c.clone())
                }
            </tbody>
        </table>
    }
    .boxed())
}

// ————————————————————————————————— tool health (§3.4) —————————————————————————————————

/// One `/tools` row as displayed. Latency is blank for the counter fallback,
/// which has no durations.
#[derive(Clone, Debug)]
pub(crate) struct ToolCells {
    pub(crate) tool: String,
    pub(crate) calls: String,
    pub(crate) fails: String,
    /// "" when nothing failed, else the failure share ("25%")
    pub(crate) fail_rate: String,
    pub(crate) p50: String,
    pub(crate) p95: String,
    pub(crate) max: String,
    pub(crate) rejected: String,
    /// drill-downs, each keeping the page's range, service and filters:
    /// the tool's spans, its events, its failed results, its rejections
    pub(crate) traces_href: String,
    pub(crate) logs_href: String,
    pub(crate) fails_href: String,
    pub(crate) rejected_href: String,
}

/// `path` under `bar` with every `key=value` filter added (each one through
/// [`Bar::href_add_filter`], so a filter already on the bar isn't doubled).
fn href_with_filters(bar: &Bar, path: &str, filters: &[(&str, &str)]) -> String {
    let mut b = bar.clone();
    let mut href = b.href(path, &[]);
    for (k, v) in filters {
        href = b.href_add_filter(path, &canonical_of(AttrSource::Row, AttrOp::Eq, k, v));
        b = Bar::parse(href.split_once('?').map(|(_, q)| q).unwrap_or(""));
    }
    href
}

/// Milliseconds the way the waterfall shows them.
fn ms_cell(s: &str) -> String {
    match s.parse::<f64>() {
        Ok(v) if v >= 1000.0 => format!("{:.2} s", v / 1000.0),
        Ok(v) if v >= 10.0 => format!("{v:.0} ms"),
        Ok(v) => format!("{v:.1} ms"),
        Err(_) => s.to_string(),
    }
}

/// `/tools`' rows: the spans branch, or — when it has nothing — the Codex
/// counters; each row with its `tool_decision` rejection count. The second
/// value says which source answered ("spans" | "metrics").
pub(crate) fn fetch_tools(db: &Db, bar: &Bar, service: &str, cfg: &Config) -> (Vec<ToolCells>, &'static str) {
    let run = |(sql, params): (String, Vec<String>)| -> Vec<Vec<String>> {
        let bind: Vec<&dyn duckdb::ToSql> = params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
        db.query_rows_with(&sql, &bind).map(|(_, r)| r).unwrap_or_default()
    };
    let mut rows = run(tool_health_query(bar, service));
    let mut source = "spans";
    if rows.is_empty() {
        rows = run(tool_metrics_query(bar, service));
        source = "metrics";
    }
    let rejected: std::collections::HashMap<String, String> = run(tool_decisions_query(bar, service))
        .into_iter()
        .map(|r| (r[0].clone(), r[1].clone()))
        .collect();
    let cells = rows
        .iter()
        .filter(|r| !r[0].is_empty())
        .map(|r| {
            let get = |i: usize| r.get(i).cloned().unwrap_or_default();
            let calls = get(1).parse::<f64>().unwrap_or(0.0);
            let fails = get(2).parse::<f64>().unwrap_or(0.0);
            // links carry the displayed name: a masked value can't leak via its href
            let tool = cfg.display(&r[0]);
            let by_tool = [("tool_name", tool.as_str())];
            ToolCells {
                traces_href: href_with_filters(bar, "/traces", &by_tool),
                logs_href: href_with_filters(bar, "/logs", &by_tool),
                fails_href: href_with_filters(bar, "/logs", &[("tool_name", &tool), ("success", "false")]),
                rejected_href: href_with_filters(bar, "/logs", &[("tool_name", &tool), ("decision", TOOL_REJECT_DECISION)]),
                tool: tool.clone(),
                calls: thousands(&num_cell(&get(1))),
                fails: thousands(&num_cell(&get(2))),
                fail_rate: if fails > 0.0 && calls > 0.0 {
                    format!("{:.0}%", 100.0 * fails / calls)
                } else {
                    String::new()
                },
                p50: ms_cell(&get(3)),
                p95: ms_cell(&get(4)),
                max: ms_cell(&get(5)),
                rejected: rejected.get(&r[0]).map(|n| num_cell(n)).unwrap_or_default(),
            }
        })
        .collect();
    (cells, source)
}

#[component]
async fn tool_row(cx: &Cx, c: ToolCells) -> Result<impl View> {
    Ok(view! {
        <tr>
            <td><b>(c.tool.clone())</b></td>
            <td class="num">(c.calls.clone())</td>
            <td class="num">
                if c.fail_rate.is_empty() {
                    (c.fails.clone())
                } else {
                    <a href=(c.fails_href.clone()) title="the failed tool_result events">
                        <span class="badge error" title=(format!("{} of {} calls failed", c.fails, c.calls))>
                            (format!("{} · {}", c.fails, c.fail_rate))
                        </span>
                    </a>
                }
            </td>
            <td class="num">(c.p50.clone())</td>
            <td class="num">(c.p95.clone())</td>
            <td class="num">(c.max.clone())</td>
            <td class="num">
                if !c.rejected.is_empty() {
                    <a href=(c.rejected_href.clone())>(c.rejected.clone())</a>
                }
            </td>
            <td>
                <a href=(c.traces_href.clone())>"spans"</a>
                " "
                <a href=(c.logs_href.clone())>"logs"</a>
            </td>
        </tr>
    })
}

/// `/tools` (§3.4): per tool, how often the agents called it, how often it
/// failed, how long it took (p50 / p95 / max execution time), and how often
/// you said no to it — under the shared [`Bar`].
#[page("/tools")]
pub(crate) async fn tools_page(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let cfg = context::<Config>(cx);
    let bar = Bar::parse(uri(cx).query().unwrap_or(""));
    let service = bar.get("service").to_string();

    let (cells, source) = fetch_tools(db, &bar, &service, cfg);
    let empty = cells.is_empty();
    let range_href =
        (empty && bar.range_is_narrow()).then(|| bar.href("/tools", &[("range", "all")]));
    let services = agent_services(db);
    let range = bar.get("range").to_string();
    let raw_f = bar.raw_f();
    let attr_errors = bar.attr_errors();
    let shown = cfg.agent_show_content;
    let note = if source == "metrics" {
        "from the tool-call counters: no tool spans in this view, so no latency"
    } else {
        "from the tool spans: latency is the execution itself, not the wait for your approval"
    };

    Ok(view! {
        <h1>"Tools"</h1>
        <p class="sub">"Which tools your coding agents call, how often they fail, and how long they take."</p>
        <p class="meta">
            (format!("{note} · kept {}", short_ttl(if source == "metrics" { &cfg.ttl_metrics } else { &cfg.ttl_traces })))
            content_marker(shown: shown)
        </p>

        <form class="filters" method="get" action="/tools">
            service_field(options: services, current: service)
            range_field(current: range)
            attr_field(raw: raw_f, errors: attr_errors)
            <button type="submit">"Apply"</button>
        </form>

        <table>
            <thead>
                <tr>
                    <th>"tool"</th>
                    <th>"calls"</th>
                    <th>"failed"</th>
                    <th>"p50"</th>
                    <th>"p95"</th>
                    <th>"max"</th>
                    <th title="tool_decision events with decision=reject">"you rejected"</th>
                    <th></th>
                </tr>
            </thead>
            <tbody>
                if let Some(h) = &range_href {
                    empty_range_hint(colspan: 8, noun: "tool calls", href: h.clone())
                }
                if empty {
                    <tr>
                        <td colspan="8" class="muted">
                            "No tool calls in this view. Claude Code sends tool spans with traces on: "
                            <a href="/setup">"see setup"</a>
                            ", or clear the filters above."
                        </td>
                    </tr>
                }
                for c in &cells {
                    tool_row(c: c.clone())
                }
            </tbody>
        </table>
    }
    .boxed())
}

// ————————————————————————————————— privacy —————————————————————————————————

/// The shared render redactor (§5): the single place stored text is scrubbed
/// before it reaches a page, an attribute chip, an expanded bag or a console
/// result cell.
///
/// * A JSON object or array is walked and every value under a [`CONTENT_KEYS`]
///   name becomes [`REDACTED`]: the key and the shape of the row stay, the
///   contents go. A nested value goes with its key whole — a `tool_input`
///   object *is* the content, not a container to recurse into.
/// * opencode's `["cli starting", "<payload>"]` body is the one real prompt
///   leak in the capture (§2): element 0 (the message) stays, everything after
///   it is the payload and becomes [`REDACTED`].
/// * Text keeps its text, with the two display masks the fixture dump applies
///   too (§5): emails become `[redacted-email@example.invalid]`, and a home
///   path (`$HOME`, `/Users/<name>`, `/home/<name>`) becomes `[redacted-path]`
///   up to its next separator.
///
/// Non-JSON text is returned untouched apart from those masks, and JSON
/// scalars (`1500`, `true`) are not re-serialized into a different shape.
pub fn redact_cell(s: &str) -> String {
    match serde_json::from_str::<Value>(s) {
        Ok(v @ (Value::Object(_) | Value::Array(_))) => mask_value(v).to_string(),
        _ => mask_text(s),
    }
}

fn mask_value(v: Value) -> Value {
    match v {
        Value::Array(items) => match items.first() {
            Some(Value::String(head)) if is_cli_starting(head) => {
                let mut out = Vec::with_capacity(items.len());
                out.push(items[0].clone());
                out.resize(items.len(), Value::String(REDACTED.to_string()));
                Value::Array(out)
            }
            _ => Value::Array(items.into_iter().map(mask_value).collect()),
        },
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| {
                    let hidden = CONTENT_KEYS.iter().any(|c| c.eq_ignore_ascii_case(&k));
                    let value = if hidden {
                        Value::String(REDACTED.to_string())
                    } else {
                        mask_value(v)
                    };
                    (k, value)
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::String(text) => Value::String(mask_text(&text)),
        other => other,
    }
}

/// The opencode marker that means "the next element is the command line,
/// prompt included" (§2) — the same test `agent-fixture.sh` applies when it
/// dumps a fixture, and the test the Fields panel applies to its payload.
pub(crate) fn is_cli_starting(head: &str) -> bool {
    head.trim().to_lowercase().starts_with("cli starting")
}

const EMAIL_PH: &str = "[redacted-email@example.invalid]";
const PATH_PH: &str = "[redacted-path]";

fn mask_text(s: &str) -> String {
    mask_paths(&mask_emails(s))
}

/// The local-part alphabet the fixture dump's email pattern allows.
fn is_email_local(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn is_domain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

/// One `…@host.tld` token at a time. A bare `@`, or one with nothing on either
/// side of it, is text and stays.
fn mask_emails(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('@') {
        let left = &rest[..at];
        let right = &rest[at + 1..];
        let start = left
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_email_local(*c))
            .last()
            .map_or(at, |(i, _)| i);
        let end = right
            .char_indices()
            .take_while(|(_, c)| is_domain_char(*c))
            .last()
            .map_or(0, |(i, c)| i + c.len_utf8());
        let domain = &right[..end];
        let tld_ok = domain.rsplit_once('.').is_some_and(|(host, tld)| {
            !host.is_empty() && tld.len() >= 2 && tld.bytes().all(|c| c.is_ascii_alphabetic())
        });
        if start < at && tld_ok {
            out.push_str(&rest[..start]);
            out.push_str(EMAIL_PH);
            rest = &right[end..];
        } else {
            // not an address: the `@` is text, and the run after it is too
            out.push_str(&rest[..=at]);
            rest = right;
        }
    }
    out.push_str(rest);
    out
}

/// The home prefixes the fixture dump masks: the operator's own `$HOME`, then
/// the conventional macOS and Linux homes.
fn home_prefixes() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(home) = std::env::var("HOME")
        && home.len() > 1
    {
        out.push(home);
    }
    out.push("/Users".to_string());
    out.push("/home".to_string());
    out
}

/// `/Users/jane/secrets.md` → `[redacted-path]/secrets.md`: the mask stops at
/// the next separator, exactly like the dump's patterns — the person's home
/// directory is what identifies them, not the file name.
fn mask_paths(s: &str) -> String {
    let mut out = s.to_string();
    for prefix in home_prefixes() {
        let mut res = String::with_capacity(out.len());
        let mut rest = out.as_str();
        while let Some(at) = rest.find(&prefix) {
            res.push_str(&rest[..at]);
            // the prefix names the *parent* directory (`/Users`), so the
            // component that identifies the person is what follows the slash
            let after = rest[at + prefix.len()..].trim_start_matches('/');
            let tail = after
                .find(['/', '\\', '"', '\'', ' ', '\t', '\n'])
                .unwrap_or(after.len());
            res.push_str(PATH_PH);
            // the remainder is carried by the next round (or the tail push below)
            rest = &after[tail..];
        }
        res.push_str(rest);
        out = res;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ——— privacy: the shared redactor (§5) ————————————————————————————————

    /// This task's Accept line: the list covers every gated attribute the
    /// official docs document (§1), each named with the gate that turns it on.
    #[test]
    fn content_keys_cover_every_gated_attr_in_the_docs() {
        let gated: &[(&str, &str)] = &[
            ("prompt", "OTEL_LOG_USER_PROMPTS"),
            ("prompt_text", "OTEL_LOG_USER_PROMPTS"),
            ("response", "OTEL_LOG_ASSISTANT_RESPONSES"),
            ("tool_input", "OTEL_LOG_TOOL_DETAILS"),
            ("tool_parameters", "OTEL_LOG_TOOL_DETAILS"),
            ("content", "OTEL_LOG_TOOL_CONTENT"),
            ("output", "OTEL_LOG_TOOL_CONTENT"),
            ("diff", "OTEL_LOG_TOOL_CONTENT"),
            ("new_context", "the detailed beta"),
            ("system_reminders", "the detailed beta"),
            ("system_prompt_preview", "the detailed beta"),
            ("user_system_prompt", "the detailed beta"),
            ("response.model_output", "the detailed beta"),
        ];
        for (key, gate) in gated {
            assert!(
                CONTENT_KEYS.contains(key),
                "{key} is gated by {gate}, so it must be redacted"
            );
        }
        // the §5 spellings the other agents, the span events and commands add
        for key in [
            "user_prompt",
            "tool_output",
            "full_command",
            "bash_command",
            "command",
        ] {
            assert!(CONTENT_KEYS.contains(&key), "{key} must be redacted");
        }
    }

    #[test]
    fn redact_cell_masks_content_values_and_keeps_the_counts() {
        let row = r#"{"prompt":"how do I fool the reviewer","prompt_length":24,
            "tool_input":{"command":"ls -la","file_path":"/tmp/hidden"},
            "tool_input_size_bytes":81,"tool_name":"bash","success":"false"}"#;
        let out = redact_cell(row);
        let v: Value = serde_json::from_str(&out).expect("still valid JSON");
        assert_eq!(v["prompt"], REDACTED);
        assert_eq!(v["prompt_length"], 24, "the numeric companion survives");
        assert_eq!(v["tool_input"], REDACTED, "a nested value is the content");
        assert_eq!(v["tool_input_size_bytes"], 81);
        assert_eq!(v["tool_name"], "bash", "the shape of the row is untouched");
        assert_eq!(v["success"], "false");
        assert!(
            !out.contains("fool") && !out.contains("ls -la") && !out.contains("hidden"),
            "{out}"
        );
    }

    #[test]
    fn redact_cell_masks_every_content_key() {
        for key in CONTENT_KEYS {
            let row = format!(r#"{{"{key}":"the agent's words","model":"m"}}"#);
            let out = redact_cell(&row);
            assert!(!out.contains("the agent's words"), "{key} leaked: {out}");
            assert!(out.contains(REDACTED), "{key} not masked: {out}");
            assert!(
                out.contains("\"model\":\"m\""),
                "{key} took its neighbour: {out}"
            );
        }
    }

    #[test]
    fn redact_cell_masks_the_cli_starting_payload_and_keeps_the_message() {
        let body = r#"["cli starting","{\"args\":[\"run\",\"what the user typed\"],\"version\":\"2.3.0\"}"]"#;
        let out = redact_cell(body);
        let v: Value = serde_json::from_str(&out).expect("still a JSON array");
        assert_eq!(
            v[0], "cli starting",
            "the message is the label, not content"
        );
        assert_eq!(v[1], REDACTED, "the payload carries the prompt");
        assert_eq!(v.as_array().map(Vec::len), Some(2), "the shape is kept");
        assert!(
            !out.contains("what the user typed") && !out.contains("2.3.0"),
            "{out}"
        );
        // the marker is matched case- and padding-insensitively, like the dump
        assert!(redact_cell(r#"["  CLI Starting ","x"]"#).contains(REDACTED));
        // any other array keeps its elements (subject to the key rules)
        assert_eq!(
            redact_cell(r#"["tool_result","ok"]"#),
            r#"["tool_result","ok"]"#
        );
    }

    #[test]
    fn redact_cell_leaves_plain_text_and_scalars_alone() {
        assert_eq!(
            redact_cell("plain text that is not JSON at all"),
            "plain text that is not JSON at all"
        );
        assert_eq!(redact_cell("1500"), "1500");
        assert_eq!(redact_cell("true"), "true");
        assert_eq!(redact_cell(""), "");
        assert_eq!(redact_cell("{not json"), "{not json");
        // a content key spelled in text that is not JSON is not a JSON value
        assert_eq!(redact_cell("prompt: none"), "prompt: none");
    }

    #[test]
    fn redact_cell_masks_emails_and_home_paths_in_display_text() {
        assert_eq!(
            redact_cell("mail jane.doe+work@example.com about it"),
            format!("mail {EMAIL_PH} about it")
        );
        assert_eq!(
            redact_cell("see /Users/jane/secrets.md now"),
            format!("see {PATH_PH}/secrets.md now")
        );
        assert_eq!(
            redact_cell("/home/jane/.aws/credentials"),
            format!("{PATH_PH}/.aws/credentials")
        );
        // inside JSON string values too
        let out = redact_cell(r#"{"msg":"wrote /Users/jane/app.rs for jane@corp.co","n":2}"#);
        assert!(out.contains(&format!("{PATH_PH}/app.rs")), "{out}");
        assert!(out.contains(EMAIL_PH), "{out}");
        assert!(out.contains("\"n\":2"));
        // not every `@` is an address, and not every slash is a home
        assert_eq!(
            redact_cell(r#"{"a":"100% @ 5","b":"user@no tld"}"#),
            r#"{"a":"100% @ 5","b":"user@no tld"}"#
        );
        assert_eq!(redact_cell("GET /api/users"), "GET /api/users");
    }

    // ——— the shared expressions (§2, §3) ————————————————————————————————

    #[test]
    fn session_expr_coalesces_the_agent_keys_in_order() {
        let sql = session_expr("log_attributes");
        let params = session_params();
        assert_eq!(params, paths(SESSION_KEYS));
        assert_eq!(sql.matches('?').count(), params.len());
        // one parse of the bag, not one per key (agent-stress OOM at N=500)
        assert_eq!(sql.matches("json_extract_string").count(), 1);
        assert_eq!(sql.matches("json_valid(log_attributes)").count(), 1);
        assert!(sql.contains("lambda x: x IS NOT NULL)[1]"), "first non-NULL, in key order");
        // Claude Code's own id first, the captured Codex keys next, opencode's
        // run id last, so a row carrying several groups by the real id
        assert_eq!(SESSION_KEYS[0], "session.id");
        assert_eq!(SESSION_KEYS[1], "gen_ai.conversation.id");
        assert!(!SESSION_KEYS.contains(&"thread.id"), "Codex's thread.id is an OS thread number");
        for key in [
            "conversation.id",
            "thread_id",
            "turn.id",
            "turn_id",
        ] {
            assert!(SESSION_KEYS.contains(&key), "captured Codex key {key}");
        }
        assert_eq!(SESSION_KEYS.last(), Some(&"opencode.run"));
        // the paths are parameters, never text
        assert!(!sql.contains("session.id") && !sql.contains("opencode.run"));
    }

    #[test]
    fn session_expr_bags_cover_the_row_and_the_resource_bag() {
        let (sql, params) = session_expr_bags(&METRIC_COLS, Some("mt"));
        assert!(sql.starts_with("COALESCE((CASE WHEN json_valid(mt.metric_attributes)"));
        assert!(sql.contains("mt.metric_attributes") && sql.contains("mt.resource_attributes"));
        assert_eq!(params.len(), SESSION_KEYS.len() * 2);
        assert_eq!(sql.matches('?').count(), params.len());
    }

    #[test]
    fn session_attr_conds_or_every_key_in_both_bags() {
        let conds = session_attr_cols(&LOG_COLS, Some("lg"));
        let params = session_attr_params(&LOG_COLS, "ses_1");
        assert_eq!(conds.len(), 2, "one condition per bag");
        assert_eq!(params.len(), (SESSION_KEYS.len() + 3) * 2);
        let sql = conds.join(" OR ");
        assert_eq!(sql.matches('?').count(), params.len());
        assert!(conds.iter().all(|c| c.contains("list_contains(") && c.contains("contains(lg.") && c.ends_with("?) END))")), "{:?}", conds[0]);
        assert!(sql.contains("lg.log_attributes") && sql.contains("lg.resource_attributes"));
        assert!(
            !sql.contains("body"),
            "no agent keeps a session id in a body"
        );
        // the guard's id twice, every key path, then the id, per bag
        assert_eq!(&params[..2], ["ses_1", "ses_1"]);
        assert_eq!(params[2], SESSION_ID_PATH);
        assert_eq!(params[SESSION_KEYS.len() + 2], "ses_1");
        assert!(!sql.contains("ses_1"), "the id stays out of the text");
    }

    #[test]
    fn event_name_expr_reads_the_attribute_first() {
        let sql = event_name_expr(Some("lg"));
        let attr = sql.find("log_attributes").unwrap();
        let col = sql.find("NULLIF(lg.event_name").unwrap();
        assert!(
            attr < col,
            "§2 diff 1: the column holds junk, the attr is truth"
        );
        assert!(sql.starts_with("COALESCE(") && sql.ends_with("NULLIF(lg.event_name, ''))"));
        assert_eq!(event_name_params(), vec![EVENT_NAME_PATH.to_string()]);
        assert_eq!(sql.matches('?').count(), event_name_params().len());
        // an unaliased page reads the bare columns
        assert!(event_name_expr(None).contains("COALESCE((CASE WHEN json_valid(log_attributes)"));
    }

    #[test]
    fn tool_and_success_helpers_compare_text() {
        let sql = tool_name_expr("span_attributes");
        assert_eq!(sql.matches('?').count(), tool_name_params().len());
        assert_eq!(tool_name_params(), paths(TOOL_NAME_KEYS));
        // the string "true" on events and the boolean on spans (§2 diff 2)
        // both become text, and a row with no key counts as a success
        let ok = success_expr("span_attributes");
        assert!(
            ok.starts_with("lower(COALESCE(") && ok.ends_with("'true'))"),
            "{ok}"
        );
        assert_eq!(success_params(), vec![attr_path(SUCCESS_KEY)]);
        assert_eq!(attr_path("a.b"), r#"$."a.b""#);
        assert_eq!(attr_path(r#"a"b"#), r#"$."a\"b""#);
    }

    // ——— the SQL builders: Bar conds round-trip, text stays clean ————————

    /// The J0 invariant every builder keeps: exactly one bound parameter per
    /// `?`, and none of what the user typed in the statement.
    fn assert_bound(sql: &str, params: &[String], user_text: &[&str]) {
        assert_eq!(
            sql.matches('?').count(),
            params.len(),
            "every `?` must have a parameter:\n{sql}"
        );
        for bad in user_text {
            assert!(
                !sql.contains(bad),
                "user text `{bad}` reached the SQL text:\n{sql}"
            );
        }
    }

    #[test]
    fn sessions_query_binds_the_bar_service_and_limit() {
        let bar = Bar::parse("f=tool_name%3Dbash&f=resource.env%3Dprod&limit=50");
        let (sql, params) = sessions_query(&bar, "claude-code");
        assert_bound(
            &sql,
            &params,
            &["bash", "prod", "claude-code", "50", "session.id"],
        );
        assert_eq!(params[params.len() - 1], "50", "the cap is bound");
        for table in [
            "otel.main.otlp_logs lg",
            "otel.main.otlp_traces tr",
            "otel.main.otlp_metrics_sum mt",
        ] {
            assert!(sql.contains(table), "missing branch {table}");
        }
        assert_eq!(sql.matches(" UNION ALL ").count(), 2);
        assert!(sql.contains("GROUP BY ev.sid ORDER BY last_seen DESC"));
        assert!(sql.contains("count(*) FILTER (WHERE ev.sig = 'spans') AS spans"));
        // every branch filters on its own alias, and the bar's filters ride it
        for c in [
            "lg.service_name = ?",
            "tr.service_name = ?",
            "mt.service_name = ?",
            "lg.log_attributes",
            "tr.span_attributes",
            "mt.resource_attributes",
        ] {
            assert!(sql.contains(c), "missing {c}");
        }
    }

    #[test]
    fn sessions_query_keeps_hostile_filter_text_out_of_the_sql() {
        let evil = r#"a"),(SELECT 1"#;
        let bar = Bar::parse(&format!("f={}", Bar::enc(&format!("{evil}=x"))));
        let (sql, params) = sessions_query(&bar, "");
        assert_bound(&sql, &params, &["SELECT 1", evil]);
        assert!(
            params.iter().any(|p| p.starts_with("$.\"a")),
            "the hostile key is data: {params:?}"
        );
        // an empty service filters nothing
        let (sql, _) = sessions_query(&Bar::default(), "");
        assert!(!sql.contains("service_name = ?"));
    }

    #[test]
    fn session_events_query_binds_the_id_and_the_bar() {
        let bar = Bar::parse("f=model%3Dsonnet");
        let (sql, params) = session_events_query(&bar, "ses_1");
        assert_bound(&sql, &params, &["ses_1", "sonnet"]);
        assert!(params.iter().any(|p| p == "ses_1"), "{params:?}");
        assert!(params.iter().any(|p| p == "sonnet"));
        assert!(sql.contains("NULLIF(lg.event_name, '')"));
        assert!(sql.ends_with(&format!("ORDER BY lg.time_unix_nano LIMIT {EVENTS_SHOWN}")), "oldest first, capped");
        // the event-name path is the first `?`, since the projection leads
        assert_eq!(params[0], EVENT_NAME_PATH);
    }

    #[test]
    fn spend_queries_bind_names_models_and_the_bar() {
        let bar = Bar::parse("f=model%3Dsonnet");
        let (sql, params) = spend_metrics_query(&bar, "claude-code");
        assert_bound(
            &sql,
            &params,
            &["claude-code", "claude_code.cost.usage", "input", "sonnet"],
        );
        assert!(sql.contains("date_trunc('day', time_unix_nano)"));
        assert!(sql.contains("GROUP BY 1, 2, 3 ORDER BY d DESC, usd DESC NULLS LAST"));
        assert!(sql.contains("(\"name\" IN (?, ?, ?, ?))"));
        assert!(
            params.contains(&SPEND_METRIC_NAMES[3].to_string()),
            "the INFERRED Codex metric rides along, to be pinned later"
        );
        assert!(params.contains(&"cache%".to_string()));

        let (sql, params) = spend_events_query(&bar, "");
        assert_bound(&sql, &params, &["api_request", "cost_usd", "sonnet"]);
        assert!(sql.contains("NULLIF(lg.event_name, '')"));
        assert!(params.contains(&API_REQUEST_EVENT.to_string()));
    }

    #[test]
    fn tool_health_queries_bind_names_and_the_bar() {
        let bar = Bar::parse("f=tool_name%3Dbash");
        let (sql, params) = tool_health_query(&bar, "");
        assert_bound(
            &sql,
            &params,
            &["claude_code.tool", "codex.tool.call", "claude_code.tool.execution", "bash"],
        );
        assert!(sql.contains("quantile_cont(COALESCE(x.dur, t.dur) / 1e6, 0.5) AS p50_ms"));
        assert!(sql.contains("quantile_cont(COALESCE(x.dur, t.dur) / 1e6, 0.95) AS p95_ms"));
        assert!(sql.contains("LEFT JOIN x ON x.trace_id = t.trace_id AND x.use_id = t.use_id"));
        assert!(sql.contains("GROUP BY 1 ORDER BY calls DESC"));
        assert!(params.contains(&TOOL_SPAN_NAMES[0].to_string()));
        assert!(!sql.contains(">= CAST(? AS TIMESTAMP)"), "range=all adds no time condition");
        let (sql, _) = tool_health_query(&Bar::parse("range=1h"), "");
        assert!(sql.contains("start_time_unix_nano >= CAST(? AS TIMESTAMP)"), "a narrow range narrows");

        let (sql, params) = tool_metrics_query(&bar, "");
        assert_bound(&sql, &params, &["codex.tool.call", "bash"]);
        assert!(sql.contains("sum(double_value) AS calls"));
        assert!(params.contains(&TOOL_METRIC_NAME.to_string()));

        let (sql, params) = tool_decisions_query(&bar, "");
        assert_bound(&sql, &params, &["'tool_decision'", "'reject'", "bash"]);
        assert!(sql.contains("count(*) AS rejected"));
        assert!(params.contains(&TOOL_DECISION_EVENT.to_string()));
        assert!(params.contains(&TOOL_REJECT_DECISION.to_string()));
    }

    // ——— /sessions render (T3) —————————————————————————————————————————————

    use topcoat::{context::Cx, view::{ViewExt, view}};

    pub(super) fn cfg(show: bool) -> Config {
        Config {
            host: "127.0.0.1".into(),
            port: 0,
            data_dir: std::env::temp_dir(),
            extension_dir: None,
            ttl_logs: "7 days".into(),
            ttl_traces: "3 days".into(),
            ttl_metrics: "30 days".into(),
            max_bytes: "64MB".into(),
            otlp_addr: "127.0.0.1:0".into(),
            ingest_token: None,
            agent_show_content: show,
        }
    }

    fn row() -> SessionRow {
        SessionRow {
            sid: "218acd0a-68ea-4150-84e2-5df4bf0af31c".into(),
            service: "claude-code".into(),
            events: "77".into(),
            spans: "16".into(),
            last_seen: "2026-11-18 09:00:00".into(),
            usd: "0.0591256".into(),
            tok_in: "56922.0".into(),
            tok_out: "78.0".into(),
        }
    }

    /// The empty state (T3 Accept): the list points a fresh user at /setup.
    #[tokio::test]
    async fn sessions_empty_row_points_at_setup() {
        let cx = Cx::new();
        let html = { let cx = &cx; view! { sessions_empty_row(colspan: 8) } }.single().await.unwrap().render(&cx);
        assert!(html.to_lowercase().contains("no agent sessions"), "{html}");
        assert!(html.contains("href=\"/setup\""), "the empty state points at /setup: {html}");
    }

    /// §3.1's row contract: truncated mono id linking to the timeline, a
    /// pre-filtered logs link, badge, `.num` cells, relative last seen.
    #[tokio::test]
    async fn session_row_links_to_timeline_and_prefiltered_logs() {
        let cx = Cx::new();
        let sid = row().sid.clone();
        let c = session_cells(&row(), &Bar::default(), &cfg(false));
        assert_eq!(c.timeline, format!("/sessions?session={sid}"), "timeline link");
        assert_eq!(c.logs, format!("/logs?f=session.id%3D{sid}"), "logs link via href_add_filter");
        assert_eq!(c.label, "218acd0a…", "id truncated in the cell");
        assert_eq!(c.usd, "$0.06");
        assert_eq!(c.traces, format!("/traces?f=session.id%3D{sid}"), "traces link, same filter");
        assert_eq!(c.tokens, "56,922 / 78", "tokens in / out, no .0 noise");
        assert!(c.last.ends_with("ago"), "last seen is relative: {}", c.last);
        let html = { let cx = &cx; view! { session_row(c: c.clone()) } }.single().await.unwrap().render(&cx);
        assert!(html.contains("218acd0a…") && html.contains("class=\"badge\""), "{html}");

        // no cost/token rollup (Codex, opencode): blank cells, never "$0" or " / "
        let bare = SessionRow { usd: String::new(), tok_in: String::new(), tok_out: String::new(), ..row() };
        let b = session_cells(&bare, &Bar::default(), &cfg(false));
        assert_eq!((b.usd.as_str(), b.tokens.as_str()), ("", ""));
        let spanless = SessionRow { spans: "0".into(), ..row() };
        assert_eq!(session_cells(&spanless, &Bar::default(), &cfg(false)).traces, "", "no spans, no traces link");
    }

    #[test]
    fn event_band_chips_exclude_content_keys() {
        // a content key that is also a glance key would still be dropped:
        // every chip key is checked against CONTENT_KEYS
        for k in EVENT_CHIP_KEYS {
            assert!(!CONTENT_KEYS.contains(k), "{k} is content and must never be a chip");
        }
        let attrs = r#"{"event.name":"tool_result","tool_name":"Bash","success":true,
            "duration_ms":1500,"tool_input":"rm -rf secret","prompt":"a synthetic prompt",
            "user.email":"jane.doe@corp.example"}"#;
        let row = |show| event_cells(
            &["tool_result".into(), "2026-11-17 10:00:00".into(), attrs.into(), String::new()],
            &cfg(show),
        );
        for show in [false, true] {
            let e = row(show);
            let keys: Vec<&str> = e.chips.iter().map(|(k, _)| k.as_str()).collect();
            assert_eq!(keys, vec!["tool_name", "success", "duration_ms"], "glance keys only, in order");
            let cost = event_cells(&["api_request".into(), "t".into(), r#"{"cost_usd":0.017056400000000003}"#.into(), String::new()], &cfg(show));
            assert_eq!(cost.chips, vec![("cost_usd".to_string(), "$0.02".to_string())]);
            assert!(e.chips.iter().all(|(_, v)| !v.contains("secret") && !v.contains("synthetic")));
        }
        // the expandable bag is masked unless the operator opted in
        let hidden = row(false).bag;
        assert!(!hidden.contains("rm -rf secret") && !hidden.contains("a synthetic prompt"), "{hidden}");
        assert!(!hidden.contains("jane.doe@corp.example"), "emails masked in the bag");
        assert!(hidden.contains("tool_input"), "the key stays, the content goes");
        assert!(row(true).bag.contains("rm -rf secret"), "opt-in shows the bag");
    }

    #[test]
    fn repeated_chipless_events_fold_into_one_row() {
        let ev = |name: &str, chips: bool| EventCells {
            time: "t".into(),
            name: name.into(),
            chips: if chips { vec![("tool_name".into(), "Bash".into())] } else { vec![] },
            bag: String::new(),
            repeat: 1,
        };
        let folded = fold_repeats(vec![
            ev("hook_registered", false), ev("hook_registered", false), ev("hook_registered", false),
            ev("plugin_loaded", false),
            ev("tool_result", true), ev("tool_result", true),
            ev("hook_registered", false),
        ]);
        let got: Vec<(&str, usize)> = folded.iter().map(|e| (e.name.as_str(), e.repeat)).collect();
        assert_eq!(got, vec![
            ("hook_registered", 3), ("plugin_loaded", 1),
            ("tool_result", 1), ("tool_result", 1), ("hook_registered", 1),
        ], "chipped events keep their rows; only adjacent runs fold");
    }

    #[test]
    fn unnamed_events_take_their_body_message_never_the_payload() {
        let body = r#"["cli starting","{\"prompt\":\"a synthetic prompt\"}"]"#;
        let e = event_cells(&[String::new(), "t".into(), "{}".into(), body.into()], &cfg(false));
        assert_eq!(e.name, "cli starting");
        assert!(e.bag.is_empty() && e.chips.is_empty());
        let plain = event_cells(&[String::new(), "t".into(), String::new(), String::new()], &cfg(false));
        assert_eq!(plain.name, "event", "nothing to name it by");
    }

    #[test]
    fn ms_cells_read_like_the_waterfall() {
        assert_eq!(ms_cell("800.0"), "800 ms");
        assert_eq!(ms_cell("2.5"), "2.5 ms");
        assert_eq!(ms_cell("258247.0"), "258.25 s");
        assert_eq!(ms_cell(""), "");
    }

    #[test]
    fn spend_cells_read_like_money_and_time() {
        assert_eq!(active_cell("45"), "45s");
        assert_eq!(active_cell("720.4"), "12m");
        assert_eq!(active_cell("11100"), "3h 05m");
        let r = SpendRow {
            day: "2026-11-17 00:00:00".into(), model: String::new(),
            dev: "4f1c2d3e4f5a6b7c8d9e".into(), usd: "0.31582839999999995".into(),
            tok_in: "1288.0".into(), tok_out: "1137".into(), tok_cache: String::new(),
            active_s: "300.2".into(), from_events: true,
        };
        let c = spend_cells(&r, &cfg(false));
        assert_eq!((c.day.as_str(), c.model.as_str(), c.usd.as_str()), ("2026-11-17", "—", "$0.32"));
        assert_eq!((c.tok_in.as_str(), c.tok_cache.as_str(), c.active.as_str()), ("1,288", "", "5m"));
        assert_eq!(c.dev, "4f1c2d3e…");
        assert!(c.from_events);
    }

    #[test]
    fn number_and_time_cells_treat_the_stored_text() {
        assert_eq!(num_cell("40.0"), "40");
        assert_eq!(num_cell("0.5"), "0.5");
        assert_eq!(num_cell("0.0189364"), "0.0189364");
        assert_eq!(num_cell("1200"), "1200");
        assert_eq!(num_cell(""), "");
        assert_eq!(num_cell("db error"), "db error");
        assert_eq!(usd_cell("0.5"), "$0.50");
        assert_eq!(usd_cell("0.31582839999999995"), "$0.32", "float noise rounds to cents");
        assert_eq!(usd_cell("0.00985177"), "$0.0099", "sub-cent keeps 4 places");
        assert_eq!(usd_cell("0"), "$0");
        assert_eq!(usd_cell(""), "", "a session without the cost metric shows no $0");
        assert_eq!(token_cell("56922.0"), "56,922");
        let now = chrono::Utc::now().naive_utc().format("%Y-%m-%d %H:%M:%S").to_string();
        assert!(ago(&now).ends_with("ago"), "recent rows read as relative: {}", ago(&now));
        assert_eq!(ago("not a timestamp"), "not a timestamp");
        assert_eq!(short_id("run_7"), "run_7");
        assert_eq!(short_id("218acd0a-68ea-4150-84e2-5df4bf0af31c"), "218acd0a…");
    }
}

// ————————————————————————————— DB fixtures ————————————————————————————————
// The agent views' promise in executable form: the SQL the builders emit runs
// against the real store and groups the three agents' rows by the key each of
// them actually carries (§2). The rows are synthetic, shaped like the measured
// capture — no captured prompt text, tool content or paths is in here.

#[cfg(test)]
pub(crate) mod db_tests {
    use super::{
        fetch_sessions, num_cell, session_events_query, sessions_query, spend_events_query,
        spend_metrics_query, tool_decisions_query, tool_health_query, tool_metrics_query,
    };
    use crate::filters::Bar;
    use crate::logs::db_tests::temp_db;
    use std::fs;

    /// The Claude Code rows §2 measured: the event name in the attributes with
    /// the OTLP `event_name` column left empty, `success` as a string,
    /// `session.id` on every row, and the Codex shape beside it — where
    /// `event_name` holds a Rust source location and the session key is
    /// `conversation.id` — plus opencode, whose only per-run key is the
    /// resource attribute `opencode.run`, and the seed's own keyless row.
    pub(crate) fn seed_logs(db: &crate::db::Db) {
        db.with_conn(|conn| {
            conn.execute_batch(
                r#"CREATE TABLE otel.main.otlp_logs(
                        time_unix_nano TIMESTAMP, event_name VARCHAR, service_name VARCHAR,
                        body VARCHAR, resource_attributes VARCHAR, log_attributes VARCHAR)
                    WITH (time_col = 'time_unix_nano', ttl = '7 days', max_bytes = '64MB');
                    INSERT INTO otel.main.otlp_logs
                        (time_unix_nano, event_name, service_name, body, resource_attributes, log_attributes)
                    VALUES
                        -- Claude Code: the name rides in the attributes, event_name is empty
                        ('2026-11-17 10:00:00', '', 'claude-code', 'claude_code.user_prompt',
                         '{"service.name":"claude-code"}',
                         '{"session.id":"ses_cc","event.name":"user_prompt","prompt":"a synthetic prompt","prompt_length":18,"user.id":"dev-1","model":"m-large"}'),
                        ('2026-11-17 10:00:01', '', 'claude-code', 'claude_code.tool_result',
                         '{"service.name":"claude-code"}',
                         '{"session.id":"ses_cc","event.name":"tool_result","tool_name":"bash","success":"false","duration_ms":1500,"user.id":"dev-1"}'),
                        ('2026-11-17 10:00:02', '', 'claude-code', 'claude_code.api_request',
                         '{"service.name":"claude-code"}',
                         '{"session.id":"ses_cc","event.name":"api_request","model":"m-large","user.id":"dev-1","cost_usd":0.25,"input_tokens":40,"output_tokens":7,"duration_ms":900}'),
                        -- a row whose event name only the OTLP column carries: the fallback
                        ('2026-11-17 10:00:03', 'retention_sweep', 'claude-code', '',
                         '{"service.name":"claude-code"}', '{"session.id":"ses_cc"}'),
                        -- Claude Code's tool_decision: one reject, one accept
                        ('2026-11-17 10:00:04', '', 'claude-code', 'claude_code.tool_decision',
                         '{"service.name":"claude-code"}',
                         '{"session.id":"ses_cc","event.name":"tool_decision","tool_name":"bash","decision":"reject","source":"user"}'),
                        ('2026-11-17 10:00:05', '', 'claude-code', 'claude_code.tool_decision',
                         '{"service.name":"claude-code"}',
                         '{"session.id":"ses_cc","event.name":"tool_decision","tool_name":"read","decision":"accept","source":"user"}'),
                        -- Codex: event_name holds a Rust source location, the name is in
                        -- the attributes, and the session key is conversation.id
                        ('2026-11-17 10:00:06', 'event otel/src/events/session_telemetry.rs:830',
                         'codex_exec', 'codex.user_prompt', '{"service.name":"codex_exec"}',
                         '{"conversation.id":"conv_9","event.name":"codex.user_prompt","model":"gpt-codex","prompt":"[REDACTED]","prompt_length":9}'),
                        -- opencode: no session key anywhere but the resource run id
                        ('2026-11-17 10:00:07', '', 'opencode',
                         '["cli starting","{\"args\":[\"run\",\"a synthetic prompt\"]}"]',
                         '{"service.name":"opencode","opencode.run":"run_7"}', '{}'),
                        -- the seed's own row: no session keys, so it is not an agent session
                        ('2026-11-17 10:00:08', '', 'cart', 'cart ready',
                         '{"service.name":"cart"}', '{}');"#,
            )?;
            Ok(())
        })
        .expect("seed agent logs");
    }

    /// Spans shaped like the capture: Claude Code's `interaction → tool →
    /// tool.execution` chain on one session (the tool span carries `tool_name`,
    /// the execution span `success` — §1, §2), a Codex tool span on
    /// `thread_id`, and opencode's spans keyed only by the resource run id,
    /// across two traces so the waterfall has more than one tree to lay out.
    pub(crate) fn seed_traces(db: &crate::db::Db) {
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE otel.main.otlp_traces(
                    start_time_unix_nano TIMESTAMP, duration_time_unix_nano BIGINT,
                    trace_id VARCHAR, span_id VARCHAR, parent_span_id VARCHAR,
                    service_name VARCHAR, \"name\" VARCHAR, kind INTEGER,
                    status_code INTEGER, status_status_message VARCHAR,
                    resource_attributes VARCHAR, span_attributes VARCHAR)
                 WITH (time_col = 'start_time_unix_nano', ttl = '3 days', max_bytes = '64MB');
                 INSERT INTO otel.main.otlp_traces
                     (start_time_unix_nano, duration_time_unix_nano, trace_id, span_id, parent_span_id,
                      service_name, \"name\", kind, status_code, status_status_message, resource_attributes, span_attributes)
                 VALUES
                    ('2026-11-17 10:00:00', 3000000000, 't1', 'sp1', '',
                     'claude-code', 'claude_code.interaction', 1, 0, '',
                     '{\"service.name\":\"claude-code\"}', '{\"session.id\":\"ses_cc\"}'),
                    ('2026-11-17 10:00:01', 1000000000, 't1', 'sp2', 'sp1',
                     'claude-code', 'claude_code.tool', 1, 0, '',
                     '{\"service.name\":\"claude-code\"}', '{\"session.id\":\"ses_cc\",\"tool_name\":\"bash\",\"tool_use_id\":\"toolu_1\"}'),
                    ('2026-11-17 10:00:01', 800000000, 't1', 'sp3', 'sp2',
                     'claude-code', 'claude_code.tool.execution', 1, 0, '',
                     '{\"service.name\":\"claude-code\"}', '{\"session.id\":\"ses_cc\",\"tool_use_id\":\"toolu_1\",\"success\":false}'),
                    ('2026-11-17 10:00:02', 200000000, 't2', 'sp5', '',
                     'codex_exec', 'codex.tool.call', 1, 0, '',
                     '{\"service.name\":\"codex_exec\"}', '{\"thread_id\":\"conv_9\",\"thread.id\":\"11\",\"tool_name\":\"apply_patch\",\"success\":\"false\"}'),
                    ('2026-11-17 10:00:03', 40000000, 't3', 'sp6', '',
                     'opencode', 'rpc.client', 2, 0, '',
                     '{\"service.name\":\"opencode\",\"opencode.run\":\"run_7\"}', '{}'),
                    ('2026-11-17 10:00:03', 20000000, 't3', 'sp7', 'sp6',
                     'opencode', 'http.request', 3, 0, '',
                     '{\"service.name\":\"opencode\",\"opencode.run\":\"run_7\"}', '{}'),
                    ('2026-11-17 10:00:04', 10000000, 't4', 'sp8', '',
                     'opencode', 'rpc.client', 2, 0, '',
                     '{\"service.name\":\"opencode\",\"opencode.run\":\"run_7\"}', '{}'),
                    ('2026-11-17 10:00:05', 10000000, 't5', 'sp9', '',
                     'cart', 'GET /cart', 2, 0, '',
                     '{\"service.name\":\"cart\"}', '{}');",
            )?;
            Ok(())
        })
        .expect("seed agent traces");
    }

    /// The metric points Claude Code attaches its `session.id` to, and the
    /// INFERRED Codex tool counter.
    pub(crate) fn seed_metrics(db: &crate::db::Db) {
        db.with_conn(|conn| {
            conn.execute_batch(
                "CREATE TABLE otel.main.otlp_metrics_sum(
                    time_unix_nano TIMESTAMP, \"name\" VARCHAR, double_value DOUBLE,
                    service_name VARCHAR, resource_attributes VARCHAR, metric_attributes VARCHAR,
                    aggregation_temporality INTEGER)
                 WITH (time_col = 'time_unix_nano', ttl = '30 days', max_bytes = '64MB');
                 INSERT INTO otel.main.otlp_metrics_sum
                     (time_unix_nano, \"name\", double_value, service_name, resource_attributes, metric_attributes, aggregation_temporality)
                 VALUES
                    ('2026-11-17 10:00:00', 'claude_code.cost.usage', 0.5, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\"}', 1),
                    ('2026-11-17 10:00:00', 'claude_code.token.usage', 40, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\",\"type\":\"input\"}', 1),
                    ('2026-11-17 10:00:00', 'claude_code.token.usage', 7, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\",\"type\":\"output\"}', 1),
                    ('2026-11-17 10:00:00', 'claude_code.token.usage', 900, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\",\"type\":\"cacheRead\"}', 1),
                    ('2026-11-17 10:00:00', 'claude_code.active_time.total', 120, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"user.id\":\"dev-1\",\"type\":\"user\"}', 1),
                    ('2026-11-17 10:00:00', 'codex.tool.call', 3, 'codex_exec',
                     '{}', '{\"tool\":\"apply_patch\",\"success\":\"false\"}', 1),
                    ('2026-11-17 10:00:00', 'some.other.metric', 1, 'claude-code',
                     '{}', '{}', 1),
                    -- a cumulative point (temporality 2): a running total,
                    -- never summed as if it were a delta
                    ('2026-11-17 09:00:00', 'claude_code.cost.usage', 99, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\"}', 2);",
            )?;
            Ok(())
        })
        .expect("seed agent metrics");
    }

    /// Run a builder and hand back its rows.
    fn rows_of(sql: String, params: Vec<String>, db: &crate::db::Db) -> Vec<Vec<String>> {
        let bind: Vec<&dyn duckdb::ToSql> =
            params.iter().map(|s| s as &dyn duckdb::ToSql).collect();
        db.query_rows_with(&sql, &bind).expect("builder SQL runs").1
    }

    /// `(sessions, service, events, spans, usd, tokens in/out)` for the list.
    fn sessions(tag: &str, query: &str, service: &str) -> Vec<Vec<String>> {
        let db = temp_db(tag);
        seed_logs(&db);
        seed_traces(&db);
        seed_metrics(&db);
        let (sql, params) = sessions_query(&Bar::parse(query), service);
        let out = rows_of(sql, params, &db);
        let _ = fs::remove_dir_all(&db.data_dir);
        out
    }

    #[test]
    fn sessions_query_groups_each_agent_by_the_key_it_carries() {
        let all = sessions("ag-sessions", "", "");
        let by_id: Vec<Vec<String>> = all
            .iter()
            .map(|r| {
                vec![
                    r[0].clone(),
                    r[1].clone(),
                    r[2].clone(),
                    r[3].clone(),
                    r[6].clone(),
                ]
            })
            .collect();
        // id, service, events, spans, usd — newest session first
        assert_eq!(
            by_id,
            vec![
                // cart and the keyless rows never appear: they have no session key
                vec![
                    "run_7".to_string(),
                    "opencode".to_string(),
                    "4".to_string(),
                    "3".to_string(),
                    String::new()
                ],
                vec![
                    "conv_9".to_string(),
                    "codex_exec".to_string(),
                    "2".to_string(),
                    "1".to_string(),
                    String::new()
                ],
                vec![
                    "ses_cc".to_string(),
                    "claude-code".to_string(),
                    "15".to_string(),
                    "3".to_string(),
                    "0.5".to_string()
                ],
            ],
            "Claude Code groups by session.id with its cost rollup attached, \
             Codex by conversation.id, opencode by the resource opencode.run"
        );
    }

    #[test]
    fn sessions_query_service_and_bar_narrow_the_list() {
        let ids = |rows: Vec<Vec<String>>| -> Vec<String> {
            rows.into_iter().map(|r| r[0].clone()).collect()
        };
        assert_eq!(
            ids(sessions("ag-svc", "", "opencode")),
            vec!["run_7".to_string()]
        );
        assert!(
            sessions("ag-svc-none", "", "cart").is_empty(),
            "the seed has no sessions"
        );
        // one attribute filter matches in whichever signal the row lives in
        assert_eq!(
            ids(sessions("ag-tool", "f=tool_name%3Dbash", "")),
            vec!["ses_cc".to_string()],
            "tool_name is on a span and on events of one session"
        );
        // a filter nothing carries leaves nothing, and is not an error
        assert!(sessions("ag-none", "f=tool_name%3Dnope", "").is_empty());
        // the bar's cap is honoured
        assert_eq!(sessions("ag-limit", "limit=2", "").len(), 2);
    }

    /// The page's own data path (beyond the builder): [`fetch_sessions`]
    /// projects the union rows into the typed shape the row renderer reads,
    /// and the bar narrows it — the list's `f=session.id=` narrowing, the
    /// service select's `service=` narrowing, and the empty selection.
    #[test]
    fn fetch_sessions_projects_and_narrows() {
        let db = temp_db("ag-fetch");
        seed_logs(&db);
        seed_traces(&db);
        seed_metrics(&db);

        let rows = fetch_sessions(&db, &Bar::default(), "").expect("fetch sessions");
        let sids: Vec<String> = rows.iter().map(|r| r.sid.clone()).collect();
        assert_eq!(sids, vec!["run_7".to_string(), "conv_9".to_string(), "ses_cc".to_string()]);
        let cc = rows.iter().find(|r| r.sid == "ses_cc").expect("the claude row");
        assert_eq!(cc.service, "claude-code");
        assert_eq!(cc.events, "15", "the cumulative point is still an event; it just isn't summed");
        assert_eq!(cc.usd, "0.5", "the cumulative 99 never reaches the cost");
        assert_eq!(cc.spans, "3");
        assert_eq!(cc.usd, "0.5");
        assert_eq!(num_cell(&cc.tok_in), "40");
        assert_eq!(num_cell(&cc.tok_out), "7");
        assert!(!cc.last_seen.is_empty(), "last seen carries the union's max time");

        // the logs-page link's filter, fed back through the bar: one row.
        let only =
            fetch_sessions(&db, &Bar::parse("f=session.id%3Dconv_9"), "").expect("narrow");
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].sid, "conv_9");

        // an empty selection is an empty list, not an error.
        let none =
            fetch_sessions(&db, &Bar::parse("f=session.id%3Dnone"), "").expect("empty");
        assert!(none.is_empty());

        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn session_timeline_fetches_spans_and_events_for_every_agent_key() {
        let db = temp_db("ag-timeline");
        seed_logs(&db);
        seed_traces(&db);
        let ev = super::fetch_session_events(&db, &Bar::default(), "ses_cc", &super::tests::cfg(false));
        assert!(!ev.is_empty() && ev.iter().any(|e| e.name == "api_request"), "{ev:?}");
        assert!(ev.iter().all(|e| !e.bag.contains("a synthetic prompt")), "band is masked");
        let (cc, _) = crate::traces::fetch_session_waterfall(&db, "ses_cc").unwrap();
        assert!(!cc.is_empty() && cc.iter().all(|s| !s.trace_id.is_empty()), "spans carry their trace id");
        let (cx, _) = crate::traces::fetch_session_waterfall(&db, "conv_9").unwrap();
        assert!(!cx.is_empty(), "codex spans join by thread_id");
        assert!(crate::traces::fetch_session_waterfall(&db, "11").unwrap().0.is_empty(), "OS thread ids are not sessions");
        let _ = std::fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn tools_rows_join_names_outcomes_and_rejections() {
        let db = temp_db("ag-tools-page");
        seed_logs(&db);
        seed_traces(&db);
        seed_metrics(&db);
        let (cells, source) = super::fetch_tools(&db, &Bar::default(), "", &super::tests::cfg(false));
        assert_eq!(source, "spans");
        let bash = cells.iter().find(|c| c.tool == "bash").expect("bash row");
        assert_eq!((bash.calls.as_str(), bash.fails.as_str(), bash.fail_rate.as_str()), ("1", "1", "100%"));
        assert_eq!((bash.p50.as_str(), bash.rejected.as_str()), ("800 ms", "1"));
        assert_eq!(bash.traces_href, "/traces?f=tool_name%3Dbash");
        assert_eq!(bash.logs_href, "/logs?f=tool_name%3Dbash");
        assert_eq!(bash.fails_href, "/logs?f=tool_name%3Dbash&f=success%3Dfalse");
        assert_eq!(bash.rejected_href, "/logs?f=tool_name%3Dbash&f=decision%3Dreject");
        // the page's own range, service and filters ride along, never doubled
        let bar = Bar::parse("range=all&service=claude-code&f=tool_name%3Dbash");
        let (cells, _) = super::fetch_tools(&db, &bar, "claude-code", &super::tests::cfg(false));
        assert_eq!(cells[0].fails_href, "/logs?range=all&service=claude-code&f=tool_name%3Dbash&f=success%3Dfalse");

        // no tool spans: the Codex counters answer, without latency
        let (cells, source) = super::fetch_tools(&db, &Bar::default(), "codex_exec", &super::tests::cfg(false));
        assert_eq!(source, "spans", "codex.tool.call spans exist in the fixture");
        assert!(cells.iter().any(|c| c.tool == "apply_patch"));
        let none = Bar::parse("f=tool_name%3Dnot-a-tool");
        let (cells, source) = super::fetch_tools(&db, &none, "", &super::tests::cfg(false));
        assert_eq!(source, "metrics", "an empty spans branch falls back to the counters");
        assert!(cells.is_empty(), "…which the same filter narrows too");
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn a_narrow_time_range_narrows_the_agent_views() {
        // the fixtures sit on 2026-11-17: an hour-wide window ending now
        // holds none of them, the default (all) holds all of them
        let db = temp_db("ag-range");
        seed_logs(&db);
        seed_traces(&db);
        seed_metrics(&db);
        let hour = Bar::parse("range=1h");
        assert!(fetch_sessions(&db, &hour, "").unwrap().is_empty());
        assert!(super::fetch_spend(&db, &hour, "").unwrap().is_empty());
        let (sql, params) = tool_health_query(&hour, "");
        assert!(rows_of(sql, params, &db).is_empty());
        assert!(!fetch_sessions(&db, &Bar::default(), "").unwrap().is_empty());
        assert!(!super::fetch_spend(&db, &Bar::default(), "").unwrap().is_empty());
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn spend_uses_metrics_and_falls_back_to_events_per_day() {
        // metrics + events for the same day: the metric rows win, no badge
        let db = temp_db("ag-spend-merge");
        seed_logs(&db);
        seed_metrics(&db);
        let rows = super::fetch_spend(&db, &Bar::default(), "").unwrap();
        assert!(rows.iter().all(|r| !r.from_events), "{rows:?}");
        assert_eq!(rows[0].usd, "0.5", "cumulative points never summed");
        let t = super::spend_totals(&rows);
        assert_eq!((t[0], t[1], t[2], t[3], t[4]), (0.5, 40.0, 7.0, 900.0, 120.0));
        let _ = fs::remove_dir_all(&db.data_dir);

        // logs only: the day comes from the api_request events, badged
        let db = temp_db("ag-spend-events");
        seed_logs(&db);
        let rows = super::fetch_spend(&db, &Bar::default(), "");
        // no metrics table at all is an error for the primary branch: the
        // page shows nothing rather than a half-truth from a broken query
        assert!(rows.is_err());
        db.with_conn(|c| c.execute_batch(
            "CREATE TABLE otel.main.otlp_metrics_sum(time_unix_nano TIMESTAMP, \"name\" VARCHAR,
               double_value DOUBLE, service_name VARCHAR, resource_attributes VARCHAR,
               metric_attributes VARCHAR, aggregation_temporality INTEGER)
             WITH (time_col = 'time_unix_nano', ttl = '30 days', max_bytes = '64MB');",
        ).map_err(Into::into)).unwrap();
        let rows = super::fetch_spend(&db, &Bar::default(), "").unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].from_events && rows[0].usd == "0.25" && rows[0].model == "m-large", "{rows:?}");
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn session_prefilter_never_changes_what_matches() {
        let db = temp_db("ag-prefilter");
        seed_logs(&db);
        // ids a JSON encoder escapes (a quote, a slash written as \/) skip
        // the substring guard; the parse alone must still find them
        db.with_conn(|conn| conn.execute_batch(
            r#"INSERT INTO otel.main.otlp_logs
                   (time_unix_nano, event_name, service_name, body, resource_attributes, log_attributes)
               VALUES
                   ('2026-11-17 11:00:00', '', 'x', '', '{}', '{"session.id":"we\"ird"}'),
                   ('2026-11-17 11:00:01', '', 'x', '', '{}', '{"session.id":"a\/b"}'),
                   ('2026-11-17 11:00:02', '', 'opencode', '', '{"opencode.run":"run.7:x"}', '{}');"#,
        ).map_err(Into::into)).unwrap();
        for (sid, want) in [("we\"ird", 1), ("a/b", 1), ("run.7:x", 1), ("ses_cc", 6), ("ses", 0)] {
            let (sql, params) = session_events_query(&Bar::default(), sid);
            assert_eq!(rows_of(sql, params, &db).len(), want, "session {sid:?}");
        }
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn session_events_query_names_events_from_the_attribute_first() {
        let db = temp_db("ag-events");
        seed_logs(&db);
        seed_traces(&db);
        seed_metrics(&db);

        let (sql, params) = session_events_query(&Bar::default(), "ses_cc");
        let rows = rows_of(sql, params, &db);
        let names: Vec<&str> = rows.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(
            names,
            vec![
                "user_prompt",
                "tool_result",
                "api_request",
                "retention_sweep",
                "tool_decision",
                "tool_decision"
            ],
            "the attribute names the rows, and the OTLP column covers the one row without it"
        );

        // the Codex row is named by its attribute, not by the Rust source
        // location its OTLP `event_name` column holds (§2 diff 1)
        let (sql, params) = session_events_query(&Bar::default(), "conv_9");
        let rows = rows_of(sql, params, &db);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], "codex.user_prompt");
        assert!(!rows[0][0].contains("session_telemetry.rs"));

        // the event band reaches the timeline through the same redactor the
        // pages will run every cell through (§5): the stored prompt is there,
        // the rendered cell is not
        let (sql, params) = session_events_query(&Bar::default(), "ses_cc");
        let attrs = rows_of(sql, params, &db)[0][2].clone();
        assert!(
            attrs.contains("a synthetic prompt"),
            "stored as sent: {attrs}"
        );
        assert!(
            !super::redact_cell(&attrs).contains("a synthetic prompt"),
            "the redactor drops it: {attrs}"
        );

        // the bar narrows the band without touching the session predicate
        let (sql, params) = session_events_query(&Bar::parse("f=tool_name%3Dbash"), "ses_cc");
        let rows = rows_of(sql, params, &db);
        assert_eq!(rows.len(), 2, "tool_result and the reject tool_decision");
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn spend_queries_total_by_day_model_and_developer() {
        let db = temp_db("ag-spend");
        seed_logs(&db);
        seed_traces(&db);
        seed_metrics(&db);

        // day, model, dev, usd, tok_in, tok_out, tok_cache, active_s
        let (sql, params) = spend_metrics_query(&Bar::default(), "");
        let rows = rows_of(sql, params, &db);
        assert_eq!(
            rows,
            vec![
                // the day with a cost first (`usd DESC NULLS LAST`), then the
                // active-time row, which carries no model
                vec![
                    "2026-11-17 00:00:00".to_string(),
                    "m-large".to_string(),
                    "dev-1".to_string(),
                    "0.5".to_string(),
                    "40.0".to_string(),
                    "7.0".to_string(),
                    "900.0".to_string(),
                    String::new(),
                ],
                vec![
                    "2026-11-17 00:00:00".to_string(),
                    String::new(),
                    "dev-1".to_string(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    "120.0".to_string(),
                ],
            ],
            "the metric branch groups by day, model and developer"
        );

        // the fallback branch reads the same shape off the api_request events
        let (sql, params) = spend_events_query(&Bar::default(), "");
        let rows = rows_of(sql, params, &db);
        assert_eq!(
            rows,
            vec![vec![
                "2026-11-17 00:00:00".to_string(),
                "m-large".to_string(),
                "dev-1".to_string(),
                "0.25".to_string(),
                "40.0".to_string(),
                "7.0".to_string(),
            ]],
            "a logs-only capture still shows cost (§3.3)"
        );
        let _ = fs::remove_dir_all(&db.data_dir);
    }

    #[test]
    fn tool_health_queries_count_calls_failures_and_latency() {
        let db = temp_db("ag-tools");
        seed_logs(&db);
        seed_traces(&db);
        seed_metrics(&db);

        // tool, calls, fails, p50, p95, max
        let (sql, params) = tool_health_query(&Bar::default(), "");
        let rows = rows_of(sql, params, &db);
        let apply = rows
            .iter()
            .find(|r| r[0] == "apply_patch")
            .unwrap_or_else(|| panic!("the tool row is missing: {rows:?}"));
        assert_eq!(
            &apply[..6],
            [
                "apply_patch".to_string(),
                "1".to_string(),
                "1".to_string(),
                "200.0".to_string(),
                "200.0".to_string(),
                "200.0".to_string(),
            ],
            "count, failure count and the percentiles, in ms"
        );
        // Claude Code: the name from `claude_code.tool`, the outcome and the
        // execution time (800 ms, not the 1 s span that includes the wait
        // for approval) from its `claude_code.tool.execution` partner
        let bash = rows
            .iter()
            .find(|r| r[0] == "bash")
            .unwrap_or_else(|| panic!("the joined bash row is missing: {rows:?}"));
        assert_eq!(
            &bash[..6],
            ["bash", "1", "1", "800.0", "800.0", "800.0"].map(String::from),
            "one call, failed per its execution span, timed by it"
        );
        assert!(rows.iter().all(|r| !r[0].is_empty()), "no nameless execution rows: {rows:?}");

        // the Codex counter fallback, when no tool spans arrived
        let (sql, params) = tool_metrics_query(&Bar::default(), "");
        let rows = rows_of(sql, params, &db);
        assert_eq!(
            rows,
            vec![vec![
                "apply_patch".to_string(),
                "3.0".to_string(),
                "3.0".to_string(),
            ]],
            "counter values are summed, so fails and calls are the same units"
        );

        // and the rejections block
        let (sql, params) = tool_decisions_query(&Bar::default(), "");
        let rows = rows_of(sql, params, &db);
        assert_eq!(
            rows,
            vec![vec!["bash".to_string(), "1".to_string()]],
            "only decision=reject counts, per tool"
        );

        // the bar narrows the spans branch exactly like the traces page does
        let (sql, params) = tool_health_query(&Bar::parse("f=tool_name%3Dnope"), "");
        assert!(rows_of(sql, params, &db).is_empty());
        let _ = fs::remove_dir_all(&db.data_dir);
    }
}
