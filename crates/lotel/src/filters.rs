//! The one filter bar shared by the logs and traces pages.
//!
//! Model: time range, service, free text, and attribute filters written
//! `key=value`, `key!=value` or `key~substring`. Several filters AND
//! together. The key prefix picks the column: a bare key reads the row's
//! own attribute bag (`log_attributes` on logs, `span_attributes` on
//! traces), `resource.` reads `resource_attributes`, and `body.` reads the
//! parsed body (J1): a JSON object body directly, or the JSON payload
//! strings inside an `[message, "<payload>"]` array body.
//!
//! `!=` and `~` only ever consider rows that carry the key: a missing key
//! extracts NULL and the row is skipped — the filter narrows, it never
//! widens (see [`Bar::attr_conds`]). In `~` the value *is* the LIKE pattern,
//! so `%` and `_` act as wildcards. The Explore builder names the same
//! columns `@col:key`: its `@log_attributes:k` / `@span_attributes:k` are
//! the bar's bare `k`, and `@resource_attributes:k` is the bar's `resource.k`.
//!
//! The page fields that stay first-class (severity on logs; span name,
//! status, min duration on traces) are plain query params the pages read
//! through [`Bar::get`] — the bar owns their URL round-tripping, the pages
//! own their SQL.
//!
//! A line that fails to parse is never a 500: it is skipped by the SQL and
//! surfaced next to the input instead (see [`Bar::attr_errors`]).
//!
//! Safety model (J0): keys and values never reach the SQL text. The JSON
//! path and the value both bind as `?` parameters, one `?` per path
//! occurrence — see [`Bar::attr_conds`] for the exact `?` order.

use topcoat::{
    Result,
    context::Cx,
    view::{View, component, view},
};

/// Which VARCHAR column an attribute filter reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrSource {
    /// the row's own bag: `log_attributes` / `span_attributes`
    Row,
    /// `resource_attributes`
    Resource,
    /// the parsed `body` (logs only — traces skip these)
    Body,
}

/// How a filter compares the extracted value to what the user typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrOp {
    Eq,
    Ne,
    Has,
}

/// One parsed `key=value` / `key!=value` / `key~substring` line. A failed
/// line keeps its error instead of poisoning the whole bar.
#[derive(Debug, Clone)]
pub struct AttrFilter {
    pub source: AttrSource,
    pub key: String,
    pub op: AttrOp,
    pub value: String,
    /// what the user typed, for the error message
    pub raw: String,
    pub error: Option<String>,
}

impl AttrFilter {
    fn err(raw: &str, msg: &str) -> Self {
        Self {
            source: AttrSource::Row,
            key: String::new(),
            op: AttrOp::Eq,
            value: String::new(),
            raw: raw.to_string(),
            error: Some(msg.to_string()),
        }
    }
}

/// The canonical URL form of one filter: the source prefix re-added and the
/// operator spelled out, so a re-parse yields the same filter. Values that
/// could be mistaken for quoting (or are empty, which only exists quoted)
/// stay quoted. Shared with the logs page's chips.
pub fn canonical_of(source: AttrSource, op: AttrOp, key: &str, value: &str) -> String {
    let prefix = match source {
        AttrSource::Row => "",
        AttrSource::Resource => "resource.",
        AttrSource::Body => "body.",
    };
    let op = match op {
        AttrOp::Eq => "=",
        AttrOp::Ne => "!=",
        AttrOp::Has => "~",
    };
    let value = if value.is_empty()
        || value.starts_with(['"', '\''])
        || value.ends_with(['"', '\''])
    {
        format!("\"{value}\"")
    } else {
        value.to_string()
    };
    format!("{prefix}{key}{op}{value}")
}

