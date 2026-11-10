//! Chart data for the Explore page: a `[xs, series…]` shape uPlot consumes,
//! served bare via the `/api/*` layout exception. Handles both builder plans
//! (time series, optionally stacked by breakdown) and the SQL console.

use serde_json::{Value, json};

use topcoat::{
    Result,
    context::{Cx, context},
    router::{page, parse_query_params},
    view::{Unescaped, View, view},
};

use crate::{
    db::Db,
    query::{self, PALETTE, QueryState},
};

fn bound(p: &[String]) -> Vec<&dyn duckdb::ToSql> {
    p.iter().map(|s| s as &dyn duckdb::ToSql).collect()
}

#[page]
async fn chart(cx: &Cx) -> Result<impl View> {
    let db = context::<Db>(cx);
    let s = parse_query_params::<QueryState>(cx).unwrap_or_default();
    Ok(view! { (Unescaped::new_unchecked(build(db, &s))) })
}

fn build(db: &Db, s: &QueryState) -> String {
    if s.mode == "sql" {
        let plan = query::compile_sql(s);
        if let Some(e) = &plan.error {
            return json!({ "error": e }).to_string();
        }
        return match db.query_rows_with(&plan.table_sql, &[]) {
            Ok((cols, rows)) => sql_series(&cols, &rows).to_string(),
            Err(e) => json!({ "error": e.to_string() }).to_string(),
        };
    }
    let plan = query::compile(s);
    if let Some(e) = &plan.error {
        return json!({ "error": e }).to_string();
    }
    match db.query_rows_with(&plan.chart_sql, &bound(&plan.chart_params)) {
        Ok((_, rows)) => {
            if plan.stacked {
                builder_stacked(&rows).to_string()
            } else {
                builder_lines(&rows, &plan.measure_labels).to_string()
            }
        }
        Err(e) => json!({ "error": e.to_string() }).to_string(),
    }
}

fn sorted_unique_x(rows: &[Vec<String>]) -> Vec<f64> {
    let mut xs: Vec<f64> = rows.iter().filter_map(|r| r[0].parse().ok()).collect();
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    xs.dedup();
    xs
}

fn index_of(xs: &[f64], x: f64) -> Option<usize> {
    xs.iter().position(|&e| (e - x).abs() < 1e-6)
}

/// Stacked bars: rows are `[x_ms, group, value]`; each group becomes a series.
fn builder_stacked(rows: &[Vec<String>]) -> Value {
    let xs = sorted_unique_x(rows);
    let mut totals: Vec<(String, f64)> = Vec::new();
    for r in rows {
        if r.len() < 3 {
            continue;
        }
        let g = r[1].clone();
        let v = r[2].parse().unwrap_or(0.0);
        match totals.iter_mut().find(|(k, _)| *k == g) {
            Some(t) => t.1 += v,
            None => totals.push((g, v)),
        }
    }
    totals.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    totals.truncate(10);
    let series: Vec<Value> = totals
        .iter()
        .enumerate()
        .map(|(i, (g, _))| {
            let mut data: Vec<Option<f64>> = vec![None; xs.len()];
            for r in rows {
                if r.len() < 3 || &r[1] != g {
                    continue;
                }
                if let (Some(ix), Some(v)) = (
                    r[0].parse::<f64>().ok().and_then(|x| index_of(&xs, x)),
                    r[2].parse::<f64>().ok(),
                ) {
                    data[ix] = Some(v);
                }
            }
            json!({ "label": g, "color": PALETTE[i % PALETTE.len()], "data": data })
        })
        .collect();
    json!({ "time": true, "stacked": true, "xs": xs, "series": series })
}

/// One line per measure: rows are `[x_ms, m0, m1, …]`.
fn builder_lines(rows: &[Vec<String>], labels: &[String]) -> Value {
    let xs = sorted_unique_x(rows);
    let series: Vec<Value> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let mut data: Vec<Option<f64>> = vec![None; xs.len()];
            for r in rows {
                if let Some(ix) = r[0].parse::<f64>().ok().and_then(|x| index_of(&xs, x)) {
                    data[ix] = r.get(1 + i).and_then(|v| v.parse::<f64>().ok());
                }
            }
            json!({ "label": label, "color": PALETTE[i % PALETTE.len()], "data": data })
        })
        .collect();
    json!({ "time": true, "stacked": false, "xs": xs, "series": series })
}

/// Generic chart for arbitrary SQL: a numeric first column is the x axis
/// (else row index), every other fully-numeric column becomes a series.
fn sql_series(cols: &[String], rows: &[Vec<String>]) -> Value {
    if rows.is_empty() || cols.is_empty() {
        return json!({ "time": false, "stacked": false, "xs": [], "series": [] });
    }
    let numeric = |j: usize| -> bool {
        rows.iter()
            .filter(|r| r.get(j).is_some_and(|v| !v.is_empty()))
            .all(|r| r[j].parse::<f64>().is_ok())
    };
    let xnum = !rows.iter().all(|r| r[0].is_empty()) && numeric(0);
    let xs: Vec<f64> = if xnum {
        rows.iter().map(|r| r[0].parse().unwrap_or(0.0)).collect()
    } else {
        (0..rows.len()).map(|i| i as f64).collect()
    };
    let start = if xnum { 1 } else { 0 };
    let series: Vec<Value> = (start..cols.len())
        .filter(|&j| numeric(j))
        .enumerate()
        .map(|(i, j)| {
            let data: Vec<Option<f64>> = rows.iter().map(|r| r[j].parse().ok()).collect();
            json!({ "label": cols[j], "color": PALETTE[i % PALETTE.len()], "data": data })
        })
        .collect();
    json!({ "time": false, "stacked": false, "xs": xs, "series": series })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacked_groups_and_orders() {
        let rows = vec![
            vec!["100".into(), "a".into(), "5".into()],
            vec!["200".into(), "a".into(), "7".into()],
            vec!["100".into(), "b".into(), "1".into()],
        ];
        let v = builder_stacked(&rows);
        assert_eq!(v["stacked"], json!(true));
        assert_eq!(v["xs"], json!([100.0, 200.0]));
        // series ordered by total desc → "a" (12) before "b" (1)
        assert_eq!(v["series"][0]["label"], json!("a"));
        assert_eq!(v["series"][0]["data"], json!([5.0, 7.0]));
        assert_eq!(v["series"][1]["data"], json!([1.0, null]));
    }

    #[test]
    fn lines_align_to_x_axis() {
        let rows = vec![vec!["10".into(), "3".into()], vec!["20".into(), "4".into()]];
        let labels = vec!["count".to_string()];
        let v = builder_lines(&rows, &labels);
        assert_eq!(v["series"][0]["data"], json!([3.0, 4.0]));
    }

    #[test]
    fn sql_numeric_first_column_is_x() {
        let cols = vec!["x".into(), "y".into()];
        let rows = vec![vec!["1".into(), "9".into()], vec!["2".into(), "8".into()]];
        let v = sql_series(&cols, &rows);
        assert_eq!(v["xs"], json!([1.0, 2.0]));
        assert_eq!(v["series"][0]["label"], json!("y"));
        assert_eq!(v["series"][0]["data"], json!([9.0, 8.0]));
    }

    #[test]
    fn sql_nonnumeric_first_column_indexes() {
        let cols = vec!["name".into(), "n".into()];
        let rows = vec![vec!["a".into(), "5".into()], vec!["b".into(), "6".into()]];
        let v = sql_series(&cols, &rows);
        assert_eq!(v["xs"], json!([0.0, 1.0]));
        assert_eq!(v["series"][0]["label"], json!("n"));
    }
}
