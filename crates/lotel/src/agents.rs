//! The coding-agent views' query layer: Claude Code, OpenAI Codex and opencode
//! telemetry read out of the six signal tables (`docs/M-agents-plan.md` §2 is
//! what the store really holds, §3 what each view reads).
//!
//! Data model only — SQL builders plus the shared render redactor (§5). The
//! pages and routes (T3–T7) call these and own the rendering.
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

use crate::filters::{AttrCols, Bar};

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
/// row), then Codex's `conversation.id` on logs and `thread.id`/`turn.id` on
/// spans (§2, the owner's capture), then opencode's `opencode.run`.
///
/// `opencode.run` is a *resource* attribute and the only per-run key opencode
/// has (open question 2 ✅), so it comes last: a row carrying a real session
/// id groups by that, and one carrying only a run id groups by the run.
pub const SESSION_KEYS: &[&str] = &[
    "session.id",
    "gen_ai.conversation.id",
    "conversation.id",
    "thread.id",
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
pub const TOOL_NAME_KEYS: &[&str] = &["tool_name", "tool_name_safe", "tool"];

/// The success key: the strings `"true"`/`"false"` on events, a boolean on
/// spans (§2 diff 2).
pub const SUCCESS_KEY: &str = "success";

/// The metric names `/spend` reads. The Claude Code names are measured (§2);
/// `codex.turn.token_usage` is INFERRED — the auth-401 capture emitted no
/// metrics at all, so it must be pinned against the logged-in Codex run before
/// T5's Codex column is trusted (open questions 3 and 7).
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
pub const TOOL_SPAN_NAMES: &[&str] = &["claude_code.tool.execution", "codex.tool.call"];
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
pub fn session_expr(col: &str) -> String {
    let parts: Vec<String> = SESSION_KEYS.iter().map(|_| extract(col)).collect();
    format!("COALESCE({})", parts.join(", "))
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
/// agent's key: one `= ?` condition per (column, key) — every [`SESSION_KEYS`]
/// spelling in `cols`' row and resource bags — to be joined with `OR` by the
/// caller. That is what makes one `f=session.id=<sid>` link match Claude Code
/// (row bag), Codex (row bag) and opencode (resource bag) rather than only the
/// agent whose spelling the page happened to guess.
///
/// The body bag is not searched: no agent keeps a session id in a log body.
/// Bind [`session_attr_params`], which returns the path then the id for each
/// condition, matching the `?` order of the returned text.
pub fn session_attr_cols(cols: &AttrCols, alias: Option<&str>) -> Vec<String> {
    [cols.row, cols.resource]
        .iter()
        .flat_map(|col| {
            let col = extract(&qualify(col, alias));
            SESSION_KEYS.iter().map(move |_| format!("({col} = ?)"))
        })
        .collect()
}

/// The `?` parameters for [`session_attr_cols`]: the key path followed by the
/// session id, per condition.
pub fn session_attr_params(cols: &AttrCols, sid: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(SESSION_KEYS.len() * 4);
    for _ in [cols.row, cols.resource] {
        for key in SESSION_KEYS {
            out.push(attr_path(key));
            out.push(sid.to_string());
        }
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
            .bar(bar, b.cols, Some(b.alias));
        sql.where_all(&w);
    }
    sql.push(
        "), cost AS (SELECT json_extract_string(metric_attributes, ?) AS sid, \
         sum(double_value) AS usd FROM otel.main.otlp_metrics_sum WHERE (\"name\" = ?) GROUP BY 1)\
         , tok AS (SELECT json_extract_string(metric_attributes, ?) AS sid, \
         sum(double_value) FILTER (WHERE (json_extract_string(metric_attributes, ?) = ?)) AS tin, \
         sum(double_value) FILTER (WHERE (json_extract_string(metric_attributes, ?) = ?)) AS tout \
         FROM otel.main.otlp_metrics_sum WHERE (\"name\" = ?) GROUP BY 1) \
         SELECT ev.sid AS sid, any_value(ev.service_name) AS svc, count(*) AS events, \
         count(*) FILTER (WHERE ev.sig = 'spans') AS spans, min(ev.t) AS first_seen, \
         max(ev.t) AS last_seen, any_value(c.usd) AS usd, any_value(t.tin) AS tok_in, \
         any_value(t.tout) AS tok_out FROM ev LEFT JOIN cost c USING (sid) \
         LEFT JOIN tok t USING (sid) WHERE (ev.sid IS NOT NULL) \
         GROUP BY ev.sid ORDER BY last_seen DESC LIMIT CAST(? AS BIGINT)",
    );
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
            " AS ev, lg.time_unix_nano AS t, COALESCE(lg.log_attributes, '{}') AS attrs \
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
    sql.finish()
}

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
        .bar(bar, &LOG_COLS, Some("lg"));
    sql.where_all(&w)
        .push(" GROUP BY 1, 2, 3 ORDER BY d DESC, usd DESC NULLS LAST");
    sql.finish()
}

/// `/tools` (§3.4): calls, failures and latency percentiles per tool, from the
/// tool spans. See [`TOOL_SPAN_NAMES`] on the Codex name.
pub fn tool_health_query(bar: &Bar, service: &str) -> (String, Vec<String>) {
    let mut sql = Sql::new();
    sql.push("SELECT ")
        .expr(&tool_name_expr("span_attributes"), &tool_name_params())
        .push(" AS tool, count(*) AS calls, count(*) FILTER (WHERE ")
        .expr(&success_expr("span_attributes"), &success_params())
        .push(
            " <> 'true') AS fails, \
         quantile_cont(duration_time_unix_nano / 1e6, 0.5) AS p50_ms, \
         quantile_cont(duration_time_unix_nano / 1e6, 0.95) AS p95_ms, \
         max(duration_time_unix_nano) / 1e6 AS max_ms FROM otel.main.otlp_traces",
        );
    let mut w = Where::new();
    w.service(service, None)
        .in_list("\"name\"", TOOL_SPAN_NAMES)
        .bar(bar, &TRACE_COLS, None);
    sql.where_all(&w).push(" GROUP BY 1 ORDER BY calls DESC");
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
        .bar(bar, &LOG_COLS, Some("lg"));
    sql.where_all(&w).push(" GROUP BY 1 ORDER BY 2 DESC");
    sql.finish()
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
        assert!(sql.starts_with("COALESCE("));
        assert_eq!(
            sql.matches("json_extract_string").count(),
            SESSION_KEYS.len()
        );
        assert_eq!(
            sql.matches("json_valid(log_attributes)").count(),
            SESSION_KEYS.len()
        );
        // Claude Code's own id first, the captured Codex keys next, opencode's
        // run id last, so a row carrying several groups by the real id
        assert_eq!(SESSION_KEYS[0], "session.id");
        assert_eq!(SESSION_KEYS[1], "gen_ai.conversation.id");
        for key in [
            "conversation.id",
            "thread.id",
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
        assert!(sql.starts_with("COALESCE(COALESCE("));
        assert!(sql.contains("mt.metric_attributes") && sql.contains("mt.resource_attributes"));
        assert_eq!(params.len(), SESSION_KEYS.len() * 2);
        assert_eq!(sql.matches('?').count(), params.len());
    }

    #[test]
    fn session_attr_conds_or_every_key_in_both_bags() {
        let conds = session_attr_cols(&LOG_COLS, Some("lg"));
        let params = session_attr_params(&LOG_COLS, "ses_1");
        assert_eq!(conds.len(), SESSION_KEYS.len() * 2);
        assert_eq!(params.len(), conds.len() * 2);
        let sql = conds.join(" OR ");
        assert_eq!(sql.matches('?').count(), params.len());
        assert!(conds.iter().all(|c| c.ends_with("= ?)")), "{:?}", conds[0]);
        assert!(sql.contains("lg.log_attributes") && sql.contains("lg.resource_attributes"));
        assert!(
            !sql.contains("body"),
            "no agent keeps a session id in a body"
        );
        // path then id, per condition
        assert_eq!(params[0], SESSION_ID_PATH);
        assert_eq!(params[1], "ses_1");
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
        assert!(sql.ends_with("ORDER BY lg.time_unix_nano"));
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
            &["claude_code.tool.execution", "codex.tool.call", "bash"],
        );
        assert!(sql.contains("quantile_cont(duration_time_unix_nano / 1e6, 0.5) AS p50_ms"));
        assert!(sql.contains("quantile_cont(duration_time_unix_nano / 1e6, 0.95) AS p95_ms"));
        assert!(sql.contains("GROUP BY 1 ORDER BY calls DESC"));
        assert!(params.contains(&TOOL_SPAN_NAMES[0].to_string()));

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
}

// ————————————————————————————— DB fixtures ————————————————————————————————
// The agent views' promise in executable form: the SQL the builders emit runs
// against the real store and groups the three agents' rows by the key each of
// them actually carries (§2). The rows are synthetic, shaped like the measured
// capture — no captured prompt text, tool content or paths is in here.

#[cfg(test)]
pub(crate) mod db_tests {
    use super::{
        session_events_query, sessions_query, spend_events_query, spend_metrics_query,
        tool_decisions_query, tool_health_query, tool_metrics_query,
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
    /// `thread.id`, and opencode's spans keyed only by the resource run id,
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
                     '{\"service.name\":\"claude-code\"}', '{\"session.id\":\"ses_cc\",\"tool_name\":\"bash\"}'),
                    ('2026-11-17 10:00:01', 800000000, 't1', 'sp3', 'sp2',
                     'claude-code', 'claude_code.tool.execution', 1, 0, '',
                     '{\"service.name\":\"claude-code\"}', '{\"session.id\":\"ses_cc\",\"success\":false}'),
                    ('2026-11-17 10:00:02', 200000000, 't2', 'sp5', '',
                     'codex_exec', 'codex.tool.call', 1, 0, '',
                     '{\"service.name\":\"codex_exec\"}', '{\"thread.id\":\"conv_9\",\"tool_name\":\"apply_patch\",\"success\":\"false\"}'),
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
                    service_name VARCHAR, resource_attributes VARCHAR, metric_attributes VARCHAR)
                 WITH (time_col = 'time_unix_nano', ttl = '30 days', max_bytes = '64MB');
                 INSERT INTO otel.main.otlp_metrics_sum
                     (time_unix_nano, \"name\", double_value, service_name, resource_attributes, metric_attributes)
                 VALUES
                    ('2026-11-17 10:00:00', 'claude_code.cost.usage', 0.5, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\"}'),
                    ('2026-11-17 10:00:00', 'claude_code.token.usage', 40, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\",\"type\":\"input\"}'),
                    ('2026-11-17 10:00:00', 'claude_code.token.usage', 7, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\",\"type\":\"output\"}'),
                    ('2026-11-17 10:00:00', 'claude_code.token.usage', 900, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"model\":\"m-large\",\"user.id\":\"dev-1\",\"type\":\"cacheRead\"}'),
                    ('2026-11-17 10:00:00', 'claude_code.active_time.total', 120, 'claude-code',
                     '{}', '{\"session.id\":\"ses_cc\",\"user.id\":\"dev-1\",\"type\":\"user\"}'),
                    ('2026-11-17 10:00:00', 'codex.tool.call', 3, 'codex_exec',
                     '{}', '{\"tool\":\"apply_patch\",\"success\":\"false\"}'),
                    ('2026-11-17 10:00:00', 'some.other.metric', 1, 'claude-code',
                     '{}', '{}');",
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
                    "14".to_string(),
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
        // §3.4 measured: `tool_name` lives on `claude_code.tool` while
        // `success` lives on `claude_code.tool.execution`, so the execution
        // span the plan's name filter selects groups under an empty tool name.
        // Reported rather than worked around; see the task notes.
        let nameless = rows
            .iter()
            .find(|r| r[0].is_empty())
            .expect("claude_code.tool.execution carries no tool_name");
        assert_eq!((nameless[1].as_str(), nameless[2].as_str()), ("1", "1"));

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