/// Parse one `key=value` / `key!=value` / `key~substring` line. The operator
/// is the earliest of `!=`, `~`, `=` — so `a=b!=c` is an equality on
/// `a` with value `b!=c`, and `a!=b=c` a negation with value `b=c`.
/// A matching pair of surrounding quotes keeps spaces (and any operator
/// character) in the value verbatim.
pub fn parse_filter(raw: &str) -> AttrFilter {
    let line = raw.trim();
    // candidate operators at their first position; ties keep insertion
    // order, so `!=` (found at the same index as its trailing `=`) wins.
    let mut cands: Vec<(usize, AttrOp, usize)> = Vec::new();
    if let Some(p) = line.find("!=") {
        cands.push((p, AttrOp::Ne, 2));
    }
    if let Some(p) = line.find('~') {
        cands.push((p, AttrOp::Has, 1));
    }
    if let Some(p) = line.find('=') {
        cands.push((p, AttrOp::Eq, 1));
    }
    if cands.is_empty() {
        return AttrFilter::err(
            raw,
            "expected key=value, key!=value or key~substring",
        );
    }
    cands.sort_by_key(|(p, _, _)| *p);
    let (at, op, width) = cands[0];
    let key_raw = line[..at].trim();
    let (source, key) = if let Some(k) = key_raw.strip_prefix("resource.") {
        (AttrSource::Resource, k.trim())
    } else if let Some(k) = key_raw.strip_prefix("body.") {
        (AttrSource::Body, k.trim())
    } else {
        (AttrSource::Row, key_raw)
    };
    if key.is_empty() {
        return AttrFilter::err(raw, "attribute key missing");
    }
    let mut value = line[at + width..].trim();
    // a balanced pair of surrounding quotes is quoting, not content
    let quoted = value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')));
    if quoted {
        value = &value[1..value.len() - 1];
    }
    if value.is_empty() && !quoted {
        return AttrFilter::err(raw, "no value to match");
    }
    AttrFilter {
        source,
        key: key.to_string(),
        op,
        value: value.to_string(),
        raw: raw.to_string(),
        error: None,
    }
}

/// Parse a whole attribute box: one filter per line (the textarea joins
/// them; browsers send `\r\n`). Empty lines are ignored.
pub fn parse_filter_list(raw: &str) -> Vec<AttrFilter> {
    raw.split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .map(parse_filter)
        .collect()
}

/// The JSON member path for a key, as a bound parameter (never SQL text).
/// The `\"` escaping is DuckDB's JSON-path string syntax — the path is data
/// here, and quoting the whole key means dotted keys address one member.
fn json_path(key: &str) -> String {
    format!("$.\"{}\"", key.replace('"', "\\\""))
}

/// Which table columns the three attribute sources read. `body: None` for
/// tables without a JSON body (traces): body filters are skipped there.
pub struct AttrCols {
    pub row: &'static str,
    pub resource: &'static str,
    pub body: Option<&'static str>,
}

/// Logs page: bare keys read `log_attributes`.
pub const LOG_COLS: AttrCols = AttrCols {
    row: "log_attributes",
    resource: "resource_attributes",
    body: Some("body"),
};

/// Traces page: bare keys read `span_attributes`; bodies are not queryable.
pub const TRACE_COLS: AttrCols = AttrCols {
    row: "span_attributes",
    resource: "resource_attributes",
    body: None,
};

/// The parsed query string of a page, shared by logs and traces (and the
/// live-tail fragments, which must filter exactly like the page they feed).
#[derive(Debug, Default, Clone)]
pub struct Bar {
    /// every key=value pair, decoded, in URL order — the bar keeps the full
    /// query so links (facets, chips) keep every filter they did not touch
    pairs: Vec<(String, String)>,
    attrs: Vec<AttrFilter>,
}

/// Seconds for a recognised time-range label; None for `all`, unset or junk
/// (which all cutoff to the epoch).
fn range_secs(range: &str) -> Option<i64> {
    match range {
        "15m" => Some(900),
        "1h" => Some(3_600),
        "6h" => Some(6 * 3_600),
        "24h" => Some(24 * 3_600),
        "7d" => Some(7 * 24 * 3_600),
        "30d" => Some(30 * 24 * 3_600),
        _ => None,
    }
}

