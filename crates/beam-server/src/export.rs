//! Downloads: the current Table view as CSV or as an Excel workbook.
//!
//! The rows are exactly the ones on screen across every page: same filter,
//! same sort, same columns. Larger results are refused with an explanation
//! rather than quietly cut short.

use crate::app::AppState;
use crate::catalog;
use crate::feed;
use crate::table::{self, readable_timestamp, TableQuery};
use crate::uploads::csv_field;
use crate::web::{message_page, require_signed_in};
use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use beam_odata::{EdmType, Property};
use rust_xlsxwriter::{ExcelDateTime, Format, Workbook};
use serde_json::Value;
use std::sync::Arc;

/// Most rows one download may hold. Larger data belongs in Excel or Power BI
/// through the feed, which pages through any number of rows.
pub const EXPORT_LIMIT: u64 = 100_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Csv,
    Excel,
}

/// Neutralises text that a spreadsheet would run as a formula when the CSV
/// is opened (OWASP "CSV injection"): such cells gain a leading apostrophe.
fn csv_text(text: &str) -> String {
    let risky = text.starts_with(['=', '+', '-', '@', '\t', '\r']);
    csv_field(&if risky { format!("'{text}") } else { text.to_string() })
}

pub fn to_csv(columns: &[Property], rows: &[Vec<Value>]) -> Vec<u8> {
    let mut out = String::new();
    let header: Vec<String> = columns.iter().map(|c| csv_text(&c.name)).collect();
    out.push_str(&header.join(","));
    out.push_str("\r\n");
    for row in rows {
        let fields: Vec<String> = row
            .iter()
            .zip(columns)
            .map(|(value, column)| match value {
                Value::Null => String::new(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::String(s) if column.edm == EdmType::DateTimeOffset => readable_timestamp(s),
                Value::String(s) => csv_text(s),
                other => csv_text(&other.to_string()),
            })
            .collect();
        out.push_str(&fields.join(","));
        out.push_str("\r\n");
    }
    // A byte-order mark so Excel opens non-English text correctly.
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(out.as_bytes());
    bytes
}

pub fn to_excel(
    sheet_name: &str,
    columns: &[Property],
    rows: &[Vec<Value>],
) -> Result<Vec<u8>, rust_xlsxwriter::XlsxError> {
    let mut workbook = Workbook::new();
    let sheet = workbook.add_worksheet();
    sheet.set_name(sheet_name.chars().take(31).collect::<String>())?;
    let bold = Format::new().set_bold();
    let date = Format::new().set_num_format("yyyy-mm-dd");
    let datetime = Format::new().set_num_format("yyyy-mm-dd hh:mm:ss");
    for (index, column) in columns.iter().enumerate() {
        sheet.write_string_with_format(0, index as u16, &column.name, &bold)?;
    }
    for (row_index, row) in rows.iter().enumerate() {
        let r = row_index as u32 + 1;
        for (index, (value, column)) in row.iter().zip(columns).enumerate() {
            let c = index as u16;
            match value {
                Value::Null => {}
                Value::Bool(b) => {
                    sheet.write_boolean(r, c, *b)?;
                }
                Value::Number(n) => match n.as_f64() {
                    // Integers beyond 2^53 would silently lose digits as numbers.
                    Some(f) if f.abs() < 9_007_199_254_740_992.0 => {
                        sheet.write_number(r, c, f)?;
                    }
                    _ => {
                        sheet.write_string(r, c, n.to_string())?;
                    }
                },
                Value::String(s) if matches!(column.edm, EdmType::Date | EdmType::DateTimeOffset) => {
                    let text = readable_timestamp(s);
                    let shown = text.split('.').next().unwrap_or(&text);
                    match ExcelDateTime::parse_from_str(shown) {
                        Ok(when) if column.edm == EdmType::Date => {
                            sheet.write_datetime_with_format(r, c, &when, &date)?
                        }
                        Ok(when) => sheet.write_datetime_with_format(r, c, &when, &datetime)?,
                        Err(_) => sheet.write_string(r, c, s)?,
                    };
                }
                // Written as text, never as a formula, whatever it starts with.
                Value::String(s) => {
                    sheet.write_string(r, c, s)?;
                }
                other => {
                    sheet.write_string(r, c, other.to_string())?;
                }
            }
        }
    }
    if !columns.is_empty() {
        sheet.set_freeze_panes(1, 0)?;
        sheet.autofilter(0, 0, rows.len() as u32, columns.len() as u16 - 1)?;
        sheet.autofit();
    }
    workbook.save_to_buffer()
}

pub async fn csv(
    state: State<Arc<AppState>>,
    name: Path<String>,
    pairs: Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    export(state, name, pairs, headers, Kind::Csv).await
}

pub async fn excel(
    state: State<Arc<AppState>>,
    name: Path<String>,
    pairs: Query<Vec<(String, String)>>,
    headers: HeaderMap,
) -> Response {
    export(state, name, pairs, headers, Kind::Excel).await
}

async fn export(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(pairs): Query<Vec<(String, String)>>,
    headers: HeaderMap,
    kind: Kind,
) -> Response {
    let signed = match require_signed_in(&state, &headers) {
        Ok(signed) => signed,
        Err(response) => return response,
    };
    let Some(entry) = catalog::find(&state, &signed.session.account, &name) else {
        return message_page(
            StatusCode::NOT_FOUND,
            "Dataset not found",
            "There is no dataset with that name.",
        );
    };
    let set = &entry.set;
    let query = TableQuery::from_pairs(&pairs);
    let view = table::view(set, &query);
    if let Some(problem) = view.problem {
        return message_page(StatusCode::BAD_REQUEST, "Nothing to download", problem);
    }
    let mut options = view.options(set);
    options.push(("$top".into(), EXPORT_LIMIT.to_string()));
    let failed = || {
        message_page(
            StatusCode::BAD_REQUEST,
            "Nothing to download",
            "That view could not be read. Go back and check the filter.",
        )
    };
    let Ok(plan) = beam_odata::plan(set, &options, EXPORT_LIMIT) else {
        return failed();
    };
    let Ok(rows) = feed::run(&state, plan.rows_sql, &plan.rows_params, plan.columns.len()).await else {
        return failed();
    };
    if rows.len() as u64 > EXPORT_LIMIT {
        return message_page(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Too many rows to download",
            "Downloads hold up to 100,000 rows. Filter the table first, or connect Excel or Power BI to the data feed, which has no limit.",
        );
    }
    let columns = plan.columns;
    let set_name = set.name.clone();
    let built = tokio::task::spawn_blocking(move || match kind {
        Kind::Csv => Ok(to_csv(&columns, &rows)),
        Kind::Excel => to_excel(&set_name, &columns, &rows).map_err(|e| e.to_string()),
    })
    .await;
    let Ok(Ok(bytes)) = built else {
        return message_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Something went wrong",
            "The download could not be prepared.",
        );
    };
    let (content_type, extension) = match kind {
        Kind::Csv => ("text/csv; charset=utf-8", "csv"),
        Kind::Excel => (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "xlsx",
        ),
    };
    // Dataset names are plain identifiers, so they are safe inside the header.
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{}.{extension}\"", set.name))
        .unwrap_or(HeaderValue::from_static("attachment"));
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (CONTENT_DISPOSITION, disposition),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn columns() -> Vec<Property> {
        let p = |name: &str, edm| Property { name: name.into(), edm };
        vec![
            p("name", EdmType::String),
            p("amount", EdmType::Double),
            p("when", EdmType::DateTimeOffset),
            p("ok", EdmType::Boolean),
        ]
    }

    #[test]
    fn csv_neutralises_formulas_but_not_negative_numbers() {
        let rows = vec![
            vec![
                json!("=HYPERLINK(\"http://evil\")"),
                json!(-5),
                json!("2025-01-07T09:15:00.000000Z"),
                json!(true),
            ],
            vec![json!("@SUM(A1)"), Value::Null, Value::Null, json!(false)],
            vec![json!("plain, with comma"), json!(1.5), Value::Null, Value::Null],
        ];
        let text = String::from_utf8(to_csv(&columns(), &rows)[3..].to_vec()).unwrap();
        assert_eq!(
            text,
            "name,amount,when,ok\r\n\
             \"'=HYPERLINK(\"\"http://evil\"\")\",-5,2025-01-07 09:15:00,true\r\n\
             '@SUM(A1),,,false\r\n\
             \"plain, with comma\",1.5,,\r\n"
        );
    }

    #[test]
    fn excel_files_are_real_workbooks_that_read_back() {
        use calamine::{Data, Reader, Xlsx};
        let rows = vec![vec![
            json!("=1+1"),
            json!(12.5),
            json!("2025-01-07T09:15:00.000000Z"),
            json!(true),
        ]];
        let bytes = to_excel("a_very_long_dataset_name_that_exceeds_the_limit", &columns(), &rows).unwrap();
        assert!(bytes.starts_with(b"PK\x03\x04"));
        let mut workbook: Xlsx<_> = calamine::open_workbook_from_rs(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(
            workbook.sheet_names(),
            vec!["a_very_long_dataset_name_that_e".to_string()]
        );
        let range = workbook.worksheet_range_at(0).unwrap().unwrap();
        assert_eq!(range.get((0, 1)), Some(&Data::String("amount".into())));
        assert_eq!(
            range.get((1, 0)),
            Some(&Data::String("=1+1".into())),
            "text, not a formula"
        );
        assert_eq!(range.get((1, 1)), Some(&Data::Float(12.5)));
        assert_eq!(range.get((1, 3)), Some(&Data::Bool(true)));
        let Some(Data::DateTime(when)) = range.get((1, 2)) else {
            panic!("expected a date")
        };
        assert_eq!(when.to_ymd_hms_milli(), (2025, 1, 7, 9, 15, 0, 0));
    }
}
