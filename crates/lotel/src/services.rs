//! Service map: a dependency graph of the services seen in the trace store.
//!
//! Nodes are services; an edge `A → B` is a parent→child span pair where the
//! child runs in a different service — a real observed call. The layout is a
//! deterministic layered (left-to-right) ranking computed in pure Rust so it
//! is stable across reloads and unit-testable; edges are drawn in an SVG
//! underlay (built as a string) and the nodes are positioned HTML anchors that
//! deep-link into the traces explorer.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{BoxView, Unescaped, ViewExt, view},
};

use crate::{
    db::Db,
    stats::{self, SvcEdge, SvcStat},
    traces::service_class,
};

#[derive(Debug, Default, Deserialize)]
pub struct MapFilters {
    /// `15m` | `1h` | `6h` | `24h` | anything else = all time.
    #[serde(default)]
    pub range: String,
}

/// Pixel geometry for the layered layout.
const COLX: f64 = 224.0;
const ROWY: f64 = 96.0;
const NODE_W: f64 = 172.0;
const NODE_H: f64 = 68.0;
const PAD: f64 = 10.0;
/// Max ranking passes — bounds the longest-path loop and caps runaway cycles.
const MAX_PASSES: usize = 64;

/// A node placed on the canvas.
#[derive(Debug, Clone, PartialEq)]
pub struct Laid {
    pub name: String,
    pub depth: usize,
    pub rank: usize,
    pub x: f64,
    pub y: f64,
}