impl Bar {
    /// Parse a raw query string (`a=1&f=k%3Dv&f=x`). Repeated `f` keys stack
    /// (one attribute filter each); topcoat's serde query params cannot
    /// collect repeats, so the bar decodes the query itself.
    pub fn parse(query: &str) -> Self {
        let mut pairs = Vec::new();
        let mut attrs = Vec::new();
        for part in query.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = match part.split_once('=') {
                Some((k, v)) => (percent_decode(k), percent_decode(v)),
                None => (percent_decode(part), String::new()),
            };
            if k == "f" {
                attrs.extend(parse_filter_list(&v));
            }
            pairs.push((k, v));
        }
        Self { pairs, attrs }
    }

    /// First value of `key`, or `""` when absent/blank.
    pub fn get(&self, key: &str) -> &str {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or("")
    }

    /// Every value of a repeated `key`, in URL order.
    pub fn all(&self, key: &str) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The parsed attribute filters, errors included.
    pub fn attrs(&self) -> &[AttrFilter] {
        &self.attrs
    }

    /// One line per unparsable attribute filter, for the box beneath the input.
    pub fn attr_errors(&self) -> Vec<String> {
        self.attrs
            .iter()
            .filter_map(|f| f.error.as_ref().map(|e| format!("{} — {e}", f.raw.trim())))
            .collect()
    }

    /// The attribute box echo: every `f` value, one per line.
    pub fn raw_f(&self) -> String {
        self.all("f").join("\n")
    }

    /// Lower cutoff timestamp for the selected range; everything at/after it
    /// matches, epoch for `all`/junk — the same contract the old LogFilters
    /// had, widened to the full set `stats::cutoff_for` knows so the feed
    /// and the latency trends never disagree about one label.
    pub fn cutoff(&self) -> String {
        let secs = range_secs(self.get("range"));
        let cutoff = match secs {
            Some(s) => chrono::Utc::now() - chrono::Duration::seconds(s),
            None => chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).unwrap(),
        };
        cutoff.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string()
    }

    /// True when a time range narrower than `all` is set — the signal that
    /// an empty list may just be the window (see [`empty_range_hint`]).
    pub fn range_is_narrow(&self) -> bool {
        range_secs(self.get("range")).is_some()
    }

    /// Validated row cap for the page feed.
    pub fn limit(&self) -> usize {
        self.get("limit").parse::<usize>().unwrap_or(200).clamp(1, 2000)
    }

    /// Percent-encode one query-string component (the unreserved set passes).
    pub(crate) fn enc(s: &str) -> String {
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

    /// Link to `path` with `set` replacing (or adding) params, keeping every
    /// other filter. Blank values mean "off" and are dropped.
    pub fn href(&self, path: &str, set: &[(&str, &str)]) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut replaced: Vec<&str> = Vec::new();
        for (k, v) in &self.pairs {
            if v.is_empty() || replaced.iter().any(|r| *r == k) {
                continue; // blanks and repeat echoes never ride along
            }
            if let Some((_, sv)) = set.iter().find(|(sk, _)| *sk == k) {
                replaced.push(k);
                if sv.is_empty() {
                    continue;
                }
                parts.push(format!("{k}={}", Self::enc(sv)));
            } else {
                parts.push(format!("{k}={}", Self::enc(v)));
            }
        }
        for (k, v) in set {
            if v.is_empty() || replaced.iter().any(|r| r == k) {
                continue;
            }
            parts.push(format!("{k}={}", Self::enc(v)));
        }
        if parts.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{}", parts.join("&"))
        }
    }

    /// Link to `path` with `drop`ped params removed, keeping the rest.
    pub fn href_without(&self, path: &str, drop: &[&str]) -> String {
        let parts: Vec<String> = self
            .pairs
            .iter()
            .filter(|(k, v)| !drop.iter().any(|d| d == k) && !v.is_empty())
            .map(|(k, v)| format!("{k}={}", Self::enc(v)))
            .collect();
        if parts.is_empty() {
            path.to_string()
        } else {
            format!("{path}?{}", parts.join("&"))
        }
    }

    /// Link that toggles `param` to `value` (or off when already `on`),
    /// keeping the other active filters — the facet links.
    pub fn href_toggling(&self, path: &str, param: &str, value: &str, on: bool) -> String {
        if on {
            self.href_without(path, &[param])
        } else {
            self.href(path, &[(param, value)])
        }
    }

    /// Link that adds a `key=value` attribute filter (an attribute chip):
    /// every current filter is kept, and `f` stacks rather than replaces.
    /// Clicking a chip whose filter is already active re-sends the same URL.
    pub fn href_add_filter(&self, path: &str, canonical: &str) -> String {
        if self.all("f").contains(&canonical) {
            return self.href(path, &[]);
        }
        let mut parts: Vec<String> = self
            .pairs
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, v)| format!("{k}={}", Self::enc(v)))
            .collect();
        parts.push(format!("f={}", Self::enc(canonical)));
        format!("{path}?{}", parts.join("&"))
    }

    /// The AND-joined SQL conditions for the parsed attribute filters and
    /// their bound parameters, in order of `?` appearance. Unparsable
    /// filters are skipped (they show next to the input); `body` filters are
    /// skipped when the table has no body column.
    ///
    /// Every read guards with `json_valid` — the store keeps VARCHAR, so a
    /// body or bag that is plain text must not reach `json_extract_string`.
    /// `=`/`!=` compare the extracted value; `~` is a case-insensitive
    /// substring whose `%` and `_` act as LIKE wildcards (the value is the
    /// pattern). A missing key extracts NULL, and `NULL = ?`, `NULL <> ?`
    /// and `NULL ILIKE ?` all match nothing: every operator skips rows that
    /// lack the key, so the filter narrows, it never widens.
    pub fn attr_conds(&self, cols: &AttrCols) -> (Vec<String>, Vec<String>) {
        self.attr_conds_aliased(cols, None)
    }

    /// As [`Bar::attr_conds`], with every column qualified by a table alias
    /// (the BM25 panel semijoins the store under one).
    pub fn attr_conds_aliased(
        &self,
        cols: &AttrCols,
        alias: Option<&str>,
    ) -> (Vec<String>, Vec<String>) {
        let qualify = |col: &str| match alias {
            Some(a) => format!("{a}.{col}"),
            None => col.to_string(),
        };
        let mut conds = Vec::new();
        let mut params = Vec::new();
        for f in self.attrs() {
            if f.error.is_some() {
                continue;
            }
            let col = qualify(match f.source {
                AttrSource::Row => cols.row,
                AttrSource::Resource => cols.resource,
                AttrSource::Body => match cols.body {
                    Some(b) => b,
                    None => continue,
                },
            });
            let col = col.as_str();
            let path = json_path(&f.key);
            let cmp = match f.op {
                AttrOp::Eq => "= ?",
                AttrOp::Ne => "<> ?",
                AttrOp::Has => "ILIKE '%' || ? || '%'",
            };
            if f.source == AttrSource::Body {
                // The body as stored, plus — for `[message, "<payload>"]`
                // arrays — each string element decoded exactly one level,
                // the same fields J1's chips read. `?` order: path, path, value.
                conds.push(format!(
                    "(COALESCE(\
                       (CASE WHEN json_valid({col}) THEN json_extract_string({col}, ?) END),\
                       (SELECT json_extract_string(i, ?) FROM (\
                          SELECT json_extract_string(e, '$') AS i \
                          FROM UNNEST(CAST(json_extract(CASE WHEN json_valid({col}) THEN {col} END, '$[*]') AS VARCHAR[])) AS t(e)\
                        ) WHERE json_valid(i) LIMIT 1)\
                     ) {cmp})"
                ));
                params.push(path.clone());
                params.push(path);
            } else {
                conds.push(format!(
                    "((CASE WHEN json_valid({col}) THEN json_extract_string({col}, ?) END) {cmp})"
                ));
                params.push(path);
            }
            params.push(f.value.clone());
        }
        (conds, params)
    }
}

