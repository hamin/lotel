//! Storage: the capped store's segment-level truth — what is on disk, what
//! the caps are, and when data will be forgotten.

use topcoat::{
    Result,
    context::{Cx, context},
    router::page,
    view::{View, view},
};

use crate::db::Db;

const TABLES: [&str; 6] = [
    "otlp_logs",
    "otlp_traces",
    "otlp_metrics_gauge",
    "otlp_metrics_sum",
    "otlp_metrics_histogram",
    "otlp_metrics_exp_histogram",
];

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

#[cfg(test)]
mod tests {
    use super::{human_bytes, human_us};

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