/// Rank services into columns by longest path from any root, then stack each
/// column by call volume. Returns placed nodes and the canvas size. Pure — no
/// DB — so the geometry is testable in isolation.
pub fn layout(stats: &[SvcStat], edges: &[SvcEdge]) -> (Vec<Laid>, f64, f64) {
    let names: Vec<&str> = stats.iter().map(|s| s.service.as_str()).collect();
    if names.is_empty() {
        return (Vec::new(), 0.0, 0.0);
    }
    let known: HashSet<&str> = names.iter().copied().collect();

    // Longest-path ranking, capped: depth(dst) = max(depth(dst), depth(src)+1).
    let mut depth: HashMap<&str, usize> = names.iter().map(|n| (*n, 0)).collect();
    let passes = edges.len().clamp(1, MAX_PASSES);
    for _ in 0..passes {
        let mut changed = false;
        for e in edges {
            if !(known.contains(e.src.as_str()) && known.contains(e.dst.as_str())) {
                continue;
            }
            let d = depth[e.src.as_str()] + 1;
            if d > depth[e.dst.as_str()] {
                depth.insert(e.dst.as_str(), d);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Volume per service, for ordering within a column (busier services higher).
    let vol: HashMap<&str, i64> = stats
        .iter()
        .map(|s| (s.service.as_str(), s.spans))
        .collect();
    let mut out_vol: HashMap<&str, i64> = HashMap::new();
    for e in edges {
        *out_vol.entry(e.src.as_str()).or_default() += e.calls;
    }

    let mut cols: HashMap<usize, Vec<&str>> = HashMap::new();
    for n in &names {
        cols.entry(depth[n]).or_default().push(n);
    }
    for col in cols.values_mut() {
        col.sort_by(|a, b| {
            let key = |s: &&str| {
                vol.get(*s).copied().unwrap_or(0) + out_vol.get(*s).copied().unwrap_or(0)
            };
            key(b).cmp(&key(a)).then_with(|| a.cmp(b))
        });
    }

    let max_depth = cols.keys().copied().max().unwrap_or(0);
    let max_rank = cols.values().map(|c| c.len()).max().unwrap_or(1);

    let mut laid = Vec::new();
    for (d, col) in cols {
        for (r, name) in col.into_iter().enumerate() {
            laid.push(Laid {
                name: name.to_string(),
                depth: d,
                rank: r,
                x: PAD + d as f64 * COLX,
                y: PAD + r as f64 * ROWY,
            });
        }
    }
    laid.sort_by_key(|l| (l.depth, l.rank));
    let w = PAD * 2.0 + NODE_W + max_depth as f64 * COLX;
    let h = PAD * 2.0 + NODE_H + (max_rank.max(1) as f64 - 1.0) * ROWY;
    (laid, w, h)
}

/// A fully rendered node — all fields owned so the boxed view can move them in
/// without borrowing any of the function's locals.
struct NodeView {
    name: String,
    href: String,
    title: String,
    head_class: String,
    spans: String,
    p50: String,
    p95: String,
    err_class: String,
    err_text: String,
    spark: String,
    x: f64,
    y: f64,
}

/// Edge geometry only, decoupled from the DB row.
struct EdgePath {
    d: String,
    width: f64,
    err: f64,
    title: String,
    label: String,
    mx: f64,
    my: f64,
}

#[page]
async fn services(cx: &Cx) -> Result<BoxView<'_>> {
    let db = context::<Db>(cx);
    let f = parse_query_params::<MapFilters>(cx).unwrap_or_default();
    let range = if f.range.is_empty() {
        "1h".to_string()
    } else {
        f.range.clone()
    };

    let nodes = stats::service_stats(db, &range);
    let edges = stats::service_edges(db, &range);
    let (_, svc_series) = stats::error_series(db, &range);
    let rate_of = |t: i64, e: i64| if t == 0 { 0.0 } else { e as f64 / t as f64 };
    // Shared scale so node sparklines are comparable; floor keeps them visible.
    let max_rate = svc_series
        .iter()
        .map(|s| rate_of(s.total, s.errs))
        .fold(0.0f64, f64::max)
        .max(0.02);

    let (laid, cw, ch) = layout(&nodes, &edges);
    let pos: HashMap<&str, &Laid> = laid.iter().map(|l| (l.name.as_str(), l)).collect();
    let stat_by_svc: HashMap<&str, &SvcStat> =
        nodes.iter().map(|s| (s.service.as_str(), s)).collect();
    let series_by_svc: HashMap<&str, &crate::stats::SvcSeries> =
        svc_series.iter().map(|s| (s.service.as_str(), s)).collect();

    // Precompute all owned render data BEFORE the view, so nothing the boxed
    // view captures borrows a local.
    let drawn: Vec<EdgePath> = edges
        .iter()
        .filter_map(|e| {
            let s = pos.get(e.src.as_str())?;
            let d = pos.get(e.dst.as_str())?;
            Some(EdgePath {
                d: edge_path(s.x, s.y, d.x, d.y),
                width: (1.0 + (e.calls as f64) * 0.05).min(6.0),
                err: e.err_rate(),
                title: format!(
                    "{} → {} · {} calls · {} errors · p95 {:.1} ms",
                    e.src, e.dst, e.calls, e.errs, e.p95_ms
                ),
                label: compact(e.calls),
                mx: s.x + NODE_W + (d.x - (s.x + NODE_W)) / 2.0,
                my: s.y + NODE_H / 2.0 + (d.y - s.y) / 2.0,
            })
        })
        .collect();
    let svg = edges_svg(&drawn, cw, ch);

    let node_views: Vec<NodeView> = laid
        .iter()
        .map(|l| {
            let st = stat_by_svc.get(l.name.as_str()).copied();
            let spans = st.map(|s| s.spans).unwrap_or(0);
            let errs = st.map(|s| s.errs).unwrap_or(0);
            let p50_ms = st.map(|s| s.p50_ms).unwrap_or(0.0);
            let p95_ms = st.map(|s| s.p95_ms).unwrap_or(0.0);
            let er = rate_of(spans, errs);
            let (err_class, err_text) = if er >= 0.02 {
                ("sm-err".to_string(), stats::pct(er))
            } else if er > 0.0 {
                ("sm-err dim".to_string(), stats::pct(er))
            } else {
                ("sm-ok".to_string(), "ok".to_string())
            };
            let spark = series_by_svc
                .get(l.name.as_str())
                .map(|s| stats::spark_svg(&s.rates, 152.0, 26.0, "#f0858a", max_rate))
                .unwrap_or_default();
            NodeView {
                name: l.name.clone(),
                href: format!("/traces?service={}", urlenc(&l.name)),
                title: format!(
                    "{} · {} spans · {} errors · p95 {:.1} ms",
                    l.name, spans, errs, p95_ms
                ),
                head_class: format!("svc-dot {}", service_class(&l.name)),
                spans: spans.to_string(),
                p50: format!("{p50_ms:.1}"),
                p95: format!("{p95_ms:.1}"),
                err_class,
                err_text,
                spark,
                x: l.x,
                y: l.y,
            }
        })
        .collect();

    let services_n = nodes.len().to_string();
    let edges_n = edges.len().to_string();
    let total_spans: i64 = nodes.iter().map(|n| n.spans).sum();
    let total_errs: i64 = nodes.iter().map(|n| n.errs).sum();
    let overall = stats::pct(rate_of(total_spans, total_errs));
    let canvas_style = format!("width:{cw}px;height:{ch}px");
    let has_nodes = !node_views.is_empty();

    Ok(view! {
        <h1>"Service map"</h1>
        <p class="sub">
            "Services and the calls between them, reconstructed from parent→child spans. Node colour is stable per service; red edges carry errors."
        </p>

        <form class="filters" method="get" action="/services">
            <label>"Range"
                <select name="range">
                    <option value="15m" if range == "15m" { selected="selected" }>"15 min"</option>
                    <option value="1h" if range == "1h" { selected="selected" }>"1 hour"</option>
                    <option value="6h" if range == "6h" { selected="selected" }>"6 hours"</option>
                    <option value="24h" if range == "24h" { selected="selected" }>"24 hours"</option>
                    <option value="" if !["15m", "1h", "6h", "24h"].contains(&range.as_str()) { selected="selected" }>"all"</option>
                </select>
            </label>
            <button type="submit">"Apply"</button>
            <span class="tail">
                (services_n)" services · "(edges_n)" edges · "
                (total_spans.to_string())" spans · "
                if total_errs > 0 {
                    <span class="badge error">(overall)" errors"</span>
                } else {
                    <span class="badge info">"no errors"</span>
                }
            </span>
        </form>

        if !has_nodes {
            <p class="muted">"No spans in this window — send OTLP traffic or widen the range."</p>
        } else {
            <div class="card sm-wrap">
                <div class="sm-canvas" style=(canvas_style)>
                    (Unescaped::new_unchecked(svg))
                    for n in node_views {
                        <a class="sm-node" href=(n.href) title=(n.title)
                           style=(format!("left:{}px;top:{}px;width:{}px;height:{}px", n.x, n.y, NODE_W, NODE_H))>
                            <div class="sm-head">
                                <span class=(n.head_class)>"●"</span>
                                <b>(n.name)</b>
                            </div>
                            <div class="sm-stats">
                                (n.spans)" spans · p50 "(n.p50)" / p95 "(n.p95)" ms · "
                                <span class=(n.err_class)>(n.err_text)</span>
                            </div>
                            <div class="sm-spark">(Unescaped::new_unchecked(n.spark))</div>
                        </a>
                    }
                </div>
            </div>

            <h2>"Legend"</h2>
            <p class="muted sm-legend">
                "Click a service to open its spans · the edge label is its call count · red means at least 2% errors · the sparkline is the error-rate trend, shared scale."
            </p>
        }
    }
    .boxed())
}

/// The SVG underlay: arrowhead markers plus one labelled cubic per edge.
fn edges_svg(edges: &[EdgePath], w: f64, h: f64) -> String {
    let mut s = format!(
        "<svg class=\"sm-edges\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\
         <defs>\
         <marker id=\"arw\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" markerWidth=\"6\" markerHeight=\"6\" orient=\"auto-start-reverse\"><path d=\"M0,0 L10,5 L0,10 z\" class=\"arw\"/></marker>\
         <marker id=\"arwe\" viewBox=\"0 0 10 10\" refX=\"9\" refY=\"5\" markerWidth=\"6\" markerHeight=\"6\" orient=\"auto-start-reverse\"><path d=\"M0,0 L10,5 L0,10 z\" class=\"arwe\"/></marker>\
         </defs>"
    );
    for e in edges {
        let hot = e.err >= 0.02;
        let cls = if hot { "smedge err" } else { "smedge" };
        let marker = if hot { "url(#arwe)" } else { "url(#arw)" };
        s.push_str(&format!(
            "<path d=\"{}\" class=\"{cls}\" stroke-width=\"{:.1}\" marker-end=\"{marker}\"><title>{}</title></path>",
            e.d,
            e.width,
            esc(&e.title)
        ));
        let lcls = if hot { "smlabel err" } else { "smlabel" };
        s.push_str(&format!(
            "<text class=\"{lcls}\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\">{}</text>",
            e.mx,
            e.my - 5.0,
            esc(&e.label)
        ));
    }
    s.push_str("</svg>");
    s
}

/// Escape text for safe embedding in the generated SVG.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A left-to-right cubic from the caller's right edge to the callee's left edge.
fn edge_path(sx: f64, sy: f64, dx: f64, dy: f64) -> String {
    let x1 = sx + NODE_W;
    let y1 = sy + NODE_H / 2.0;
    let x2 = dx;
    let y2 = dy + NODE_H / 2.0;
    let bend = ((x2 - x1).abs() * 0.5).max(30.0);
    let c1 = x1 + bend;
    let c2 = x2 - bend;
    format!("M{x1:.1},{y1:.1} C{c1:.1},{y1:.1} {c2:.1},{y2:.1} {x2:.1},{y2:.1}")
}

/// Call-count label: `1.2k` above 999.
fn compact(n: i64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Percent-encode a service name for a query string.
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{compact, edge_path, layout, urlenc};
    use crate::stats::{SvcEdge, SvcStat};

    fn st(service: &str, spans: i64) -> SvcStat {
        SvcStat {
            service: service.into(),
            spans,
            errs: 0,
            p50_ms: 1.0,
            p95_ms: 2.0,
        }
    }
    fn eg(src: &str, dst: &str, calls: i64) -> SvcEdge {
        SvcEdge {
            src: src.into(),
            dst: dst.into(),
            calls,
            errs: 0,
            p95_ms: 1.0,
        }
    }

    #[test]
    fn empty_graph_is_zero_sized() {
        let (laid, w, h) = layout(&[], &[]);
        assert!(laid.is_empty());
        assert_eq!((w, h), (0.0, 0.0));
    }

    #[test]
    fn chain_ranks_left_to_right() {
        let nodes = vec![st("gateway", 10), st("checkout", 8), st("payments", 5)];
        let edges = vec![eg("gateway", "checkout", 8), eg("checkout", "payments", 5)];
        let (laid, w, h) = layout(&nodes, &edges);
        let x = |n: &str| laid.iter().find(|l| l.name == n).unwrap().x;
        assert!(x("gateway") < x("checkout"));
        assert!(x("checkout") < x("payments"));
        assert_eq!(laid.iter().find(|l| l.name == "payments").unwrap().depth, 2);
        assert!(w > 0.0 && h > 0.0);
    }

    #[test]
    fn fanout_shares_a_column() {
        let nodes = vec![st("gateway", 10), st("a", 3), st("b", 9)];
        let edges = vec![eg("gateway", "a", 3), eg("gateway", "b", 9)];
        let (laid, _, _) = layout(&nodes, &edges);
        let a = laid.iter().find(|l| l.name == "a").unwrap();
        let b = laid.iter().find(|l| l.name == "b").unwrap();
        assert_eq!(a.depth, b.depth);
        assert_ne!(a.y, b.y);
        assert!(b.y < a.y, "busier service ranks higher");
    }

    #[test]
    fn cycles_do_not_hang_or_overflow() {
        let nodes = vec![st("a", 1), st("b", 1), st("c", 1)];
        let edges = vec![eg("a", "b", 1), eg("b", "c", 1), eg("c", "a", 1)];
        let (laid, _, _) = layout(&nodes, &edges);
        assert_eq!(laid.len(), 3);
        assert!(laid.iter().all(|l| l.depth <= 64), "depth stays bounded");
    }

    #[test]
    fn isolated_service_still_placed() {
        let nodes = vec![st("solo", 4)];
        let (laid, _, _) = layout(&nodes, &[]);
        let solo = laid.iter().find(|l| l.name == "solo").unwrap();
        assert_eq!(solo.depth, 0);
        assert_eq!(solo.rank, 0);
    }

    #[test]
    fn edge_label_compacts_thousands() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1200), "1.2k");
    }

    #[test]
    fn edge_path_is_a_cubic() {
        let d = edge_path(0.0, 0.0, 300.0, 100.0);
        assert!(d.starts_with('M'));
        assert!(d.contains(" C"));
    }

    #[test]
    fn urlenc_escapes_spaces() {
        assert_eq!(urlenc("my svc"), "my%20svc");
        assert_eq!(urlenc("api-gateway"), "api-gateway");
    }
}
