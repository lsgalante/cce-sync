//! Workbooks: the vault's `.xlsx` files, as the MCP tools read and write
//! them, on IronCalc (the engine cce-spreadsheet draws). Like tables they
//! are attachments, so the tools write them around the note index, one
//! `Put` conditional on the rev they read.
//!
//! IronCalc keeps cells, formulas, styles, merged cells, column widths and
//! conditional formatting, but not every part of a workbook: saving one
//! that has charts, pictures, pivot tables, Excel tables, comments, macros,
//! data validation or hyperlinks would drop them. [`unkept`] names what a
//! save would lose, and the tools refuse to write such a workbook (reading
//! it is fine).
//!
//! Formulas recalculate on the server with the clock in UTC; `TODAY()` and
//! `NOW()` near midnight can be a day off the user's.

use std::io::{Cursor, Read};

use ironcalc::base::Model;

const LOCALE: &str = "en";
const TZ: &str = "UTC";
const LANGUAGE: &str = "en";

/// Excel's limits.
const MAX_ROW: i32 = 1_048_576;
const MAX_COLUMN: i32 = 16_384;

/// The most cells [`render`] shows without a range.
pub const MAX_CELLS: usize = 4000;

/// Whether a path is a workbook the tools handle.
pub fn is_workbook(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(".xlsx")
}

pub fn load(bytes: &[u8]) -> Result<Model<'static>, String> {
    let wb = ironcalc::import::load_from_xlsx_bytes(bytes, "Book", LOCALE, TZ).map_err(|e| format!("not a workbook IronCalc can read: {e}"))?;
    let mut model = Model::from_workbook(wb, LANGUAGE)?;
    model.evaluate();
    Ok(model)
}

pub fn save(model: &Model) -> Result<Vec<u8>, String> {
    let out = ironcalc::export::save_xlsx_to_writer(model, Cursor::new(Vec::new())).map_err(|e| format!("saving failed: {e}"))?;
    Ok(out.into_inner())
}

/// A new workbook with these sheets (name, rows of cell inputs), at least one.
pub fn create(sheets: &[(String, Vec<Vec<String>>)]) -> Result<Model<'static>, String> {
    if sheets.is_empty() {
        return Err("a workbook needs at least one sheet".into());
    }
    let mut model = Model::new_empty("Book", LOCALE, TZ, LANGUAGE)?;
    for (i, (name, rows)) in sheets.iter().enumerate() {
        if i == 0 {
            model.rename_sheet_by_index(0, name)?;
        } else {
            model.add_sheet(name)?;
        }
        for (r, row) in rows.iter().enumerate() {
            for (c, input) in row.iter().enumerate() {
                if !input.is_empty() {
                    model.set_user_input(i as u32, r as i32 + 1, c as i32 + 1, input.clone())?;
                }
            }
        }
    }
    model.evaluate();
    Ok(model)
}

/// What saving this workbook through IronCalc would drop, in words; empty
/// when nothing would be lost.
pub fn unkept(bytes: &[u8]) -> Result<Vec<&'static str>, String> {
    const PARTS: &[(&str, &str)] = &[
        ("xl/charts/", "charts"),
        ("xl/drawings/", "drawings or pictures"),
        ("xl/media/", "pictures"),
        ("xl/pivotTables/", "pivot tables"),
        ("xl/pivotCache/", "pivot tables"),
        ("xl/tables/", "Excel tables"),
        ("xl/comments", "comments"),
        ("xl/threadedComments/", "comments"),
        ("xl/vbaProject.bin", "macros"),
        ("xl/externalLinks/", "links to other workbooks"),
        ("xl/slicers/", "slicers"),
        ("xl/timelines/", "timelines"),
    ];
    const TAGS: &[(&str, &str)] = &[("<dataValidations", "data validation"), ("<hyperlinks", "hyperlinks")];
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("not a workbook: {e}"))?;
    let mut found = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = f.name().to_string();
        for (prefix, what) in PARTS {
            if name.starts_with(prefix) && !found.contains(what) {
                found.push(*what);
            }
        }
        if name.starts_with("xl/worksheets/") && name.ends_with(".xml") {
            let mut xml = String::new();
            f.read_to_string(&mut xml).map_err(|e| e.to_string())?;
            for (tag, what) in TAGS {
                if xml.contains(tag) && !found.contains(what) {
                    found.push(*what);
                }
            }
        }
    }
    Ok(found)
}

/// The sheet called `name` (any case), or the first sheet.
pub fn sheet(model: &Model, name: Option<&str>) -> Result<u32, String> {
    let names = sheet_names(model);
    match name {
        None => Ok(0),
        Some(n) => names
            .iter()
            .position(|s| s.eq_ignore_ascii_case(n.trim()))
            .map(|i| i as u32)
            .ok_or_else(|| format!("no sheet called {n}; the sheets are {}", names.join(", "))),
    }
}

pub fn sheet_names(model: &Model) -> Vec<String> {
    model.get_worksheets_properties().into_iter().map(|p| p.name).collect()
}

