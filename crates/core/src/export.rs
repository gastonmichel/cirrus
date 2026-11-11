//! Turning query results into CSV (for files) or TSV (for the clipboard).

use anyhow::Result;

use crate::model::QueryResult;

/// CSV with a header row; NULL becomes an empty field.
pub fn to_csv(result: &QueryResult) -> Result<String> {
    let mut w = csv::Writer::from_writer(Vec::new());
    w.write_record(result.columns.iter().map(|c| c.name.as_str()))?;
    for row in &result.rows {
        w.write_record(row.iter().map(|v| v.as_deref().unwrap_or("")))?;
    }
    Ok(String::from_utf8(w.into_inner()?)?)
}

/// Tab-separated rows (pasteable into spreadsheets). NULL becomes an empty cell.
pub fn to_tsv<'a>(rows: impl IntoIterator<Item = Vec<Option<&'a str>>>) -> String {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|v| v.unwrap_or("").replace(['\t', '\n', '\r'], " "))
                .collect::<Vec<_>>()
                .join("\t")
        })
        .collect::<Vec<_>>()
        .join("\n")
}
