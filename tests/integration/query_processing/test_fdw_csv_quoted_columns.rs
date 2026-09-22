//! Column names that need quoting must survive the CSV driver's own
//! `CREATE TABLE` round-trip: the driver generates a schema string that
//! `VirtualTable::resolve_columns` re-parses, so an unquoted reserved word
//! fails the re-parse and an unquoted spaced name re-parses into a *different*
//! column (`My Col TEXT` reads as column `My` of type `Col TEXT`).

use crate::common::{self, ExecRows, TempDatabase};
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// `id,order,My Col` over two rows; the header is skipped, so the CSV's own
/// header text never feeds column naming — the DDL does.
fn write_csv(dir: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path)?;
    writeln!(f, "id,order,My Col")?;
    writeln!(f, "1,10,alpha")?;
    writeln!(f, "2,20,beta")?;
    Ok(path)
}

const COLS: &str = r#"("id" TEXT, "order" TEXT, "My Col" TEXT)"#;
const UMLAUT_COLS: &str = r#"("über" TEXT, "id" TEXT)"#;

#[turso_macros::test]
fn test_csv_fdw_quoted_reserved_and_spaced_columns(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "quoted_cols.csv")?;

    conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")?;
    conn.execute(&format!(
        "CREATE FOREIGN TABLE ft {COLS} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true')",
        csv_path.display()
    ))?;

    let cols: Vec<(String, String)> =
        conn.exec_rows("SELECT name, type FROM pragma_table_info('ft')");
    assert_eq!(
        cols,
        vec![
            ("id".to_string(), "TEXT".to_string()),
            ("order".to_string(), "TEXT".to_string()),
            ("My Col".to_string(), "TEXT".to_string()),
        ],
        "declared columns must reach the vtab intact"
    );

    let rows: Vec<(String, String, String)> =
        conn.exec_rows(r#"SELECT "id", "order", "My Col" FROM ft"#);
    assert_eq!(
        rows,
        vec![
            ("1".to_string(), "10".to_string(), "alpha".to_string()),
            ("2".to_string(), "20".to_string(), "beta".to_string()),
        ],
        "quoted columns must read data, not the double-quote string fallback"
    );

    Ok(())
}

/// A spaced name alone does not fail the re-parse — it succeeds as a
/// *different* column, so `sqlite_master` and the in-memory table disagree.
#[turso_macros::test]
fn test_csv_fdw_spaced_column_is_not_silently_renamed(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "spaced_only.csv")?;

    conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")?;
    let cols_ddl = r#"("id" TEXT, "My Col" TEXT)"#;
    conn.execute(&format!(
        "CREATE FOREIGN TABLE ft {cols_ddl} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true')",
        csv_path.display()
    ))?;

    let cols: Vec<(String, String)> =
        conn.exec_rows("SELECT name, type FROM pragma_table_info('ft')");
    assert_eq!(
        cols,
        vec![
            ("id".to_string(), "TEXT".to_string()),
            ("My Col".to_string(), "TEXT".to_string()),
        ],
        "spaced name must not split into column `My` of type `Col TEXT`"
    );

    let vals: Vec<(String,)> = conn.exec_rows(r#"SELECT "My Col" FROM ft"#);
    assert_eq!(
        vals,
        vec![("10".to_string(),), ("20".to_string(),)],
        "must return data, not the literal 'My Col' via the identifier fallback"
    );

    Ok(())
}

/// A column name that itself contains a double quote round-trips only if the
/// generated schema re-escapes it.
#[turso_macros::test]
fn test_csv_fdw_column_name_containing_quote(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "quote_in_name.csv")?;

    conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")?;
    let cols = r#"("id" TEXT, "sa""y" TEXT, "c" TEXT)"#;
    conn.execute(&format!(
        "CREATE FOREIGN TABLE ft {cols} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true')",
        csv_path.display()
    ))?;

    let names: Vec<(String,)> = conn.exec_rows("SELECT name FROM pragma_table_info('ft')");
    assert_eq!(
        names,
        vec![
            ("id".to_string(),),
            ("sa\"y".to_string(),),
            ("c".to_string(),),
        ]
    );

    let vals: Vec<(String,)> = conn.exec_rows(r#"SELECT "sa""y" FROM ft"#);
    assert_eq!(vals, vec![("10".to_string(),), ("20".to_string(),)]);

    Ok(())
}

/// The `identity` option must be able to address a column whose name requires
/// quoting — otherwise the quoting fix creates columns no matview can key on.
#[turso_macros::test]
fn test_csv_fdw_identity_over_quoted_column(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "identity_quoted.csv")?;

    conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")?;
    conn.execute(&format!(
        "CREATE FOREIGN TABLE ft {COLS} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true', identity '\"My Col\"')",
        csv_path.display()
    ))?;

    let rows: Vec<(String,)> = conn.exec_rows(r#"SELECT "My Col" FROM ft"#);
    assert_eq!(rows, vec![("alpha".to_string(),), ("beta".to_string(),)]);

    Ok(())
}

