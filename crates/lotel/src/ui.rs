//! Shared UI chrome: the dark console stylesheet, nav, and small helpers.

/// Calm dark console look; shipped raw since CSS is not HTML.
pub(crate) const CSS: &str = r#"
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body {
    margin: 0; padding: 0;
    background: #0b0e14; color: #d7dce5;
    font: 15px/1.55 ui-sans-serif, -apple-system, "Segoe UI", sans-serif;
}
.layout { display: flex; min-height: 100vh; }
nav {
    flex: 0 0 190px; padding: 1.25rem 1rem;
    border-right: 1px solid #1b212e; background: #0d1119;
}
nav .brand { font-weight: 700; letter-spacing: 0.01em; margin: 0 0 1.25rem 0.5rem; }
nav .brand .dot { color: #7dd3a0; }
nav a {
    display: block; padding: 0.4rem 0.5rem; margin-bottom: 0.15rem;
    color: #8a93a5; text-decoration: none; border-radius: 6px; font-size: 0.9rem;
}
nav a:hover { color: #d7dce5; background: #141a26; }
nav a[aria-current="page"] { color: #eaf0fa; background: #182031; }
main { flex: 1; max-width: 1080px; padding: 2rem 2.25rem; }
h1 { font-size: 1.35rem; letter-spacing: 0.01em; margin: 0 0 0.25rem; }
.sub { color: #7d8595; margin: 0 0 1.5rem; }
h2 { font-size: 0.8rem; text-transform: uppercase; letter-spacing: 0.12em;
     color: #7d8595; margin: 1.75rem 0 0.5rem; }
table { border-collapse: collapse; width: 100%; font-size: 0.85rem; }
th, td { text-align: left; padding: 0.4rem 0.6rem; border-bottom: 1px solid #1b212e;
         vertical-align: top; }
th { color: #8a93a5; font-weight: 600; white-space: nowrap; }
td { font-variant-numeric: tabular-nums; }
tr:hover td { background: #0e1420; }
code, pre { color: #9ecbff; }
pre { background: #11151f; border: 1px solid #1b212e; border-radius: 6px;
      padding: 0.7rem 0.9rem; overflow-x: auto; font-size: 0.8rem; }
.kv td:first-child { color: #8a93a5; width: 12rem; }
.badge {
    display: inline-block; min-width: 3.2rem; text-align: center;
    padding: 0.05rem 0.45rem; border-radius: 999px; font-size: 0.72rem;
    font-weight: 600; letter-spacing: 0.04em;
}
.badge.trace  { background: #1b212e; color: #8a93a5; }
.badge.debug  { background: #16212e; color: #7ea6d9; }
.badge.info   { background: #12261f; color: #7dd3a0; }
.badge.warn   { background: #2a2314; color: #e6c068; }
.badge.error  { background: #2c1618; color: #f0858a; }
.badge.fatal  { background: #3a0f14; color: #ff6b74; }
.badge.other  { background: #1b212e; color: #aab4c5; }
.filters {
    display: flex; flex-wrap: wrap; gap: 0.5rem; align-items: end;
    background: #0e1420; border: 1px solid #1b212e; border-radius: 8px;
    padding: 0.75rem 0.9rem; margin-bottom: 1rem;
}
.filters label { display: flex; flex-direction: column; gap: 0.2rem;
                 font-size: 0.72rem; color: #7d8595; text-transform: uppercase;
                 letter-spacing: 0.08em; }
.filters input, .filters select {
    background: #11151f; color: #d7dce5; border: 1px solid #232b3b;
    border-radius: 6px; padding: 0.35rem 0.55rem; font-size: 0.85rem;
}
.filters input:focus, .filters select:focus { outline: 1px solid #3d68a8; }
.filters button {
    background: #182031; color: #d7dce5; border: 1px solid #2b3548;
    border-radius: 6px; padding: 0.42rem 0.9rem; font-size: 0.85rem; cursor: pointer;
}
.filters button:hover { background: #1e2841; }
.tail { display: flex; align-items: center; gap: 0.4rem; font-size: 0.8rem;
        color: #8a93a5; text-transform: none; letter-spacing: 0; }
.tail input { accent-color: #7dd3a0; }
.logbody { white-space: pre-wrap; word-break: break-word; font-variant-numeric: normal; }
.muted { color: #7d8595; }
.mono { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 0.8rem; }
.trace-link { color: #9ecbff; text-decoration: none; }
.trace-link:hover { text-decoration: underline; }
.card {
    background: #0e1420; border: 1px solid #1b212e; border-radius: 8px;
    padding: 0.9rem 1.1rem; margin-bottom: 1rem;
}
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(220px, 1fr)); gap: 0.75rem; }
.stat .n { font-size: 1.5rem; font-weight: 650; }
.stat .l { font-size: 0.72rem; color: #7d8595; text-transform: uppercase; letter-spacing: 0.08em; }
"#;

/// Sidebar nav: (label, path).
pub(crate) fn nav_links() -> [(&'static str, &'static str); 5] {
    [
        ("Dashboard", "/"),
        ("Logs", "/logs"),
        ("Traces", "/traces"),
        ("Metrics", "/metrics"),
        ("Storage", "/storage"),
    ]
}

/// Lowercase severity family for badge styling.
pub(crate) fn sev_class(sev: &str) -> &'static str {
    match sev.to_ascii_uppercase().as_str() {
        "TRACE" | "TRACE2" | "TRACE3" | "TRACE4" => "trace",
        "DEBUG" | "DEBUG2" | "DEBUG3" | "DEBUG4" => "debug",
        "INFO" | "INFORMATION" | "INFORMATIONAL" | "NOTICE" => "info",
        "WARN" | "WARNING" => "warn",
        "ERROR" | "ERR" | "FATAL" | "CRITICAL" | "CRIT" | "EMERGENCY" | "EMERG" | "ALERT" => {
            if sev.to_ascii_uppercase() == "FATAL" || sev.to_ascii_uppercase() == "CRITICAL" {
                "fatal"
            } else {
                "error"
            }
        }
        _ => "other",
    }
}
