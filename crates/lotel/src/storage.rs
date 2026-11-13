//! Storage: the capped store's segment-level truth — what is on disk, what
//! the caps are, and when data will be forgotten.

use topcoat::{
    Result,
    context::{Cx, context},
    router::page,
    view::{View, component, view},
};

use crate::{
    db::Db,
    ui::thousands,
};

const TABLES: [&str; 6] = [
    "otlp_logs",
    "otlp_traces",
    "otlp_metrics_gauge",
    "otlp_metrics_sum",
    "otlp_metrics_histogram",
    "otlp_metrics_exp_histogram",
];

/// Microseconds in a day / an hour — the ribbon's countdown units.
const DAY_US: i64 = 86_400_000_000;
const HOUR_US: i64 = 3_600_000_000;

/// Human rendering of the microsecond caps capped-core reports.
fn human_us(us: &str) -> String {
    let Ok(v) = us.parse::<i64>() else {
        return us.to_string();
    };
    if v == 0 {
        return "—".into();
    }
    let units = [
        ("d", 86_400_000_000i64),
        ("h", 3_600_000_000),
        ("m", 60_000_000),
        ("s", 1_000_000),
        ("ms", 1_000),
    ];
    for (u, size) in units {
        if v >= size {
            return format!("{}{}", v / size, u);
        }
    }
    format!("{}us", v)
}

fn human_bytes(b: &str) -> String {
    let Ok(v) = b.parse::<i64>() else {
        return b.to_string();
    };
    const UNITS: [(&str, i64); 3] = [("GB", 1 << 30), ("MB", 1 << 20), ("KB", 1 << 10)];
    for (u, size) in UNITS {
        if v >= size {
            return format!("{:.1} {}", v as f64 / size as f64, u);
        }
    }
    format!("{v} B")
}

/// The server clock in microseconds — the ribbon's right edge. (The mock
/// pinned `now` to the newest row because its fixture data is old; the live
/// app measures against reality.)
fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or_default()
}

/// (left %, width %) of one segment on its table's retention window, which
/// spans `now − ttl .. now` with the use-by line at the left edge. The drawn
/// span is the segment clipped to the window: rows older than the use-by
/// line and rows ahead of `now` do not count, so a partly expired segment
/// shrinks instead of overhanging the line. The 0.8% floor keeps a fresh —
/// even empty — segment visible: when it bites at the right edge, the
/// segment shifts left rather than shrinking to nothing. This fixes two
/// bugs the mock's `storage_ribbon()` in design/useby/build.py still
/// carries (that file deliberately keeps the reference version).
fn seg_geometry(min_ts: i64, max_ts: i64, now: i64, ttl_us: i64) -> (f64, f64) {
    let start = now - ttl_us;
    let lo = min_ts.max(start);
    let hi = max_ts.max(lo).min(now);
    let left = (((lo - start) as f64 / ttl_us as f64) * 100.0).clamp(0.0, 100.0);
    let width = (((hi - lo) as f64 / ttl_us as f64) * 100.0).max(0.8);
    (left.min(100.0 - width), width)
}

/// Countdown label until a segment is forgotten: retention deletes it whole
/// once it slides past the LEFT edge — the use-by line — which happens when
/// its newest row reaches `max_ts + ttl`, so the remaining `ttl − (now −
/// max_ts)` reads "6d 16h" (hours only under a day, "0h" once past).
fn forgets_in(max_ts: i64, now: i64, ttl_us: i64) -> String {
    let rem = (max_ts + ttl_us - now).max(0);
    let days = rem / DAY_US;
    let hours = (rem % DAY_US) / HOUR_US;
    if days > 0 { format!("{days}d {hours}h") } else { format!("{hours}h") }
}

/// One positioned segment on the ribbon.
struct RibbonSeg {
    open: bool,
    style: String,
    title: String,
}

/// One ribbon row: a table's segments on its window, plus the segment id and
/// countdown of the oldest sealed segment — the next one to be forgotten.
struct RibbonRow {
    table: String,
    ttl_label: String,
    segs: Vec<RibbonSeg>,
    next: Option<(String, String)>,
}