/// A column's letters, from 1 (`A`).
pub fn column_name(mut col: i32) -> String {
    let mut s = Vec::new();
    while col > 0 {
        let r = (col - 1) % 26;
        s.push(b'A' + r as u8);
        col = (col - 1) / 26;
    }
    s.reverse();
    String::from_utf8(s).expect("ascii")
}

/// `B4` (also `$B$4`) as (row, column), from 1.
pub fn cell(addr: &str) -> Result<(i32, i32), String> {
    let a = addr.trim().replace('$', "").to_ascii_uppercase();
    let split = a.find(|c: char| c.is_ascii_digit()).ok_or_else(|| format!("{addr} is not a cell like B4"))?;
    let (letters, digits) = a.split_at(split);
    if letters.is_empty() || !letters.bytes().all(|b| b.is_ascii_uppercase()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{addr} is not a cell like B4"));
    }
    let col = letters.bytes().try_fold(0i32, |n, b| n.checked_mul(26).and_then(|n| n.checked_add(i32::from(b - b'A') + 1)));
    let row: i32 = digits.parse().map_err(|_| format!("{addr} is not a cell like B4"))?;
    match col {
        Some(col) if (1..=MAX_COLUMN).contains(&col) && (1..=MAX_ROW).contains(&row) => Ok((row, col)),
        _ => Err(format!("{addr} is outside the sheet")),
    }
}

/// `A1:D20` (or one cell) as (top, left, bottom, right).
pub fn range(r: &str) -> Result<(i32, i32, i32, i32), String> {
    let (a, b) = r.split_once(':').unwrap_or((r, r));
    let ((r1, c1), (r2, c2)) = (cell(a)?, cell(b)?);
    Ok((r1.min(r2), c1.min(c2), r1.max(r2), c1.max(c2)))
}

/// The used part of a sheet, or `None` when it is empty.
pub fn used(model: &Model, sheet: u32) -> Result<Option<(i32, i32, i32, i32)>, String> {
    let ws = model.workbook.worksheet(sheet)?;
    if ws.sheet_data.is_empty() {
        return Ok(None);
    }
    let d = ws.dimension();
    Ok(Some((d.min_row, d.min_column, d.max_row, d.max_column)))
}

fn shown(s: &str) -> String {
    s.replace('\t', " ").replace("\r\n", "⏎").replace('\n', "⏎")
}

/// A sheet's cells in `area` (default: what is used) as a tab-separated grid
/// of displayed values, column letters across the top and row numbers down
/// the side, then the formulas in it. A grid over [`MAX_CELLS`] is cut at a
/// whole row and says so.
pub fn render(model: &Model, sheet: u32, area: Option<(i32, i32, i32, i32)>) -> Result<String, String> {
    let Some((top, left, bottom, right)) = area.map_or_else(|| used(model, sheet), |a| Ok(Some(a)))? else {
        return Ok("(the sheet is empty)".into());
    };
    let width = (right - left + 1) as usize;
    let last = if area.is_none() && width * (bottom - top + 1) as usize > MAX_CELLS {
        top + (MAX_CELLS / width).max(1) as i32 - 1
    } else {
        bottom
    };
    let mut out = vec![std::iter::once(String::new()).chain((left..=right).map(column_name)).collect::<Vec<_>>().join("\t")];
    let mut formulas = Vec::new();
    for row in top..=last {
        let mut line = vec![row.to_string()];
        for col in left..=right {
            line.push(shown(&model.get_formatted_cell_value(sheet, row, col)?));
            if let Some(f) = model.get_cell_formula(sheet, row, col)? {
                formulas.push(format!("{}{row} {f}", column_name(col)));
            }
        }
        out.push(line.join("\t"));
    }
    if last < bottom {
        out.push(format!(
            "(rows {top}-{last} of {top}-{bottom}; pass a range such as {}{}:{}{} for more)",
            column_name(left),
            last + 1,
            column_name(right),
            bottom.min(last + (last - top + 1))
        ));
    }
    if !formulas.is_empty() {
        out.push(String::new());
        out.push("formulas:".into());
        out.extend(formulas);
    }
    Ok(out.join("\n"))
}

/// Set cells from inputs as typed into the sheet (`=` starts a formula; an
/// empty input clears the cell, keeping its style), then recalculate.
pub fn set(model: &mut Model, sheet: u32, cells: &[(String, String)]) -> Result<(), String> {
    for (addr, input) in cells {
        let (row, col) = cell(addr)?;
        if input.is_empty() {
            model.workbook.worksheet_mut(sheet)?.cell_clear_contents(row, col)?;
        } else {
            model.set_user_input(sheet, row, col, input.clone()).map_err(|e| format!("{addr}: {e}"))?;
        }
    }
    model.evaluate();
    Ok(())
}

