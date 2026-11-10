//! Shared UI chrome: the dark console stylesheet, nav, and small helpers.

use topcoat::{
    Result,
    context::Cx,
    view::{Unescaped, View, component, view},
};

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
.badge.hist   { background: #241b2e; color: #b07fc9; }
.badge.exp    { background: #16282b; color: #6fb3b8; }
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
/* — trace waterfall — */
.wf { border: 1px solid #1b212e; border-radius: 8px; background: #0e1420;
      padding: 0.5rem 0.75rem; font-size: 0.8rem; }
.wfruler, .wfrow { display: grid; grid-template-columns: 320px 1fr; gap: 0.6rem;
                   align-items: center; }
.wfruler { color: #5c6575; font-size: 0.68rem; padding-bottom: 0.3rem;
           border-bottom: 1px solid #1b212e; margin-bottom: 0.35rem; }
.wfruler .wftrack { position: relative; height: 1rem; }
.wfruler .tick { position: absolute; top: 0; }
.wfrow { color: inherit; text-decoration: none; padding: 0.12rem 0; border-radius: 4px; }
.wfrow:hover { background: #141a26; }
.wflabel { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.wflabel b { color: #c8d2e2; font-weight: 600; }
.wftrack { position: relative; height: 1.15rem; }
.wfbar { position: absolute; top: 0.12rem; height: 0.9rem; min-width: 2px;
         border-radius: 3px; font-size: 0.66rem; line-height: 0.9rem;
         padding: 0 0.3rem; overflow: hidden; white-space: nowrap; color: #0b0e14; }
.wfbar.wferr { outline: 1.5px solid #f0858a; }
.svc-dot { font-size: 0.6rem; margin-right: 0.15rem; }
.svc0, .wfbar.svc0 { background: #4f8cc9; } .svc-dot.svc0 { background: none; color: #4f8cc9; }
.svc1, .wfbar.svc1 { background: #57b98a; } .svc-dot.svc1 { background: none; color: #57b98a; }
.svc2, .wfbar.svc2 { background: #c9a04f; } .svc-dot.svc2 { background: none; color: #c9a04f; }
.svc3, .wfbar.svc3 { background: #b07fc9; } .svc-dot.svc3 { background: none; color: #b07fc9; }
.svc4, .wfbar.svc4 { background: #c97f7f; } .svc-dot.svc4 { background: none; color: #c97f7f; }
.svc5, .wfbar.svc5 { background: #6fb3b8; } .svc-dot.svc5 { background: none; color: #6fb3b8; }
/* — histogram distribution — */
.hist { font-size: 0.8rem; }
.hrow { display: grid; grid-template-columns: 150px 1fr; gap: 0.6rem;
        align-items: center; padding: 0.14rem 0; }
.hlabel { color: #aab4c5; text-align: right; white-space: nowrap; }
.htrack { position: relative; height: 1.05rem; background: #0b0f18;
          border-radius: 3px; overflow: hidden; }
.hbar { position: absolute; inset: 0 auto 0 0; min-width: 2px;
        background: linear-gradient(90deg, #3f7d63, #7dd3a0); border-radius: 3px; }
.hcount { position: absolute; top: 0; line-height: 1.05rem; padding-left: 0.35rem;
          color: #d7dce5; font-size: 0.72rem; font-variant-numeric: tabular-nums; }
/* — explore: query builder + results — */
.qtabs { display: flex; gap: 0.25rem; margin-bottom: 1rem; border-bottom: 1px solid #1b212e; }
.qtabs .tab { padding: 0.45rem 0.9rem; color: #8a93a5; text-decoration: none; font-size: 0.85rem;
              border-bottom: 2px solid transparent; margin-bottom: -1px; }
.qtabs .tab:hover { color: #d7dce5; }
.qtabs .tab.on { color: #eaf0fa; border-bottom-color: #7dd3a0; }
.qb { background: #0e1420; border: 1px solid #1b212e; border-radius: 10px; padding: 0.9rem 1rem;
      margin-bottom: 1rem; }
.qrow { display: flex; flex-wrap: wrap; gap: 0.6rem; margin-bottom: 0.7rem; }
.qfield { display: flex; flex-direction: column; gap: 0.22rem; }
.qcap { font-size: 0.68rem; color: #7d8595; text-transform: uppercase; letter-spacing: 0.08em; }
.qb select, .qb input { background: #11151f; color: #d7dce5; border: 1px solid #232b3b;
      border-radius: 6px; padding: 0.35rem 0.5rem; font-size: 0.83rem; }
.qb select:focus, .qb input:focus { outline: 1px solid #3d68a8; }
.qsec { margin: 0.7rem 0; }
.qlab { display: block; font-size: 0.68rem; color: #7d8595; text-transform: uppercase;
        letter-spacing: 0.08em; margin-bottom: 0.35rem; }
.qtoken { display: inline-flex; gap: 0.3rem; align-items: center; margin: 0 0.35rem 0.35rem 0;
          background: #141a26; border: 1px solid #232b3b; border-radius: 7px; padding: 0.22rem 0.3rem; }
.qtoken .qcol, .qtoken .qop, .qtoken .qfn, .qtoken .qval, .qtoken .qkey {
          background: #0e1420; border: 1px solid #26304400; border-radius: 5px; padding: 0.25rem 0.4rem; }
.qtoken .qval { min-width: 7rem; }
.qtoken .qkey { min-width: 7rem; }
.qtoken .qcol { color: #9ecbff; }
.qdel { background: none; border: 0; color: #6b7484; cursor: pointer; font-size: 1rem; line-height: 1;
        padding: 0 0.2rem; }
.qdel:hover { color: #f0858a; }
.qadd { background: none; border: 1px dashed #2b3548; color: #8a93a5; border-radius: 6px;
        padding: 0.28rem 0.7rem; font-size: 0.78rem; cursor: pointer; }
.qadd:hover { color: #d7dce5; border-color: #3d68a8; }
.qrun { background: #1c6b46; color: #eafbf1; border: 1px solid #25915e; border-radius: 7px;
        padding: 0.5rem 1.4rem; font-size: 0.88rem; font-weight: 600; cursor: pointer; margin-top: 0.3rem; }
.qrun:hover { background: #22855a; }
.qstats { display: flex; flex-wrap: wrap; gap: 1rem; align-items: baseline; margin: 0.2rem 0 0.7rem;
          font-size: 0.82rem; color: #8a93a5; }
.qstats .qs b { color: #d7dce5; font-variant-numeric: tabular-nums; }
.qchart { min-height: 20px; }
.qtablewrap { overflow-x: auto; border: 1px solid #1b212e; border-radius: 8px; }
.qtable { margin: 0; }
.qtable thead th { position: sticky; top: 0; background: #11151f; z-index: 1; }
.qtable td { max-width: 32rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.qempty { padding: 1rem 1.1rem; margin: 0; }
.qerr { background: #241014; border: 1px solid #4a1e24; border-radius: 8px; padding: 0.7rem 0.9rem;
        margin-bottom: 1rem; color: #f0858a; }
.qerr pre { background: none; border: 0; padding: 0.2rem 0 0; margin: 0.3rem 0 0; color: #f4a7ab;
            white-space: pre-wrap; }
.sqlform { margin-bottom: 1rem; }
.sqlbox { width: 100%; background: #0b0f18; color: #cfe0ff; border: 1px solid #232b3b;
          border-radius: 8px; padding: 0.7rem 0.9rem; font-family: ui-monospace, Menlo, monospace;
          font-size: 0.82rem; resize: vertical; }
.sqlbox:focus { outline: 1px solid #3d68a8; }
.sqlbar { display: flex; align-items: center; gap: 0.8rem; margin-top: 0.5rem; }
.sqlbar button { background: #1c6b46; color: #eafbf1; border: 1px solid #25915e; border-radius: 7px;
          padding: 0.42rem 1.2rem; font-size: 0.85rem; font-weight: 600; cursor: pointer; }
.sqlbar button:hover { background: #22855a; }
.qsql { margin-top: 0.9rem; }
.qsql summary { cursor: pointer; color: #8a93a5; font-size: 0.8rem; }
/* — explore: result tab bar (events / series / breakdown) — */
.rtabs { display: flex; gap: 0.15rem; border-bottom: 1px solid #1b212e; margin-bottom: 0.7rem; }
.rtab { padding: 0.35rem 0.75rem; font-size: 0.8rem; color: #8a93a5; text-decoration: none;
        border-bottom: 2px solid transparent; margin-bottom: -1px; }
.rtab:hover { color: #d7dce5; }
.rtab.on { color: #eaf0fa; border-bottom-color: #4f8cc9; }
/* — explore: two-column results + facet sidebar + stats tooltip — */
.qcols { display: grid; grid-template-columns: minmax(0, 1fr) 224px; gap: 1.2rem; align-items: start; }
.qmain { min-width: 0; }
.qfacets { position: sticky; top: 1rem; max-height: calc(100vh - 2rem); overflow-y: auto;
           padding-left: 1.2rem; border-left: 1px solid #1b212e; }
.qfacets-head { font-size: 0.7rem; color: #7d8595; text-transform: uppercase;
                letter-spacing: 0.1em; margin-bottom: 0.6rem; }
.facet { margin-bottom: 0.9rem; }
.fhead { display: flex; justify-content: space-between; align-items: baseline; font-size: 0.72rem;
         color: #aab4c5; font-weight: 600; padding: 0.1rem 0.2rem; cursor: help; }
.fkind { color: #5c6575; font-weight: 400; font-size: 0.66rem; }
.fvals { margin-top: 0.2rem; }
.fval { display: grid; grid-template-columns: 1fr auto; gap: 0.5rem; align-items: center;
        position: relative; padding: 0.13rem 0.35rem; border-radius: 5px; color: #aab4c5;
        text-decoration: none; font-size: 0.75rem; }
.fval:hover { background: #141a26; color: #eaf0fa; }
.fvlab { position: relative; z-index: 1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.fvcnt { position: relative; z-index: 1; color: #7d8595; font-variant-numeric: tabular-nums; }
.fvbar { position: absolute; left: 0; top: 0; bottom: 0; z-index: 0; opacity: 0.55; border-radius: 5px;
         background: linear-gradient(90deg, #24405f, #2f5a86); }
.fcell { color: #9ecbff; text-decoration: none; cursor: pointer; }
.fcell:hover { text-decoration: underline; }
.qtable th[data-stat] { cursor: help; }
.qtable th[data-stat]:hover { color: #d7dce5; }
.qtip { position: absolute; background: #11151f; border: 1px solid #2b3548; color: #d7dce5;
        padding: 0.28rem 0.5rem; border-radius: 6px; font-size: 0.72rem; z-index: 60;
        pointer-events: none; box-shadow: 0 6px 18px rgba(0, 0, 0, 0.45); white-space: nowrap; }
/* — dashboard: error-rate sparklines — */
.spark { display: block; overflow: visible; }
.spark-area { opacity: 0.16; }
.spark-line { fill: none; stroke-width: 1.5px; stroke-linejoin: round; stroke-linecap: round; }
.errcard .err-top { display: flex; align-items: center; gap: 1rem; flex-wrap: wrap; }
.err-now { display: flex; flex-direction: column; }
.err-big { font-size: 1.8rem; font-weight: 680; color: #f0858a; font-variant-numeric: tabular-nums; }
.err-spark { flex: 1 1 240px; min-width: 200px; }
.err-svc-list { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 0.3rem 1.4rem; margin-top: 0.7rem; }
.err-svc { display: grid; grid-template-columns: 1fr 120px auto; gap: 0.6rem; align-items: center; text-decoration: none; color: inherit; padding: 0.12rem 0.2rem; border-radius: 5px; }
.err-svc:hover { background: #141a26; }
.err-svc-name { font-size: 0.82rem; color: #c8d2e2; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.err-svc-rate { font-size: 0.78rem; color: #7d8595; font-variant-numeric: tabular-nums; text-align: right; min-width: 3.2rem; }
.err-svc-rate.hi { color: #f0858a; }
.err-foot { margin: 0.6rem 0 0; font-size: 0.75rem; }
/* — service map — */
.sm-wrap { overflow: auto; padding: 0.5rem; }
.sm-canvas { position: relative; }
.sm-edges { position: absolute; inset: 0; pointer-events: none; }
.smedge { fill: none; stroke: #3a465c; stroke-opacity: 0.9; }
.smedge.err { stroke: #b8525a; stroke-opacity: 0.95; }
.arw { fill: #3a465c; } .arwe { fill: #b8525a; }
.smlabel { fill: #8a93a5; font-size: 11px; font-variant-numeric: tabular-nums; paint-order: stroke; stroke: #0b0e14; stroke-width: 3px; stroke-linejoin: round; }
.smlabel.err { fill: #f0a0a4; }
.sm-node { position: absolute; box-sizing: border-box; display: flex; flex-direction: column; gap: 0.15rem;
           background: #0e1420; border: 1px solid #232b3b; border-radius: 8px; padding: 0.45rem 0.6rem;
           text-decoration: none; color: inherit; box-shadow: 0 1px 0 #0b0e14; }
.sm-node:hover { border-color: #3d68a8; background: #121a28; }
.sm-head { display: flex; align-items: center; gap: 0.3rem; font-size: 0.82rem; }
.sm-head b { color: #eaf0fa; font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sm-stats { font-size: 0.7rem; color: #7d8595; font-variant-numeric: tabular-nums; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.sm-err { color: #f0858a; } .sm-err.dim { color: #b98a8d; } .sm-ok { color: #57b98a; }
.sm-spark { line-height: 0; }
.sm-legend { font-size: 0.78rem; }
.sm-lat svg { display: block; }
.lat { display: grid; grid-template-columns: minmax(220px, 1fr) minmax(260px, 1.15fr); gap: 1.4rem; margin: 0.2rem 0 1.2rem; }
.lat h3 { margin: 0 0 0.5rem; font-size: 0.68rem; letter-spacing: 0.09em; text-transform: uppercase; color: #7d8595; }
.lat svg { max-width: 100%; }
.lat-cap { font-size: 0.72rem; color: #7d8595; margin: 0.4rem 0 0; }
.dist-row { display: grid; grid-template-columns: 84px 1fr 52px; gap: 0.5rem; align-items: center; padding: 0.12rem 0; font-size: 0.76rem; }
.dist-n { text-align: right; color: #7d8595; font-variant-numeric: tabular-nums; }
@media (max-width: 900px) {
    .lat { grid-template-columns: 1fr; }
}

.facets { display: grid; grid-template-columns: repeat(auto-fit, minmax(260px, 1fr)); gap: 14px; margin: 0.2rem 0 1.1rem; }
.facet { background: #0e1420; border: 1px solid #1b2334; border-radius: 10px; padding: 0.6rem 0.75rem; }
.facet h3 { margin: 0 0 0.45rem; font-size: 0.68rem; letter-spacing: 0.09em; text-transform: uppercase; color: #7d8595; }
.facet-row { display: grid; grid-template-columns: minmax(70px, 130px) 1fr 44px; gap: 0.5rem;
             align-items: center; padding: 0.18rem 0.3rem; border-radius: 6px;
             color: inherit; text-decoration: none; font-size: 0.78rem; }
.facet-row:hover { background: #141b2c; }
.facet-row.on { background: #16203a; outline: 1px solid #2b3a55; }
.facet-k { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.facet-bar { height: 8px; background: #10182a; border-radius: 4px; overflow: hidden; }
.facet-bar i { display: block; height: 100%; background: linear-gradient(90deg, #3d68a8, #6ea3e8); }
.facet-n { text-align: right; color: #7d8595; font-variant-numeric: tabular-nums; }
.ranked { margin: 0.2rem 0 1rem; }
.ranked-head { display: flex; gap: 0.6rem; align-items: baseline; margin-bottom: 0.4rem; }
.ranked-title { font-weight: 600; color: #eaf0fa; }
.ranked-row { display: grid; grid-template-columns: 150px 46px 62px 110px 1fr; gap: 0.55rem;
              align-items: baseline; padding: 0.22rem 0.2rem; border-top: 1px solid #151b28;
              color: inherit; text-decoration: none; font-size: 0.8rem; }
.ranked-row:hover { background: #121a28; }
.ranked-time { font-size: 0.72rem; }
.badge.score { background: #17233b; color: #9db9ff; font-size: 0.66rem; }
.ranked-svc { color: #aab4c5; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

@media (max-width: 900px) {
    .qcols { grid-template-columns: 1fr; }
    .qfacets { position: static; max-height: none; padding-left: 0; border-left: 0;
               border-top: 1px solid #1b212e; padding-top: 0.9rem; }
}
"#;

/// Sidebar nav: (label, path).
pub(crate) fn nav_links() -> [(&'static str, &'static str); 7] {
    [
        ("Dashboard", "/"),
        ("Logs", "/logs"),
        ("Traces", "/traces"),
        ("Metrics", "/metrics"),
        ("Explore", "/query"),
        ("Services", "/services"),
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
            if sev.eq_ignore_ascii_case("FATAL") || sev.eq_ignore_ascii_case("CRITICAL") {
                "fatal"
            } else {
                "error"
            }
        }
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::sev_class;

    #[test]
    fn severity_families() {
        assert_eq!(sev_class("INFO"), "info");
        assert_eq!(sev_class("information"), "info");
        assert_eq!(sev_class("WARN"), "warn");
        assert_eq!(sev_class("WARNING"), "warn");
        assert_eq!(sev_class("ERROR"), "error");
        assert_eq!(sev_class("FATAL"), "fatal");
        assert_eq!(sev_class("critical"), "fatal");
        assert_eq!(sev_class(""), "other");
        assert_eq!(sev_class("banana"), "other");
    }
}


/// The live-refresh controller, embedded once per page that opts in. Polls a
/// `/api/live?topic=` fragment and swaps the target's HTML — pausing when the
/// tab is hidden and when the toggle is off. This is the single client half of
/// the freshness seam: swap its `fetch` for an `EventSource` to go push-based
/// (the SaaS shape) without touching any page.
pub(crate) const LIVE_JS: &str = r#"
window.LotelLive=(function(){
  function sub(o){
    var t=document.getElementById(o.target);
    if(!t) return;
    var ctl=o.toggle?document.getElementById(o.toggle):null;
    var base='/api/live?topic='+encodeURIComponent(o.topic);
    var ms=o.cadence||5000, timer=null;
    function url(){ var s=window.location.search||''; return base+(s?'&'+s.replace(/^\?/,''):''); }
    function tick(){
      if(document.hidden) return;
      fetch(url(),{cache:'no-store'}).then(function(r){return r.text();}).then(function(h){t.innerHTML=h;}).catch(function(){});
    }
    function on(){ if(!timer){ t.setAttribute('data-live','on'); timer=setInterval(tick,ms); tick(); } }
    function off(){ if(timer){ clearInterval(timer); timer=null; } t.setAttribute('data-live','off'); }
    function sync(){ if(ctl && !ctl.checked) off(); else on(); }
    if(ctl) ctl.addEventListener('change',sync);
    sync();
  }
  return {sub:sub};
})();
"#;

/// Opt a region into live refresh: loads the controller and subscribes `target`
/// to `/api/live?topic=` while `toggle` is checked. Pauses when hidden.
#[component]
pub(crate) async fn live_refresh(
    cx: &Cx,
    topic: String,
    target: String,
    toggle: String,
    cadence: u32,
) -> Result<impl View> {
    let boot = format!(
        "LotelLive.sub({{topic:{topic:?},target:{target:?},toggle:{toggle:?},cadence:{cadence}}});"
    );
    Ok(view! {
        <script>(Unescaped::new_unchecked(LIVE_JS))</script>
        <script>(Unescaped::new_unchecked(boot))</script>
    })
}
