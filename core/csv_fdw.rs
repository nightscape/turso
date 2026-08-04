//! Built-in CSV foreign data wrapper for SQL/MED integration.
//!
//! Usage:
//! ```sql
//! CREATE SERVER csv_files OPTIONS (driver 'csv');
//! CREATE FOREIGN TABLE employees (name TEXT, age TEXT, city TEXT)
//!   SERVER csv_files OPTIONS (path 'employees.csv', skip_header 'true');
//! SELECT * FROM employees;
//! ```
//!
//! The optional `identity` option names the columns that uniquely identify a
//! row (comma-separated), which opts matviews over the table into incremental
//! maintenance:
//!
//! ```sql
//! CREATE FOREIGN TABLE messages (uuid TEXT, body TEXT)
//!   SERVER csv_files OPTIONS (path 'msgs.csv', identity 'uuid');
//! ```

use std::collections::HashMap;

use crate::foreign::{
    ForeignColumnDef, ForeignCursor, ForeignDataWrapper, ForeignDriverFactory, KeyColumn,
    PushedConstraint,
};
use crate::sync::Arc;
use crate::{Connection, Result, Value};

#[derive(Debug)]
pub struct CsvDriverFactory;

impl ForeignDriverFactory for CsvDriverFactory {
    fn create_fdw(
        &self,
        _server_options: &HashMap<String, String>,
        table_options: &HashMap<String, String>,
        columns: &[ForeignColumnDef],
    ) -> Result<Arc<dyn ForeignDataWrapper>> {
        let path = table_options.get("path").ok_or_else(|| {
            crate::LimboError::ParseError("CSV foreign table requires 'path' option".to_string())
        })?;
        let skip_header = table_options
            .get("skip_header")
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1" || v.eq_ignore_ascii_case("yes"))
            .unwrap_or(false);
        let col_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let col_types: Vec<String> = columns.iter().map(|c| c.type_name.clone()).collect();
        let identity = table_options
            .get("identity")
            .map(|spec| resolve_identity(spec, &col_names))
            .transpose()?;
        Ok(Arc::new(CsvFdw {
            path: path.clone(),
            skip_header,
            col_names,
            col_types,
            identity,
        }))
    }
}

/// Resolve a comma-separated `identity` option into column indices.
///
/// Rejects unknown and repeated column names: both would otherwise surface much
/// later as a malformed unique index over the view's mirror.
fn resolve_identity(spec: &str, col_names: &[String]) -> Result<Vec<u32>> {
    if spec.trim().is_empty() {
        return Err(crate::LimboError::ParseError(
            "CSV foreign table 'identity' option must name at least one column".to_string(),
        ));
    }
    let mut indices = Vec::new();
    for name in spec.split(',') {
        let name = name.trim();
        let unquoted = unquote_ident(name);
        let idx = col_names
            .iter()
            .position(|c| c.eq_ignore_ascii_case(&unquoted))
            .ok_or_else(|| {
                crate::LimboError::ParseError(format!(
                    "CSV foreign table 'identity' option names unknown column '{name}'"
                ))
            })? as u32;
        if indices.contains(&idx) {
            return Err(crate::LimboError::ParseError(format!(
                "CSV foreign table 'identity' option repeats column '{name}'"
            )));
        }
        indices.push(idx);
    }
    Ok(indices)
}

/// Strip one balanced identifier quote pair, undoubling the inner delimiter.
///
/// Columns whose names require quoting are addressable in `identity` only by
/// their quoted spelling. An unbalanced or absent pair is returned as-is and
/// then fails the column lookup, which reports the spec the user wrote.
///
/// The pair is established from the end *bytes* before anything is sliced:
/// an ASCII delimiter at both ends is what makes indices 1 and `len - 1` char
/// boundaries, so a non-ASCII name like `über` is only ever returned whole.
fn unquote_ident(spec: &str) -> String {
    let bytes = spec.as_bytes();
    if bytes.len() < 2 {
        return spec.to_string();
    }
    let inner = || &spec[1..spec.len() - 1];
    match (bytes[0], bytes[bytes.len() - 1]) {
        (b'"', b'"') => inner().replace("\"\"", "\""),
        (b'`', b'`') => inner().replace("``", "`"),
        (b'[', b']') => inner().to_string(),
        _ => spec.to_string(),
    }
}

#[derive(Debug)]
struct CsvFdw {
    path: String,
    skip_header: bool,
    col_names: Vec<String>,
    col_types: Vec<String>,
    identity: Option<Vec<u32>>,
}

impl ForeignDataWrapper for CsvFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &[]
    }

    fn identity_columns(&self) -> Option<&[u32]> {
        self.identity.as_deref()
    }

    fn schema_sql(&self) -> String {
        let cols: Vec<String> = self
            .col_names
            .iter()
            .zip(self.col_types.iter())
            .map(|(name, ty)| {
                let ident = turso_parser::ast::Name::exact(name.clone()).as_ident();
                format!("{ident} {ty}")
            })
            .collect();
        format!("CREATE TABLE csv_data({})", cols.join(", "))
    }

    fn open_cursor(&self, _conn: Arc<Connection>) -> Result<Box<dyn ForeignCursor>> {
        let content = std::fs::read_to_string(&self.path).map_err(|e| {
            crate::LimboError::InternalError(format!(
                "Failed to read CSV file '{}': {e}",
                self.path
            ))
        })?;
        let rows = parse_csv(&content, self.skip_header, self.col_names.len());
        Ok(Box::new(CsvCursor {
            rows,
            index: 0,
            started: false,
        }))
    }
}