/// Group `capped_status` rows (ten columns per segment) into one ribbon row
/// per capped table, keeping the tables' first-seen order. A table whose ttl
/// is missing or unusable gets no row — there is no window to lay it on, and
/// the mock skips it the same way. An open segment with no rows yet sits at
/// `now` for both ends, exactly as the mock fills empties.
fn ribbon_rows(segments: &[Vec<String>], now: i64) -> Vec<RibbonRow> {
    let mut grouped: Vec<(&str, Vec<&Vec<String>>)> = Vec::new();
    for row in segments {
        if row.len() != 10 {
            continue; // the query-failure placeholder, not segment data
        }
        match grouped.iter_mut().find(|(t, _)| *t == row[0]) {
            Some((_, rows)) => rows.push(row),
            None => grouped.push((row[0].as_str(), vec![row])),
        }
    }
    let mut out = Vec::new();
    for (table, rows) in grouped {
        let Ok(ttl_us) = rows[0][7].parse::<i64>() else { continue };
        if ttl_us <= 0 {
            continue;
        }
        // The sealed segment whose newest row is oldest is next to forget.
        let oldest = rows
            .iter()
            .filter(|r| r[2] == "true")
            .map(|r| (r[1].as_str(), r[6].parse::<i64>().unwrap_or(now)))
            .min_by_key(|(_, mx)| *mx);
        let segs = rows
            .iter()
            .map(|r| {
                let open = r[2] != "true";
                let min_ts = r[5].parse::<i64>().unwrap_or(now);
                let max_ts = r[6].parse::<i64>().unwrap_or(now);
                let (left, width) = seg_geometry(min_ts, max_ts, now, ttl_us);
                RibbonSeg {
                    open,
                    style: format!("left:{left:.2}%;width:{width:.2}%"),
                    title: format!(
                        "segment {} · {} rows · {} · {}",
                        r[1],
                        thousands(&r[3]),
                        human_bytes(&r[4]),
                        if open { "open" } else { "sealed" },
                    ),
                }
            })
            .collect();
        let next = oldest.map(|(sid, mx)| (sid.to_string(), forgets_in(mx, now, ttl_us)));
        out.push(RibbonRow {
            table: table.to_string(),
            ttl_label: human_us(&rows[0][7]),
            segs,
            next,
        });
    }
    out
}

/// The use-by ribbon: every capped table's segments laid on its retention
/// window, oldest-sealed countdowns in the right column. The port of
/// `storage_ribbon()` in design/useby/build.py — same markup and maths, with
/// the server clock for `now` instead of the newest row.
#[component]
async fn ribbon(cx: &Cx, rows: Vec<RibbonRow>) -> Result<impl View> {
    Ok(view! {
        <section class="ribbon" aria-labelledby="ribbon-h">
            <h2 id="ribbon-h">"What forgets next"</h2>
            <p class="muted ribbon-cap">
                "Each table's segments laid on its retention window. The left edge is the use-by line: a segment that slides past it is deleted whole. The right edge is now."
            </p>
            <div class="ribbon-rows">
                for r in rows {
                    <div class="ribbon-row">
                        <div class="ribbon-name">
                            <span class="mono">(r.table)</span>
                            <span class="useby"><b>(r.ttl_label)</b></span>
                        </div>
                        <div class="ribbon-track">
                            <span class="ribbon-line" aria-hidden="true"></span>
                            for s in r.segs {
                                <span class=(if s.open { "seg seg-open" } else { "seg" })
                                      style=(s.style) title=(s.title)></span>
                            }
                        </div>
                        <div class="ribbon-next muted">
                            if let Some((sid, countdown)) = r.next {
                                "segment "(sid)" forgets in "<b>(countdown)</b>
                            }
                        </div>
                    </div>
                }
                <div class="ribbon-axis muted" aria-hidden="true">
                    <span>"use-by line"</span><span>"now"</span>
                </div>
            </div>
        </section>
    })
}

#[cfg(test)]
mod tests {
    use super::{DAY_US, HOUR_US, forgets_in, human_bytes, human_us, ribbon_rows, seg_geometry};

    /// A ten-column `capped_status` row, in the page's column order.
    #[allow(clippy::too_many_arguments)]
    fn seg(
        table: &str,
        id: &str,
        sealed: &str,
        rows: &str,
        bytes: &str,
        min: &str,
        max: &str,
        ttl: &str,
    ) -> Vec<String> {
        vec![
            table.into(),
            id.into(),
            sealed.into(),
            rows.into(),
            bytes.into(),
            min.into(),
            max.into(),
            ttl.into(),
            "2000000000".into(),
            "true".into(),
        ]
    }

