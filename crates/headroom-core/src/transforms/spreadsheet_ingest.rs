//! Binary spreadsheet ingestion: `.xlsx` → tabular text.
//!
//! Rust port of `headroom/transforms/spreadsheet_ingest.py`. The compression
//! pipeline is text-only, so binary spreadsheets enter through this adapter at
//! the SDK boundary. Each sheet is rendered to CSV text, which then flows
//! through the normal tabular detection → SmartCrusher path like any other
//! table.
//!
//! # Parity notes (locked against the Python reference)
//!
//! - Python renders rows with stdlib `csv.writer` with `lineterminator="\n"`
//!   (upstream 10d8f15b; the old `\r\n` default plus `.strip("\n")` left a
//!   dangling `\r` on the final line) and minimal quoting (a field is quoted
//!   only when it contains a comma, quote, CR or LF), embedded `"` doubled.
//!   Fully-blank trailing rows are dropped before rendering (openpyxl's used
//!   range routinely extends past the last data row). Pinned constants below
//!   are verified against the fixed CPython output.
//! - Sheets whose rendered CSV is empty after a whitespace strip are excluded
//!   from the output entirely, and worksheet insertion order is preserved
//!   (Python returns a `dict`; we return `Vec<(name, csv)>`).
//! - openpyxl (`read_only=True, data_only=True`) yields cached formula VALUES,
//!   not formula strings — calamine does the same by default.

use std::path::{Path, PathBuf};

use calamine::{open_workbook, Data, Reader, Xlsx};

/// Errors mirroring the Python reference's exception surface
/// (`FileNotFoundError` / `ValueError` / parse failures).
#[derive(Debug, thiserror::Error)]
pub enum SpreadsheetError {
    /// Python: `FileNotFoundError(f"Spreadsheet not found: {p}")`.
    #[error("Spreadsheet not found: {}", .0.display())]
    NotFound(PathBuf),
    /// Python: `ValueError(f"Unsupported spreadsheet format '{suffix}'. ...")`.
    #[error("Unsupported spreadsheet format '{suffix}'. Supported: .xlsx")]
    UnsupportedFormat { suffix: String },
    /// The workbook exists and has a supported suffix but failed to parse.
    #[error("Failed to parse spreadsheet: {0}")]
    Parse(String),
}

/// Render one cell the way CPython renders the Python objects openpyxl
/// returns: `str(30)` → `"30"`, `str(2.5)` → `"2.5"`, `str(True)` → `"True"`,
/// `None` → `""`.
///
/// openpyxl hands integral numbers back as Python `int`, while calamine
/// reads xlsx numerics as `f64` — so integral floats are printed without the
/// fractional part to match. Non-integral floats use Rust's shortest
/// round-trip `Display`, which matches CPython `repr` for common values
/// (e.g. `0.1`, `2.5`); very large magnitudes diverge (Python switches to
/// `1e+16` notation, Rust does not) — accepted, undocumented territory for
/// spreadsheet cells.
///
/// DateTime cells: Python yields `str(datetime)` (e.g. `2024-01-02 03:04:05`);
/// chrono's `NaiveDateTime` Display matches for whole-second values but
/// diverges on fractional seconds (Python pads microseconds to 6 digits).
/// Documented divergence — the tests pin int/float/string/bool/None only.
fn cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            // i64 cast is exact for |f| < 2^53; beyond that xlsx has already
            // lost integer precision anyway.
            if f.is_finite() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::Bool(b) => (if *b { "True" } else { "False" }).to_string(),
        Data::DateTime(dt) => match dt.as_datetime() {
            Some(naive) => naive.to_string(),
            None => format!("{}", dt.as_f64()),
        },
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => e.to_string(),
    }
}

/// Quote a field per Python `csv.writer` QUOTE_MINIMAL: quote only when the
/// field contains the delimiter, the quote char, or a line-terminator char.
fn write_csv_field(out: &mut String, field: &str) {
    if field.contains([',', '"', '\r', '\n']) {
        out.push('"');
        for ch in field.chars() {
            if ch == '"' {
                out.push('"');
            }
            out.push(ch);
        }
        out.push('"');
    } else {
        out.push_str(field);
    }
}