/// Non-ASCII column names are ordinary identifiers: an unquoted `identity`
/// spec must resolve against them by the plain case-insensitive match.
#[turso_macros::test]
fn test_csv_fdw_identity_unquoted_unicode_column(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "identity_unicode.csv")?;

    conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")?;
    conn.execute(&format!(
        "CREATE FOREIGN TABLE ft {UMLAUT_COLS} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true', identity 'über')",
        csv_path.display()
    ))?;

    let rows: Vec<(String,)> = conn.exec_rows(r#"SELECT "über" FROM ft"#);
    assert_eq!(rows, vec![("1".to_string(),), ("2".to_string(),)]);

    Ok(())
}

/// The quoted spelling of the same name must keep resolving.
#[turso_macros::test]
fn test_csv_fdw_identity_quoted_unicode_column(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "identity_unicode_q.csv")?;

    conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")?;
    let ident = r#"'"über"'"#;
    conn.execute(&format!(
        "CREATE FOREIGN TABLE ft {UMLAUT_COLS} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true', identity {ident})",
        csv_path.display()
    ))?;

    let rows: Vec<(String,)> = conn.exec_rows(r#"SELECT "über" FROM ft"#);
    assert_eq!(rows, vec![("1".to_string(),), ("2".to_string(),)]);

    Ok(())
}

/// A name that is a single multi-byte character: both the leading and the
/// trailing byte index fall inside it.
#[turso_macros::test]
fn test_csv_fdw_identity_single_multibyte_column(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "identity_one_char.csv")?;

    conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")?;
    let cols = r#"("ü" TEXT, "id" TEXT)"#;
    conn.execute(&format!(
        "CREATE FOREIGN TABLE ft {cols} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true', identity 'ü')",
        csv_path.display()
    ))?;

    let rows: Vec<(String,)> = conn.exec_rows(r#"SELECT "ü" FROM ft"#);
    assert_eq!(rows, vec![("1".to_string(),), ("2".to_string(),)]);

    Ok(())
}

/// An unbalanced quote around a multi-byte spec is a user error, and must
/// surface as the ordinary unknown-column error rather than a panic.
#[turso_macros::test]
fn test_csv_fdw_identity_unbalanced_quote_unicode(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "identity_unbalanced.csv")?;

    conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")?;
    let ident = r#"'über"'"#;
    let result = conn.execute(&format!(
        "CREATE FOREIGN TABLE ft {UMLAUT_COLS} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true', identity {ident})",
        csv_path.display()
    ));
    let err = result.unwrap_err().to_string();
    assert!(err.contains("unknown column"), "Got: {err}");

    Ok(())
}

/// The mirror builder already quotes; pin that a matview over quoted foreign
/// columns builds and reads back.
#[turso_macros::test(views)]
fn test_csv_fdw_matview_over_quoted_columns(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = write_csv(tmp_db.path.parent().unwrap(), "matview_quoted.csv")?;

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE SERVER csv_files OPTIONS (driver 'csv')",
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        &format!(
            "CREATE FOREIGN TABLE ft {COLS} SERVER csv_files \
             OPTIONS (path '{}', skip_header 'true')",
            csv_path.display()
        ),
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        r#"CREATE MATERIALIZED VIEW mv AS SELECT "order", "My Col" FROM ft"#,
    )?;

    let rows: Vec<(String, String)> = conn.exec_rows(r#"SELECT "order", "My Col" FROM mv"#);
    assert_eq!(
        rows,
        vec![
            ("10".to_string(), "alpha".to_string()),
            ("20".to_string(), "beta".to_string()),
        ]
    );

    Ok(())
}

/// Reopening rebuilds the driver from the stored DDL
/// (`Schema::populate_foreign_table`), so it re-runs the same generation.
#[test]
fn test_csv_fdw_quoted_columns_survive_reopen() {
    let dir = TempDir::new().unwrap().keep();
    let db_path = dir.join("quoted_reopen.db");
    let csv_path = write_csv(&dir, "reopen_quoted.csv").unwrap();

    let ddl = format!(
        "CREATE FOREIGN TABLE ft {COLS} SERVER csv_files \
         OPTIONS (path '{}', skip_header 'true')",
        csv_path.display()
    );

    {
        let db = TempDatabase::new_with_existent(&db_path);
        let conn = db.connect_limbo();
        conn.execute("CREATE SERVER csv_files OPTIONS (driver 'csv')")
            .unwrap();
        conn.execute(&ddl).unwrap();
        conn.close().unwrap();
    }

    {
        let db = TempDatabase::new_with_existent(&db_path);
        let conn = db.connect_limbo();
        let rows: Vec<(String, String)> = conn.exec_rows(r#"SELECT "order", "My Col" FROM ft"#);
        assert_eq!(
            rows,
            vec![
                ("10".to_string(), "alpha".to_string()),
                ("20".to_string(), "beta".to_string()),
            ]
        );
        conn.close().unwrap();
    }
}