    #[test]
    fn humanizes_ttl() {
        assert_eq!(human_us("604800000000"), "7d");
        assert_eq!(human_us("259200000000"), "3d");
        assert_eq!(human_us("2592000000000"), "30d");
        assert_eq!(human_us("3600000000"), "1h");
        assert_eq!(human_us("0"), "—");
        assert_eq!(human_us("500000"), "500ms");
        assert_eq!(human_us("not-a-number"), "not-a-number");
    }

    #[test]
    fn humanizes_sizes() {
        assert_eq!(human_bytes("2000000000"), "1.9 GB");
        assert_eq!(human_bytes("1047"), "1.0 KB");
        assert_eq!(human_bytes("512"), "512 B");
        assert_eq!(human_bytes("1048576"), "1.0 MB");
        assert_eq!(human_bytes("x"), "x");
    }

    #[test]
    fn geometry_places_segments_on_the_window() {
        let now = 1_700_000_000_000_000;
        let ttl = 7 * DAY_US;
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;

        // Fully inside: three days old to two days old — a day of a week window.
        let (l, w) = seg_geometry(now - 3 * DAY_US, now - 2 * DAY_US, now, ttl);
        assert_eq!(format!("left:{l:.2}%;width:{w:.2}%"), "left:57.14%;width:14.29%");

        // Partly expired (min = start − 2d, max = start + 1d): left clamps to 0
        // and the width counts only the day still inside the window — the mock
        // wrongly drew the expired span too.
        let (l, w) = seg_geometry(now - 9 * DAY_US, now - 6 * DAY_US, now, ttl);
        assert_eq!(l, 0.0);
        assert_eq!(format!("{w:.2}"), "14.29");

        // A fresh one-instant segment: the 0.8% width floor keeps it visible.
        let (l, w) = seg_geometry(now - DAY_US, now - DAY_US, now, ttl);
        assert_eq!(format!("{l:.2}"), "85.71");
        assert_eq!(w, 0.8);

        // The open segment of live data sits at `now` (empty ones fall back to
        // it too): the floor wins by shifting it left, not by shrinking it to
        // the invisible sliver the mock produced.
        let (l, w) = seg_geometry(now, now, now, ttl);
        assert!(near(l, 99.2), "left {l}");
        assert!(near(w, 0.8), "width {w}");

        // The last five minutes of a week: a visible sliver at the right edge.
        let (l, w) = seg_geometry(now - 5 * 60 * 1_000_000, now, now, ttl);
        assert_eq!(format!("left:{l:.2}%;width:{w:.2}%"), "left:99.20%;width:0.80%");

        // Fully expired (max before the use-by line): a sliver at the line,
        // waiting for retention to take the whole file.
        let (l, w) = seg_geometry(now - 10 * DAY_US, now - 9 * DAY_US, now, ttl);
        assert!(near(l, 0.0), "left {l}");
        assert!(near(w, 0.8), "width {w}");
    }

    #[test]
    fn countdown_labels_days_and_hours() {
        let now = 1_700_000_000_000_000;
        let ttl = 7 * DAY_US;
        // The mock's headline case: last row 8h old on a 7d table -> 6d 16h left.
        assert_eq!(forgets_in(now - 8 * HOUR_US, now, ttl), "6d 16h");
        // Under a day: hours only.
        assert_eq!(forgets_in(now - 6 * DAY_US - 8 * HOUR_US, now, ttl), "16h");
        // Already past the line: clamped, never negative.
        assert_eq!(forgets_in(now - 8 * DAY_US, now, ttl), "0h");
    }