/// A row is blank when every field is empty or whitespace-only. Operates on
/// the stringified row because `cell_to_string` already maps `Data::Empty`
/// (openpyxl `None`) to `""` — matching Python's `cell is None or
/// str(cell).strip() == ""` check.
fn is_blank_row(row: &[String]) -> bool {
    row.iter().all(|field| field.trim().is_empty())
}

/// Render rows to CSV text — Python `_rows_to_csv` semantics (upstream
/// 10d8f15b): drop fully-blank trailing rows, `\n` line terminator.
///
/// openpyxl's `iter_rows` walks the sheet's *used range*, which routinely
/// extends past the last data row (leftover formatting, a cleared cell), so
/// a real sheet commonly ends in `(None, None, ...)` tuples. Those used to
/// be written out as blank `,` rows.
///
/// `lineterminator="\n"` is load-bearing: `csv.writer` defaults to `\r\n`,
/// and the old `.strip("\n")` removed only the `\n`, leaving a dangling
/// `\r` on the final line — noise fed straight to the LLM.
fn rows_to_csv(rows: &[Vec<String>]) -> String {
    let mut last = rows.len();
    while last > 0 && is_blank_row(&rows[last - 1]) {
        last -= 1;
    }
    let mut buf = String::new();
    for row in &rows[..last] {
        for (i, field) in row.iter().enumerate() {
            if i > 0 {
                buf.push(',');
            }
            write_csv_field(&mut buf, field);
        }
        buf.push('\n');
    }
    buf.trim_matches('\n').to_string()
}