fn parse_csv(content: &str, skip_header: bool, num_columns: usize) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut lines = content.lines();
    if skip_header {
        lines.next();
    }
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let fields = parse_csv_line(line);
        // Pad or truncate to match expected column count
        let mut row = Vec::with_capacity(num_columns);
        for i in 0..num_columns {
            row.push(fields.get(i).cloned().unwrap_or_default());
        }
        rows.push(row);
    }
    rows
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    current.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                current.push(ch);
            }
        } else {
            match ch {
                ',' => {
                    fields.push(current.trim().to_string());
                    current = String::new();
                }
                '"' => {
                    in_quotes = true;
                }
                _ => {
                    current.push(ch);
                }
            }
        }
    }
    fields.push(current.trim().to_string());
    fields
}

struct CsvCursor {
    rows: Vec<Vec<String>>,
    index: usize,
    started: bool,
}

// SAFETY: CsvCursor only contains owned data.
unsafe impl Send for CsvCursor {}
unsafe impl Sync for CsvCursor {}

impl ForeignCursor for CsvCursor {
    fn filter(&mut self, _constraints: &[PushedConstraint]) -> Result<bool> {
        self.index = 0;
        self.started = true;
        Ok(!self.rows.is_empty())
    }

    fn next(&mut self) -> Result<bool> {
        if !self.started {
            return Ok(false);
        }
        self.index += 1;
        Ok(self.index < self.rows.len())
    }

    fn column(&self, idx: usize) -> Result<Value> {
        let row = &self.rows[self.index];
        if idx < row.len() {
            Ok(Value::build_text(row[idx].clone()))
        } else {
            Ok(Value::Null)
        }
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_csv_line_simple() {
        let fields = parse_csv_line("alice,30,berlin");
        assert_eq!(fields, vec!["alice", "30", "berlin"]);
    }

    #[test]
    fn test_parse_csv_line_quoted() {
        let fields = parse_csv_line("\"Alice Smith\",30,\"New York\"");
        assert_eq!(fields, vec!["Alice Smith", "30", "New York"]);
    }

    #[test]
    fn test_parse_csv_line_escaped_quotes() {
        let fields = parse_csv_line("\"He said \"\"hello\"\"\",30");
        assert_eq!(fields, vec!["He said \"hello\"", "30"]);
    }

    #[test]
    fn test_parse_csv_with_header() {
        let content = "name,age,city\nalice,30,berlin\nbob,25,munich";
        let rows = parse_csv(content, true, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["alice", "30", "berlin"]);
        assert_eq!(rows[1], vec!["bob", "25", "munich"]);
    }

    /// Build a CSV FDW over columns `uuid, session_id, body` with the given
    /// extra table options on top of a dummy `path`.
    fn fdw_with_options(extra: &[(&str, &str)]) -> Result<Arc<dyn ForeignDataWrapper>> {
        let mut opts = HashMap::from([("path".to_string(), "/nonexistent.csv".to_string())]);
        for (k, v) in extra {
            opts.insert(k.to_string(), v.to_string());
        }
        let columns: Vec<ForeignColumnDef> = ["uuid", "session_id", "body"]
            .iter()
            .map(|n| ForeignColumnDef {
                name: n.to_string(),
                type_name: "TEXT".to_string(),
            })
            .collect();
        CsvDriverFactory.create_fdw(&HashMap::new(), &opts, &columns)
    }

    #[test]
    fn identity_absent_declares_no_identity() {
        let fdw = fdw_with_options(&[]).unwrap();
        assert_eq!(fdw.identity_columns(), None);
    }

    #[test]
    fn identity_resolves_single_column() {
        let fdw = fdw_with_options(&[("identity", "uuid")]).unwrap();
        assert_eq!(fdw.identity_columns(), Some(&[0u32][..]));
    }

    /// Composite identity, tolerating whitespace and matching case-insensitively
    /// the way the rest of SQL identifier handling does.
    #[test]
    fn identity_resolves_composite_column_list() {
        let fdw = fdw_with_options(&[("identity", " SESSION_ID , uuid ")]).unwrap();
        assert_eq!(fdw.identity_columns(), Some(&[1u32, 0u32][..]));
    }

    #[test]
    fn identity_rejects_unknown_column() {
        let err = fdw_with_options(&[("identity", "nope")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown column"), "unexpected error: {err}");
        assert!(err.contains("nope"), "error should name the column: {err}");
    }

    #[test]
    fn identity_rejects_repeated_column() {
        let err = fdw_with_options(&[("identity", "uuid,uuid")])
            .unwrap_err()
            .to_string();
        assert!(err.contains("repeats column"), "unexpected error: {err}");
    }

    #[test]
    fn identity_rejects_empty_spec() {
        let err = fdw_with_options(&[("identity", "")])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("at least one column"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_csv_driver_factory_missing_path() {
        let factory = CsvDriverFactory;
        let result = factory.create_fdw(
            &HashMap::new(),
            &HashMap::new(),
            &[ForeignColumnDef {
                name: "a".to_string(),
                type_name: "TEXT".to_string(),
            }],
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path"));
    }
}