    #[test]
    fn ribbon_groups_tables_and_names_the_oldest_sealed() {
        let now = 1_700_000_000_000_000;
        let ttl = "604800000000"; // 7d
        let segs = vec![
            seg("otlp_logs", "0", "true", "6486", "168656", &(now - 9 * DAY_US).to_string(), &(now - 6 * DAY_US).to_string(), ttl),
            seg("otlp_logs", "1", "true", "11024", "276400", &(now - 2 * DAY_US).to_string(), &(now - DAY_US).to_string(), ttl),
            seg("otlp_logs", "2", "false", "60322", "1500000", "", "", ttl), // open, no rows yet
            vec!["otlp_traces".into(), "error: boom".into()],                 // query-failure placeholder
            seg("otlp_metrics_gauge", "0", "false", "3", "512", "", "", ""),  // ttl missing
            seg("otlp_metrics_sum", "0", "true", "3", "512", "", "", "0"),    // ttl unusable
        ];
        let rows = ribbon_rows(&segs, now);
        assert_eq!(rows.len(), 1); // the placeholder and the ttl-less tables are skipped
        let r = &rows[0];
        assert_eq!(r.table, "otlp_logs");
        assert_eq!(r.ttl_label, "7d");

        // Partly expired sealed segment at the left edge (width counts only the
        // span still inside the window), a healthy one, and the open one shifted
        // onto the track so the 0.8% floor keeps it visible.
        assert_eq!(r.segs[0].style, "left:0.00%;width:14.29%");
        assert!(!r.segs[0].open);
        assert_eq!(r.segs[1].style, "left:71.43%;width:14.29%");
        assert_eq!(r.segs[2].style, "left:99.20%;width:0.80%"); // empties sit at `now`
        assert!(r.segs[2].open);
        assert_eq!(r.segs[0].title, "segment 0 · 6,486 rows · 164.7 KB · sealed");

        // Oldest sealed by max_ts is segment 0 (six days old) -> one day left on a 7d window.
        assert_eq!(r.next, Some(("0".to_string(), "1d 0h".to_string())));
    }

    #[test]
    fn ribbon_row_without_sealed_segments_has_no_countdown() {
        let now = 1_700_000_000_000_000;
        let segs = vec![seg(
            "otlp_logs",
            "0",
            "false",
            "5",
            "512",
            &(now - DAY_US).to_string(),
            &(now - HOUR_US).to_string(),
            "604800000000",
        )];
        let rows = ribbon_rows(&segs, now);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].segs[0].open);
        assert_eq!(rows[0].next, None);
    }
}

#[page]
async fn storage(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);

    let overview = db
        .query_rows("SELECT * FROM capped_tables('otel')")
        .unwrap_or_else(|e| (vec!["error".into()], vec![vec![format!("{e}")]]));

    // Segment detail per table, concatenated with the table name prefixed.
    let mut segments: Vec<Vec<String>> = Vec::new();
    for t in TABLES {
        match db.query_rows(&format!(
            "SELECT '{t}', segment_id, sealed, rows, bytes, min_ts_us, max_ts_us, ttl_us, max_bytes, indexed \
             FROM capped_status('otel', '{t}')"
        )) {
            Ok((_, rows)) => segments.extend(rows),
            Err(e) => segments.push(vec![t.to_string(), format!("error: {e}")]),
        }
    }

    Ok(view! {
        <h1>"Storage"</h1>
        <p class="sub">"Every table is sealed parquet segments plus one open segment; retention deletes whole files."</p>

        ribbon(rows: ribbon_rows(&segments, now_us()))

        <h2>"Tables"</h2>
        <table>
            <thead>
                <tr>
                    for col in &overview.0 {
                        <th>(col.clone())</th>
                    }
                </tr>
            </thead>
            <tbody>
                for row in &overview.1 {
                    <tr>
                        for cell in row {
                            <td>(cell.clone())</td>
                        }
                    </tr>
                }
            </tbody>
        </table>

        <h2>"Segments"</h2>
        <table>
            <thead>
                <tr>
                    <th>"table"</th><th>"segment"</th><th>"sealed"</th><th>"rows"</th><th>"bytes"</th>
                    <th>"min ts (µs)"</th><th>"max ts (µs)"</th><th>"ttl"</th><th>"max bytes"</th><th>"indexed"</th>
                </tr>
            </thead>
            <tbody>
                for row in &segments {
                    <tr>
                        if row.len() == 10 {
                            <td class="mono">(row[0].clone())</td>
                            <td>(row[1].clone())</td>
                            <td>(row[2].clone())</td>
                            <td>(row[3].clone())</td>
                            <td>(human_bytes(&row[4]))</td>
                            <td class="muted">(row[5].clone())</td>
                            <td class="muted">(row[6].clone())</td>
                            <td>(human_us(&row[7]))</td>
                            <td>(human_bytes(&row[8]))</td>
                            <td>(row[9].clone())</td>
                        } else {
                            <td colspan="10" class="muted">(row.join(" "))</td>
                        }
                    </tr>
                }
            </tbody>
        </table>
    })
}