/// Percent-decode one query-string component (`+` is a space, per
/// `application/x-www-form-urlencoded`; stray `%` stays literal).
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The shared Time range select.
#[component]
pub(crate) async fn range_field(cx: &Cx, current: String) -> Result<impl View> {
    Ok(view! {
        <label>"Time range"
            <select name="range">
                <option value="1h" if current == "1h" { selected="selected" }>"1 hour"</option>
                <option value="6h" if current == "6h" { selected="selected" }>"6 hours"</option>
                <option value="24h" if current == "24h" { selected="selected" }>"24 hours"</option>
                <option value="7d" if current == "7d" { selected="selected" }>"7 days"</option>
                <option value="all" if !["1h", "6h", "24h", "7d"].contains(&current.as_str()) { selected="selected" }>"all"</option>
            </select>
        </label>
    })
}

/// The shared Service select.
#[component]
pub(crate) async fn service_field(cx: &Cx, options: Vec<String>, current: String) -> Result<impl View> {
    Ok(view! {
        <label>"Service"
            <select name="service">
                <option value="" if current.is_empty() { selected="selected" }>"all"</option>
                for s in &options {
                    <option value=(s.clone()) if *s == current { selected="selected" }>(s.clone())</option>
                }
            </select>
        </label>
    })
}

