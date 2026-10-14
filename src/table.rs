//! Tables: the vault's `.csv` and `.tsv` files, as the MCP tools read and
//! write them. They sync as attachments (no line merge: a clash leaves a
//! conflict copy), so Claude's writes to them go straight to the store with
//! a revision precondition rather than through the note index.
//!
//! The reader is RFC 4180's: fields split by the delimiter, a field that
//! starts with `"` runs to the closing quote (newlines and delimiters
//! inside it included, `""` an escaped quote), records end at `\n` or
//! `\r\n`. Blank lines are skipped. A table is well formed when every
//! record has as many fields as the header; Claude's writes must be.

/// The delimiter of a table path (`,` for `.csv`, tab for `.tsv`), or
/// `None` for anything else.
pub fn delimiter(path: &str) -> Option<char> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".csv") {
        Some(',')
    } else if lower.ends_with(".tsv") {
        Some('\t')
    } else {
        None
    }
}

/// One record: its bytes in the text (without the line ending), how many
/// fields it has, and the line it starts on (from 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub start: usize,
    pub end: usize,
    pub fields: usize,
    pub line: usize,
}

/// The records of a table's text, blank lines skipped.
pub fn records(text: &str, delim: char) -> Result<Vec<Record>, String> {
    let mut out = Vec::new();
    let (mut start, mut fields, mut line, mut rec_line) = (0, 1, 1, 1);
    let (mut quoted, mut field_start) = (false, true);
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek().is_some_and(|&(_, n)| n == '"') => {
                    chars.next();
                }
                '"' => quoted = false,
                '\n' => line += 1,
                _ => {}
            }
            continue;
        }
        match c {
            '"' if field_start => {
                quoted = true;
                field_start = false;
            }
            '\n' => {
                let end = if text[..i].ends_with('\r') { i - 1 } else { i };
                if end > start {
                    out.push(Record { start, end, fields, line: rec_line });
                }
                line += 1;
                (start, fields, rec_line, field_start) = (i + 1, 1, line, true);
            }
            c if c == delim => {
                fields += 1;
                field_start = true;
            }
            _ => field_start = false,
        }
    }
    if quoted {
        return Err(format!("a quoted field opened on line {rec_line} never closes"));
    }
    let end = text.len() - usize::from(text.ends_with('\r'));
    if end > start {
        out.push(Record { start, end, fields, line: rec_line });
    }
    Ok(out)
}

/// The records of a table Claude is writing, refused unless it has a
/// header and every record has the header's number of fields.
pub fn check(text: &str, delim: char) -> Result<Vec<Record>, String> {
    let recs = records(text, delim)?;
    let Some(header) = recs.first() else {
        return Err("a table needs at least a header row".into());
    };
    if let Some((n, r)) = recs.iter().enumerate().skip(1).find(|(_, r)| r.fields != header.fields) {
        return Err(format!("row {n} (line {}) has {} fields; the header has {}", r.line, r.fields, header.fields));
    }
    Ok(recs)
}

/// `text` with `rows` added after its last record, on a line of their own.
pub fn appended(text: &str, rows: &str) -> String {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(if out.contains("\r\n") { "\r\n" } else { "\n" });
    }
    out.push_str(rows);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(text: &str) -> Vec<(usize, usize)> {
        records(text, ',').unwrap().iter().map(|r| (r.fields, r.line)).collect()
    }

    #[test]
    fn quoted_fields_hold_delimiters_quotes_and_newlines() {
        let t = "a,b,c\n\"x, y\",\"say \"\"hi\"\"\",\"two\nlines\"\nd,e,f\n";
        assert_eq!(fields(t), vec![(3, 1), (3, 2), (3, 4)]);
        let r = records(t, ',').unwrap();
        assert_eq!(&t[r[2].start..r[2].end], "d,e,f");
    }

    #[test]
    fn crlf_blank_lines_and_no_final_newline() {
        let t = "a,b\r\n\r\n1,2\r\n3,4";
        let r = records(t, ',').unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(&t[r[1].start..r[1].end], "1,2");
        assert_eq!(&t[r[2].start..r[2].end], "3,4");
    }

    #[test]
    fn a_quote_inside_a_field_is_text() {
        assert_eq!(fields("a,b\n5\" pipe,x\n"), vec![(2, 1), (2, 2)]);
    }

    #[test]
    fn check_refuses_ragged_rows_and_open_quotes() {
        assert!(check("a,b,c\n1,2,3\n", ',').is_ok());
        assert_eq!(check("a,b,c\n1,2,3\n4,5\n", ',').unwrap_err(), "row 2 (line 3) has 2 fields; the header has 3");
        assert!(check("a,b\n\"1,2\n", ',').unwrap_err().contains("never closes"));
        assert!(check("\n\n", ',').is_err());
        assert!(check("a\tb\n1\t2\n", '\t').is_ok());
    }

    #[test]
    fn append_keeps_one_row_per_line() {
        assert_eq!(appended("a,b\n1,2", "3,4"), "a,b\n1,2\n3,4\n");
        assert_eq!(appended("a,b\r\n1,2", "3,4\n"), "a,b\r\n1,2\r\n3,4\n");
        assert_eq!(appended("a,b\n", "3,4\n5,6"), "a,b\n3,4\n5,6\n");
    }

    #[test]
    fn delimiter_by_extension() {
        assert_eq!(delimiter("Finance/Ledger.CSV"), Some(','));
        assert_eq!(delimiter("x.tsv"), Some('\t'));
        assert_eq!(delimiter("x.md"), None);
    }
}