/// Load a spreadsheet file into `[(sheet_name, csv_text), ...]`.
///
/// Mirrors Python `load_spreadsheet(path) -> dict[str, str]`: empty sheets are
/// omitted and workbook sheet order is preserved.
///
/// Legacy `.xls` is DELIBERATELY not ported: the Python `_load_xls` path is
/// `pragma: no cover` (untested, needs optional xlrd + a binary fixture), so
/// there is no reference behavior to lock parity against. `.xls` returns an
/// unsupported-format error instead.
pub fn load_spreadsheet(path: &Path) -> Result<Vec<(String, String)>, SpreadsheetError> {
    if !path.exists() {
        return Err(SpreadsheetError::NotFound(path.to_path_buf()));
    }
    let suffix = path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
        .unwrap_or_default();
    if suffix != ".xlsx" {
        return Err(SpreadsheetError::UnsupportedFormat { suffix });
    }

    let mut wb =
        open_workbook::<Xlsx<_>, _>(path).map_err(|e| SpreadsheetError::Parse(e.to_string()))?;
    let mut sheets = Vec::new();
    for name in wb.sheet_names() {
        let range = wb
            .worksheet_range(&name)
            .map_err(|e| SpreadsheetError::Parse(e.to_string()))?;
        let rows: Vec<Vec<String>> = range
            .rows()
            .map(|row| row.iter().map(cell_to_string).collect())
            .collect();
        let text = rows_to_csv(&rows);
        if !text.trim().is_empty() {
            sheets.push((name, text));
        }
    }
    Ok(sheets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name)
    }

    /// Pinned CPython reference output for the "Types" sheet of
    /// `spreadsheet_sample.xlsx`, produced by running the Python
    /// `headroom.transforms.spreadsheet_ingest.load_spreadsheet` on the same
    /// fixture (see the fixture-generation script in the port notes). Locks:
    /// int→`30`, float→`2.5`/`0.1`, bool→`True`/`False`, None→``, comma/quote/
    /// newline quoting, doubled quotes, `\n` joins, no trailing `\r`.
    const TYPES_EXPECTED: &str = "int,float,text,bool,none,comma,quote,newline\n30,2.5,plain,True,,\"a,b\",\"say \"\"hi\"\"\",\"line1\nline2\"\n-7,0.1,trail ,False,,,'',end";

    /// The "Data" sheet is deterministic (header + 40 rows); build the pinned
    /// Python reference programmatically. Verified byte-equal against CPython
    /// output for the same fixture.
    fn data_expected() -> String {
        let mut s = String::from("id,name,dept,status");
        for i in 0..40 {
            let dept = ["eng", "sales", "ops"][i % 3];
            s.push_str(&format!("\n{i},user_{i},{dept},active"));
        }
        s
    }

    // Mirrors Python test_rows_to_csv_drops_trailing_empty_rows_and_has_no_dangling_cr
    // (upstream 10d8f15b): trailing all-empty rows are dropped, interior
    // empties preserved, no stray `\r` anywhere.
    #[test]
    fn test_rows_to_csv_drops_trailing_empty_rows_and_has_no_dangling_cr() {
        let rendered = rows_to_csv(&[
            vec!["Name".into(), "Age".into()],
            vec!["Alice".into(), "30".into()],
            vec!["".into(), "".into()],
            vec!["".into(), "  ".into()],
            vec!["".into(), "".into()],
        ]);
        assert_eq!(rendered, "Name,Age\nAlice,30");
        assert!(!rendered.contains('\r'));

        // Interior empty rows are preserved (only the trailing run is dropped).
        assert_eq!(
            rows_to_csv(&[
                vec!["a".into(), "b".into()],
                vec!["".into(), "".into()],
                vec!["c".into(), "d".into()],
                vec!["".into(), "".into()],
            ]),
            "a,b\n,\nc,d"
        );

        // A fully empty sheet renders to the empty string.
        assert_eq!(
            rows_to_csv(&[vec!["".into(), "".into()], vec!["".into(), "".into()]]),
            ""
        );
    }

    // Mirrors Python test_load_and_compress_xlsx (load half): "Empty" sheet
    // excluded, sheet order preserved, header row intact — plus byte-exact
    // comparison against the pinned Python reference output.
    #[test]
    fn test_load_xlsx_matches_python_reference() {
        let sheets = load_spreadsheet(&fixture("spreadsheet_sample.xlsx")).unwrap();
        let names: Vec<&str> = sheets.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Data", "Types"]); // "Empty" excluded

        let data = &sheets[0].1;
        assert_eq!(data.lines().next().unwrap(), "id,name,dept,status");
        assert_eq!(*data, data_expected());

        assert_eq!(sheets[1].1, TYPES_EXPECTED);
    }

    // Mirrors Python test_compress_spreadsheet_empty_workbook_returns_empty
    // (the load layer: an all-empty workbook yields an empty map).
    #[test]
    fn test_empty_workbook_returns_empty() {
        let sheets = load_spreadsheet(&fixture("spreadsheet_empty.xlsx")).unwrap();
        assert!(sheets.is_empty());
    }

    // Mirrors Python test_load_spreadsheet_rejects_unknown_extension.
    #[test]
    fn test_rejects_unknown_extension() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("data.txt");
        std::fs::write(&bad, "a,b\n1,2\n").unwrap();
        let err = load_spreadsheet(&bad).unwrap_err();
        assert!(matches!(
            &err,
            SpreadsheetError::UnsupportedFormat { suffix } if suffix == ".txt"
        ));
        assert!(err.to_string().starts_with("Unsupported"));
    }

    // Rust-specific: legacy .xls is deliberately unported (Python path was
    // pragma: no cover) — it must fail loudly, not silently mis-parse.
    #[test]
    fn test_xls_is_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let xls = dir.path().join("legacy.xls");
        std::fs::write(&xls, b"stub").unwrap();
        let err = load_spreadsheet(&xls).unwrap_err();
        assert!(matches!(
            err,
            SpreadsheetError::UnsupportedFormat { ref suffix } if suffix == ".xls"
        ));
    }

    // Mirrors Python test_load_spreadsheet_missing_file.
    #[test]
    fn test_missing_file() {
        let err = load_spreadsheet(Path::new("/nonexistent/nope.xlsx")).unwrap_err();
        assert!(matches!(err, SpreadsheetError::NotFound(_)));
        assert!(err.to_string().starts_with("Spreadsheet not found:"));
    }
}