/// The shared attribute box: one `key=value` / `key!=value` / `key~text`
/// filter per line, with parse errors inline beneath it (never a 500).
#[component]
pub(crate) async fn attr_field(cx: &Cx, raw: String, errors: Vec<String>) -> Result<impl View> {
    Ok(view! {
        <label>"Attributes"
            <textarea name="f" rows="2" placeholder="key=value · key!=value · key~text · resource.key=value · body.key=value">(raw)</textarea>
            for e in errors {
                <span class="ferr">(e)</span>
            }
        </label>
    })
}

/// One-line hint row for an empty feed shown when a time range narrower
/// than `all` is set (the owner's chip-plus-1h empty page): the window is
/// the likely suspect, so offer the same query at `range=all`. A plain
/// `<a>` with the shared `muted` class — works with JS off, like the facets.
#[component]
pub(crate) async fn empty_range_hint(
    cx: &Cx,
    colspan: usize,
    noun: &'static str,
    href: String,
) -> Result<impl View> {
    Ok(view! {
        <tr>
            <td colspan=(colspan.to_string()) class="muted">
                (format!("No {noun} in this time range. "))
                <a href=(href)>"Search the same filters over all time"</a>
            </td>
        </tr>
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AttrOp, AttrSource, Bar, LOG_COLS, TRACE_COLS, parse_filter, parse_filter_list,
        percent_decode,
    };

    fn parsed(raw: &str) -> (AttrSource, AttrOp, String, String) {
        let f = parse_filter(raw);
        assert_eq!(f.error, None, "{raw} must parse");
        (f.source, f.op, f.key.clone(), f.value.clone())
    }

    #[test]
    fn operators_split_on_the_earliest_one() {
        assert_eq!(parsed("tool_name=bash"), (AttrSource::Row, AttrOp::Eq, "tool_name".into(), "bash".into()));
        assert_eq!(parsed("tool_name!=bash"), (AttrSource::Row, AttrOp::Ne, "tool_name".into(), "bash".into()));
        assert_eq!(parsed("session.id~ses_01"), (AttrSource::Row, AttrOp::Has, "session.id".into(), "ses_01".into()));
        // the earliest operator wins, the rest is value
        assert_eq!(parsed("a=b!=c"), (AttrSource::Row, AttrOp::Eq, "a".into(), "b!=c".into()));
        assert_eq!(parsed("a!=b=c"), (AttrSource::Row, AttrOp::Ne, "a".into(), "b=c".into()));
        assert_eq!(parsed("a~b=c"), (AttrSource::Row, AttrOp::Has, "a".into(), "b=c".into()));
    }

    #[test]
    fn prefixes_pick_the_column() {
        assert_eq!(parsed("resource.deployment.environment=prod").0, AttrSource::Resource);
        assert_eq!(parsed("resource.deployment.environment=prod").2, "deployment.environment");
        assert_eq!(parsed("body.message=session created"), (AttrSource::Body, AttrOp::Eq, "message".into(), "session created".into()));
        // only a leading prefix counts; a key that merely mentions one stays bare
        assert_eq!(parsed("my.resource.key=1").0, AttrSource::Row);
    }

    #[test]
    fn quoted_values_keep_spaces_and_operators() {
        assert_eq!(parsed(r#"body.msg="cache miss""#).3, "cache miss");
        assert_eq!(parsed("body.msg='cache miss'").3, "cache miss");
        assert_eq!(parsed(r#"k="a=b""#).3, "a=b");
        assert_eq!(parsed(r#"k~"a~b""#), (AttrSource::Row, AttrOp::Has, "k".into(), "a~b".into()));
        // unbalanced quotes are content
        assert_eq!(parsed(r#"k="a"#).3, "\"a");
        // unquoted spaces survive too (the URL encodes them); edges trim
        assert_eq!(parsed("k= a b ").3, "a b");
    }

    #[test]
    fn bad_lines_error_rather_than_panic() {
        assert!(parse_filter("just text").error.is_some());
        assert!(parse_filter("=value").error.is_some(), "missing key");
        assert!(parse_filter("resource.=x").error.is_some(), "prefix is not a key");
        assert!(parse_filter("k=").error.is_some(), "empty value");
        assert!(parse_filter("k!=").error.is_some());
        assert!(parse_filter("k~").error.is_some());
        // an explicitly quoted empty string IS a value: it matches ""
        assert_eq!(parse_filter(r#"k="""#).error, None);
        assert_eq!(parse_filter(r#"k="""#).value, "");
        // one bad line never takes the box down
        let fs = parse_filter_list("good=1\njunk\nk!=2");
        assert_eq!(fs.len(), 3);
        assert_eq!(fs.iter().filter(|f| f.error.is_some()).count(), 1);
    }

    #[test]
    fn list_splits_lines_not_commas() {
        let fs = parse_filter_list("a=1\r\nb=2\n\n c~3 ");
        assert_eq!(fs.len(), 3);
        assert_eq!(fs[2].value, "3");
    }

    #[test]
    fn canonical_round_trips() {
        use super::canonical_of;
        let mut f = parse_filter("resource.cluster=us east");
        assert_eq!(
            canonical_of(f.source, f.op, &f.key, &f.value),
            "resource.cluster=us east"
        );
        f.value = "\"weird".into();
        assert_eq!(
            canonical_of(f.source, f.op, &f.key, &f.value),
            "resource.cluster=\"\"weird\""
        );
        let back = parse_filter(&canonical_of(f.source, f.op, &f.key, &f.value));
        assert_eq!(back.error, None);
        assert_eq!(back.value, "\"weird");
    }

    #[test]
    fn bar_decodes_repeated_f_and_plain_params() {
        let bar = Bar::parse("q=time%20out&f=tool_name%3Dbash&severity=ERROR&f=resource.env%3Dprod&service=cart");
        assert_eq!(bar.get("q"), "time out");
        assert_eq!(bar.get("severity"), "ERROR");
        assert_eq!(bar.get("service"), "cart");
        assert_eq!(bar.all("f"), vec!["tool_name=bash", "resource.env=prod"]);
        assert_eq!(bar.attrs().len(), 2);
        assert_eq!(bar.attrs()[0].key, "tool_name");
        assert_eq!(bar.attrs()[1].source, AttrSource::Resource);
    }

    #[test]
    fn percent_decode_handles_plus_and_strays() {
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn all_time_cutoff_is_epoch() {
        let bar = Bar::parse("");
        assert_eq!(bar.cutoff(), "1970-01-01 00:00:00");
        assert_eq!(bar.limit(), 200);
    }

    #[test]
    fn narrow_range_is_the_one_the_empty_hint_acts_on() {
        // only a range the cutoff actually honours counts as narrow
        assert!(Bar::parse("range=1h").range_is_narrow());
        assert!(Bar::parse("range=30d").range_is_narrow());
        assert!(!Bar::parse("").range_is_narrow(), "no range cutoffs to all");
        assert!(!Bar::parse("range=all").range_is_narrow());
        assert!(!Bar::parse("range=junk").range_is_narrow(), "junk cutoffs to epoch like all");
    }

    #[test]
    fn range_and_limit_are_clamped() {
        let bar = Bar::parse("range=24h&limit=9999");
        assert_ne!(bar.cutoff(), "1970-01-01 00:00:00");
        assert_eq!(bar.limit(), 2000);
        assert_eq!(Bar::parse("range=junk").cutoff(), "1970-01-01 00:00:00");
        assert_eq!(Bar::parse("limit=0").limit(), 1);
    }

    #[test]
    fn encoding_covers_the_boring_and_the_nasty() {
        assert_eq!(Bar::enc("cart"), "cart");
        assert_eq!(Bar::enc("cache miss"), "cache%20miss");
        assert_eq!(Bar::enc("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn facet_links_keep_the_rest_of_the_query() {
        let bar = Bar::parse("q=time+out&range=6h&service=cart&severity=ERROR");
        // turning one facet off drops it, keeps the others
        assert_eq!(
            bar.href_toggling("/logs", "severity", "ERROR", true),
            "/logs?q=time%20out&range=6h&service=cart"
        );
        // switching to a value replaces the previous one
        assert_eq!(
            bar.href_toggling("/logs", "severity", "WARN", false),
            "/logs?q=time%20out&range=6h&service=cart&severity=WARN"
        );
        // no filters at all yields a clean path
        let empty = Bar::parse("");
        assert_eq!(empty.href_toggling("/logs", "service", "cart", false), "/logs?service=cart");
        assert_eq!(empty.href_toggling("/logs", "service", "cart", true), "/logs");
        // blank values mean "off" and never ride along
        let blanks = Bar::parse("service=&severity=ERROR");
        assert_eq!(blanks.href("/logs", &[]), "/logs?severity=ERROR");
    }

    #[test]
    fn chip_links_stack_f_and_dedupe() {
        let bar = Bar::parse("service=cart");
        assert_eq!(
            bar.href_add_filter("/logs", "tool_name=bash"),
            "/logs?service=cart&f=tool_name%3Dbash"
        );
        // already active: the link keeps the query as-is, no duplicate f
        let twice = Bar::parse("service=cart&f=tool_name%3Dbash");
        assert_eq!(twice.href_add_filter("/logs", "tool_name=bash"), "/logs?service=cart&f=tool_name%3Dbash");
    }

    #[test]
    fn attr_conds_bind_paths_and_values_in_order() {
        let bar = Bar::parse("f=tool_name%3Dbash&f=resource.env%3Dprod&f=msg~miss&f=junk");
        let (conds, params) = bar.attr_conds(&LOG_COLS);
        // the bad line is skipped, not executed
        assert_eq!(conds.len(), 3);
        assert_eq!(conds[0], "((CASE WHEN json_valid(log_attributes) THEN json_extract_string(log_attributes, ?) END) = ?)");
        assert_eq!(conds[1], "((CASE WHEN json_valid(resource_attributes) THEN json_extract_string(resource_attributes, ?) END) = ?)");
        assert_eq!(conds[2], "((CASE WHEN json_valid(log_attributes) THEN json_extract_string(log_attributes, ?) END) ILIKE '%' || ? || '%')");
        assert_eq!(
            params,
            vec![
                r#"$."tool_name""#.to_string(),
                "bash".to_string(),
                r#"$."env""#.to_string(),
                "prod".to_string(),
                r#"$."msg""#.to_string(),
                "miss".to_string(),
            ]
        );
    }

    #[test]
    fn attr_conds_on_traces_skip_body_filters() {
        let bar = Bar::parse("f=body.msg%3Dx&f=http.method%3DPOST");
        let (conds, params) = bar.attr_conds(&TRACE_COLS);
        assert_eq!(conds.len(), 1);
        assert!(conds[0].contains("span_attributes"));
        assert_eq!(params, vec![r#"$."http.method""#.to_string(), "POST".to_string()]);
    }

    #[test]
    fn body_conds_read_stored_json_and_decoded_array_elements() {
        let bar = Bar::parse("f=body.version%3D2.3.0");
        let (conds, params) = bar.attr_conds(&LOG_COLS);
        assert_eq!(conds.len(), 1);
        let sql = &conds[0];
        // direct read on the stored body AND the one-level-decoded string
        // elements of [message, "<payload>"] array bodies
        assert!(sql.contains("json_extract_string(body, ?)"));
        assert!(sql.contains("json_extract(CASE WHEN json_valid(body) THEN body END, '$[*]')"));
        assert!(sql.contains("WHERE json_valid(i) LIMIT 1"));
        // path, path, value — the order the `?` appear in the text
        assert_eq!(params.len(), 3);
        assert_eq!(params[0], r#"$."version""#);
        assert_eq!(params[1], r#"$."version""#);
        assert_eq!(params[2], "2.3.0");
        // the SQL text itself carries no user input
        assert!(!sql.contains("version\""));
        assert!(!sql.contains("2.3.0"));
    }

    #[test]
    fn hostile_keys_stay_out_of_the_sql_text() {
        let evil = r#"a"),(SELECT 1"#;
        let bar = Bar::parse(&format!(
            "f={}",
            super::Bar::enc(&format!("{evil}=x"))
        ));
        let (conds, params) = bar.attr_conds(&LOG_COLS);
        assert_eq!(conds.len(), 1);
        assert!(!conds[0].contains("SELECT 1"), "user text leaked into SQL text");
        // the path is data: quotes survive as DuckDB's own path escaping,
        // wrapped in the path's own member quotes
        assert_eq!(params[0], "$.\"a\\\"),(SELECT 1\"");
    }
}