/// Add rows under the used part of a sheet, starting at its first used
/// column (column A on an empty sheet); returns the first new row.
pub fn append(model: &mut Model, sheet: u32, rows: &[Vec<String>]) -> Result<i32, String> {
    let (first, left) = used(model, sheet)?.map_or((1, 1), |(_, left, bottom, _)| (bottom + 1, left));
    if first + rows.len() as i32 - 1 > MAX_ROW {
        return Err("the rows would run past the end of the sheet".into());
    }
    for (r, row) in rows.iter().enumerate() {
        for (c, input) in row.iter().enumerate() {
            if !input.is_empty() {
                let (row_n, col_n) = (first + r as i32, left + c as i32);
                model.set_user_input(sheet, row_n, col_n, input.clone()).map_err(|e| format!("{}{row_n}: {e}", column_name(col_n)))?;
            }
        }
    }
    model.evaluate();
    Ok(first)
}

/// What a cell shows now, with its formula: `B4 = 1,200 (=SUM(B1:B3))`.
pub fn describe(model: &Model, sheet: u32, row: i32, col: i32) -> Result<String, String> {
    let v = shown(&model.get_formatted_cell_value(sheet, row, col)?);
    Ok(match model.get_cell_formula(sheet, row, col)? {
        Some(f) => format!("{}{row} = {v} ({f})", column_name(col)),
        None => format!("{}{row} = {v}", column_name(col)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(r: &[&[&str]]) -> Vec<Vec<String>> {
        r.iter().map(|row| row.iter().map(|s| s.to_string()).collect()).collect()
    }

    #[test]
    fn addresses() {
        assert_eq!(cell("B4"), Ok((4, 2)));
        assert_eq!(cell("$aa$10"), Ok((10, 27)));
        assert_eq!(column_name(27), "AA");
        assert_eq!(column_name(16384), "XFD");
        assert!(cell("XFE1").is_err() && cell("A0").is_err() && cell("4B").is_err() && cell("").is_err());
        assert_eq!(range("D20:A1"), Ok((1, 1, 20, 4)));
        assert_eq!(range("C3"), Ok((3, 3, 3, 3)));
    }

    #[test]
    fn create_save_load_round_trip_with_formulas() {
        let m = create(&[
            ("Budget".into(), rows(&[&["Item", "Cost"], &["Rent", "1200"], &["Food", "300"], &["Total", "=SUM(B2:B3)"]])),
            ("Notes".into(), rows(&[&["hello"]])),
        ])
        .unwrap();
        let bytes = save(&m).unwrap();
        assert!(unkept(&bytes).unwrap().is_empty());
        let mut m = load(&bytes).unwrap();
        assert_eq!(sheet_names(&m), vec!["Budget", "Notes"]);
        assert_eq!(sheet(&m, Some("notes")), Ok(1));
        assert!(sheet(&m, Some("Nope")).unwrap_err().contains("Budget, Notes"));
        assert_eq!(describe(&m, 0, 4, 2).unwrap(), "B4 = 1500 (=SUM(B2:B3))");
        let first = append(&mut m, 0, &rows(&[&["Gas", "40"]])).unwrap();
        assert_eq!(first, 5);
        set(&mut m, 0, &[("B4".into(), "=SUM(B2:B3,B5)".into()), ("A1".into(), String::new())]).unwrap();
        let m = load(&save(&m).unwrap()).unwrap();
        assert_eq!(describe(&m, 0, 4, 2).unwrap(), "B4 = 1540 (=SUM(B2:B3,B5))");
        let grid = render(&m, 0, None).unwrap();
        assert!(grid.starts_with("\tA\tB\n1\t\tCost\n2\tRent\t1200\n"), "{grid}");
        assert!(grid.ends_with("formulas:\nB4 =SUM(B2:B3,B5)"), "{grid}");
    }

    #[test]
    fn a_long_sheet_is_cut_at_a_whole_row() {
        let many: Vec<Vec<String>> = (0..3000).map(|i| vec![i.to_string(), "x".into()]).collect();
        let m = create(&[("S".into(), many)]).unwrap();
        let grid = render(&m, 0, None).unwrap();
        assert!(grid.contains("\n2000\t1999\tx\n(rows 1-2000 of 1-3000; pass a range such as A2001:B3000 for more)"), "{grid}");
        assert!(render(&m, 0, Some((2999, 1, 3000, 2))).unwrap().ends_with("2999\t2998\tx\n3000\t2999\tx"));
        assert_eq!(render(&create(&[("E".into(), vec![])]).unwrap(), 0, None).unwrap(), "(the sheet is empty)");
    }

    #[test]
    fn unkept_names_what_a_save_would_drop() {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let opts = zip::write::FileOptions::default();
        zip.start_file("xl/charts/chart1.xml", opts).unwrap();
        zip.start_file("xl/worksheets/sheet1.xml", opts).unwrap();
        zip.write_all(b"<worksheet><hyperlinks/></worksheet>").unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        assert_eq!(unkept(&bytes).unwrap(), vec!["charts", "hyperlinks"]);
    }
}
