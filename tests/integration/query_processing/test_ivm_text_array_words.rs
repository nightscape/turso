//! A text-element array stores each element as the text it was written as,
//! whatever number or float spelling it has; a numeric array still refuses an
//! element that is not a number.

use crate::common::TempDatabase;
use proptest::prelude::*;
use proptest::test_runner::{Config as RunnerConfig, TestCaseError, TestRunner};
use std::collections::BTreeMap;
use std::sync::Arc;
use turso_core::{Connection, LimboError, Numeric, Value};

/// One array element: a word, or NULL.
pub(crate) type Element = Option<String>;

fn any_case(word: &'static str) -> impl Strategy<Value = String> {
    prop::collection::vec(any::<bool>(), word.len()).prop_map(move |upper| {
        word.chars()
            .zip(upper)
            .map(|(c, up)| if up { c.to_ascii_uppercase() } else { c })
            .collect()
    })
}

/// Unicode whitespace that is not ASCII, such as a no-break space:
/// PostgreSQL array syntax skips only ASCII whitespace.
fn unicode_space() -> impl Strategy<Value = char> {
    prop::sample::select(
        [
            '\u{85}', '\u{a0}', '\u{1680}', '\u{2028}', '\u{202f}', '\u{3000}', '\u{feff}',
        ]
        .into_iter()
        .chain('\u{2000}'..='\u{200a}')
        .collect::<Vec<_>>(),
    )
}

/// A word with Unicode whitespace that is not ASCII at an end.
fn unicode_space_word() -> impl Strategy<Value = String> {
    let end = || (prop::option::of(unicode_space()), "[ \t]{0,1}");
    (end(), "[a-z0-9]{0,2}", end())
        .prop_filter("a space at an end", |((l, _), _, (r, _))| {
            l.is_some() || r.is_some()
        })
        .prop_map(|((l, la), w, (r, ra))| {
            let s = |c: Option<char>| c.map(String::from).unwrap_or_default();
            format!("{la}{}{w}{}{ra}", s(l), s(r))
        })
}

/// A word a caller can write into an array: plain words, every spelling of a
/// number and of a non-finite float, and texts with the characters the array
/// literal syntax treats specially.
pub(crate) fn word() -> impl Strategy<Value = String> {
    let sign = prop::sample::select(vec!["", "-", "+"]);
    let non_finite = (
        sign.clone(),
        prop_oneof![any_case("nan"), any_case("inf"), any_case("infinity")],
    )
        .prop_map(|(s, w)| format!("{s}{w}"));
    let number = prop_oneof![
        "[+-]?0{0,3}[0-9]{1,4}",
        "[+-]?[0-9]{1,3}\\.[0-9]{0,3}",
        "[+-]?\\.[0-9]{1,3}",
        "[+-]?[0-9]{1,2}(\\.[0-9]{1,2})?[eE][+-]?[0-9]{1,3}",
        "[0-9]{19,22}",
        "0[xX][0-9a-fA-F]{1,4}",
        Just(i64::MIN.to_string()),
        Just(i64::MAX.to_string()),
        Just("1e400".to_string()),
    ];
    let text = prop::collection::vec(any::<char>().prop_filter("no NUL", |c| *c != '\0'), 0..5)
        .prop_map(|cs| cs.into_iter().collect::<String>());
    prop_oneof![
        3 => "[a-zA-Z]{1,6}",
        3 => non_finite,
        3 => number,
        1 => any_case("null"),
        1 => Just(String::new()),
        2 => "[ a,{}\"\\\\'\t]{1,5}",
        1 => " ?[0-9]{1,3} ?",
        2 => "\\PC{1,4}",
        1 => text,
        2 => unicode_space_word(),
    ]
}

/// An array of drawn words and NULLs.
pub(crate) fn elements() -> impl Strategy<Value = Vec<Element>> {
    prop::collection::vec(prop::option::weighted(0.9, word()), 0..5)
}

/// How a `{...}` element is spelled.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Spelling {
    /// Unquoted where the syntax allows it, else quoted.
    Bare,
    Quoted,
    /// Quoted, with a backslash before every character but `n`, `t` and `r`.
    Escaped,
}

/// How an array value reaches the statement.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Form {
    /// A `'{...}'` text literal in the SQL.
    Literal,
    /// A `{...}` text bound as a parameter.
    Parameter,
    /// `ARRAY[...]` of SQL string literals.
    Constructor,
}

#[derive(Clone, Debug)]
pub(crate) struct Spelled {
    pub(crate) elements: Vec<Element>,
    pub(crate) form: Form,
    spellings: Vec<Spelling>,
    /// ASCII whitespace around each element and around the braces: none, a
    /// space or a tab.
    padding: Vec<(u8, u8)>,
    /// Unicode whitespace that is not ASCII before and after the braces of a
    /// `{...}` text, which makes the text no array.
    junk: (Option<char>, Option<char>),
}

pub(crate) fn spelled(
    elements: impl Strategy<Value = Vec<Element>>,
) -> impl Strategy<Value = Spelled> {
    let form = prop_oneof![
        3 => Just(Form::Literal),
        2 => Just(Form::Parameter),
        1 => Just(Form::Constructor),
    ];
    let spelling = prop_oneof![
        3 => Just(Spelling::Bare),
        2 => Just(Spelling::Quoted),
        1 => Just(Spelling::Escaped)
    ];
    (elements, form).prop_flat_map(move |(elements, form)| {
        let n = elements.len();
        (
            Just(elements),
            Just(form),
            prop::collection::vec(spelling.clone(), n),
            prop::collection::vec((0u8..3, 0u8..3), n + 1),
            (
                prop::option::weighted(0.06, unicode_space()),
                prop::option::weighted(0.06, unicode_space()),
            ),
        )
            .prop_map(|(elements, form, spellings, padding, junk)| Spelled {
                elements,
                form,
                spellings,
                padding,
                junk,
            })
    })
}

fn sql_string(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A word that `{...}` can carry unquoted: PostgreSQL array syntax quotes
/// whitespace, the delimiters, quotes, backslashes and the word NULL.
fn can_be_bare(word: &str) -> bool {
    !word.is_empty()
        && !word.eq_ignore_ascii_case("null")
        && !word
            .chars()
            .any(|c| c.is_ascii_whitespace() || c == '\x0B' || ",{}\"\\".contains(c))
}

impl Spelled {
    /// The `{...}` text of the array.
    pub(crate) fn pg_text(&self) -> String {
        let spaces = |n: u8| ["", " ", "\t"][n as usize];
        let mut out: String = self.junk.0.into_iter().collect();
        out.push_str(spaces(self.padding[0].0));
        out.push('{');
        for (i, (element, spelling)) in self.elements.iter().zip(&self.spellings).enumerate() {
            if i > 0 {
                out.push(',');
            }
            let (before, after) = self.padding[i + 1];
            out.push_str(spaces(before));
            match (element, spelling) {
                (None, _) => out.push_str("NULL"),
                (Some(w), Spelling::Bare) if can_be_bare(w) => out.push_str(w),
                (Some(w), Spelling::Bare | Spelling::Quoted) => {
                    out.push('"');
                    for c in w.chars() {
                        if c == '"' || c == '\\' {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                }
                (Some(w), Spelling::Escaped) => {
                    out.push('"');
                    for c in w.chars() {
                        if !matches!(c, 'n' | 't' | 'r') {
                            out.push('\\');
                        }
                        out.push(c);
                    }
                    out.push('"');
                }
            }
            out.push_str(spaces(after));
        }
        out.push('}');
        out.push_str(spaces(self.padding[0].1));
        out.extend(self.junk.1);
        out
    }

    /// Whether the SQL of this array reads as an array.
    fn is_array(&self) -> bool {
        matches!(self.form, Form::Constructor) || self.junk == (None, None)
    }

    /// Whether `word`, spelled as `spelling`, is an unquoted token of a `{...}` text.
    fn bare(&self, word: &str, spelling: Spelling) -> bool {
        !matches!(self.form, Form::Constructor)
            && matches!(spelling, Spelling::Bare)
            && can_be_bare(word)
    }

    /// The SQL expression of the array and the parameter it binds, if any.
    pub(crate) fn sql(&self) -> (String, Option<String>) {
        match self.form {
            Form::Literal => (sql_string(&self.pg_text()), None),
            Form::Parameter => ("?1".to_string(), Some(self.pg_text())),
            Form::Constructor => (
                format!(
                    "ARRAY[{}]",
                    self.elements
                        .iter()
                        .map(|e| e.as_deref().map_or("NULL".to_string(), sql_string))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None,
            ),
        }
    }
}

/// Runs `sql`, binding `param` to `?1` if given, and returns its rows.
fn query(
    conn: &Arc<Connection>,
    sql: &str,
    param: Option<&str>,
) -> Result<Vec<Vec<Value>>, LimboError> {
    let mut stmt = conn.prepare(sql)?;
    if let Some(p) = param {
        stmt.bind_at(1.try_into().unwrap(), Value::build_text(p.to_string()))?;
    }
    stmt.run_collect_rows()
}

/// The length and the `n` elements of the array expression `array` in `from`,
/// read one by one by their subscripts, which count from 1 as in PostgreSQL.
fn read_elements(
    conn: &Arc<Connection>,
    array: &str,
    from: &str,
    n: usize,
    param: Option<&str>,
) -> Result<Vec<Vec<Value>>, LimboError> {
    let mut columns = vec![format!("array_length({array})")];
    columns.extend((1..=n).map(|i| format!("{array}[{i}]")));
    query(
        conn,
        &format!("SELECT {} {from}", columns.join(", ")),
        param,
    )
}

/// What the read of `read_elements` returns for an array holding `values`.
fn element_row(values: &[Value]) -> Vec<Value> {
    let mut row = vec![Value::from_i64(values.len() as i64)];
    row.extend(values.iter().cloned());
    row
}

fn text_values(elements: &[Element]) -> Vec<Value> {
    elements
        .iter()
        .map(|e| {
            e.as_ref()
                .map_or(Value::Null, |w| Value::build_text(w.clone()))
        })
        .collect()
}

/// A text-element array type and the DDL that declares its element type.
#[derive(Clone, Copy, Debug)]
enum TextElement {
    Text,
    /// ANY reads a `{...}` text as an untyped array does (see `stored_values`).
    Any,
    CustomType,
    /// An ENCODE that keeps the word: lookups on an index then encode their key first.
    CustomTypeWithEncode,
    Domain,
}

impl TextElement {
    fn setup(self) -> (&'static str, &'static str) {
        match self {
            TextElement::Text => ("TEXT", ""),
            TextElement::Any => ("ANY", ""),
            TextElement::CustomType => ("word", "CREATE TYPE word BASE text"),
            TextElement::CustomTypeWithEncode => {
                ("wordx", "CREATE TYPE wordx BASE text ENCODE value || ''")
            }
            TextElement::Domain => ("tag", "CREATE DOMAIN tag AS TEXT"),
        }
    }
}

#[derive(Clone, Debug)]
struct TextConfig {
    element: TextElement,
    index_on_words: bool,
    view: bool,
}

#[derive(Clone, Debug)]
enum TextOp {
    Insert {
        id: i64,
        words: Spelled,
        replace: bool,
    },
    Update {
        id: i64,
        words: Spelled,
    },
    Delete {
        id: i64,
    },
    Cast {
        words: Spelled,
    },
    Lookup {
        words: Spelled,
    },
    /// `UPDATE a SET words = <call over words> WHERE id = <id>`.
    Call {
        id: i64,
        call: ArrayCall,
    },
}

/// An array function that writes a new value of the `words` column.
#[derive(Clone, Debug)]
enum ArrayCall {
    Cat(Spelled),
    Append(String),
    Prepend(String),
    Remove(String),
    /// `string_to_array` of the words joined by `,`.
    Split(Vec<String>),
}

impl ArrayCall {
    fn sql(&self) -> (String, Option<String>) {
        match self {
            ArrayCall::Cat(words) => {
                let (array, param) = words.sql();
                (format!("array_cat(words, {array})"), param)
            }
            ArrayCall::Append(w) => (format!("array_append(words, {})", sql_string(w)), None),
            ArrayCall::Prepend(w) => (format!("array_prepend({}, words)", sql_string(w)), None),
            ArrayCall::Remove(w) => (format!("array_remove(words, {})", sql_string(w)), None),
            ArrayCall::Split(ws) => (
                format!("string_to_array({}, ',')", sql_string(&ws.join(","))),
                None,
            ),
        }
    }

    /// The array the call writes over `stored`, or `None` when the column refuses it.
    fn apply(&self, element: TextElement, stored: &[Value]) -> Option<Vec<Value>> {
        let text = |w: &String| Value::build_text(w.clone());
        Some(match self {
            ArrayCall::Cat(words) => [stored, &stored_values(element, words)?].concat(),
            ArrayCall::Append(w) => [stored, &[text(w)]].concat(),
            ArrayCall::Prepend(w) => [&[text(w)], stored].concat(),
            ArrayCall::Remove(w) => stored
                .iter()
                .filter(|v| !same_value(v, &text(w)))
                .cloned()
                .collect(),
            ArrayCall::Split(ws) => ws.iter().map(text).collect(),
        })
    }
}

fn array_call() -> impl Strategy<Value = ArrayCall> {
    let split_word = word().prop_filter("a word without the delimiter", |w| {
        !w.is_empty() && !w.contains(',')
    });
    // array_cat reads a `{...}` text numbers-first, as untyped arrays do, and
    // returns NULL for a non-finite token or a text that is no array, instead of
    // an error: no bare number and no junk is drawn.
    let cat_words = spelled(elements()).prop_filter("an array, no unquoted number", |s| {
        s.is_array()
            && s.elements.iter().zip(&s.spellings).all(|(e, sp)| {
                e.as_ref()
                    .is_none_or(|w| !s.bare(w, *sp) || w.parse::<f64>().is_err())
            })
    });
    prop_oneof![
        cat_words.prop_map(ArrayCall::Cat),
        word().prop_map(ArrayCall::Append),
        word().prop_map(ArrayCall::Prepend),
        word().prop_map(ArrayCall::Remove),
        prop::collection::vec(split_word, 1..4).prop_map(ArrayCall::Split),
    ]
}

/// The values an array of `element` stores for `words`, or `None` when it
/// refuses them: text elements keep every word; ANY reads an unquoted word in
/// a `{...}` text as an integer, else a float, else a text, and refuses the
/// array when that float is not finite.
fn stored_values(element: TextElement, words: &Spelled) -> Option<Vec<Value>> {
    if !words.is_array() {
        return None;
    }
    if !matches!(element, TextElement::Any) {
        return Some(text_values(&words.elements));
    }
    words
        .elements
        .iter()
        .zip(&words.spellings)
        .map(|(e, s)| match e {
            None => Some(Value::Null),
            Some(w) if words.bare(w, *s) => {
                if let Ok(i) = w.parse::<i64>() {
                    return Some(Value::from_i64(i));
                }
                match w.parse::<f64>() {
                    Ok(f) if f.is_finite() => Some(Value::from_f64(f)),
                    Ok(_) => None,
                    Err(_) => Some(Value::build_text(w.clone())),
                }
            }
            Some(w) => Some(Value::build_text(w.clone())),
        })
        .collect()
}

fn same_rows(a: &[Vec<Value>], b: &[Vec<Value>]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same_value(v, w)))
}

fn text_config() -> impl Strategy<Value = TextConfig> {
    (
        prop_oneof![
            Just(TextElement::Text),
            Just(TextElement::Any),
            Just(TextElement::CustomType),
            Just(TextElement::CustomTypeWithEncode),
            Just(TextElement::Domain)
        ],
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(|(element, index_on_words, view)| TextConfig {
            element,
            index_on_words,
            view,
        })
}

fn text_op() -> impl Strategy<Value = TextOp> {
    let id = 1i64..5;
    prop_oneof![
        3 => (id.clone(), spelled(elements()), any::<bool>())
            .prop_map(|(id, words, replace)| TextOp::Insert { id, words, replace }),
        2 => (id.clone(), spelled(elements())).prop_map(|(id, words)| TextOp::Update { id, words }),
        1 => id.prop_map(|id| TextOp::Delete { id }),
        2 => spelled(elements()).prop_map(|words| TextOp::Cast { words }),
        2 => spelled(prop::collection::vec(word().prop_map(Some), 0..4))
            .prop_map(|words| TextOp::Lookup { words }),
        2 => (1i64..5, array_call()).prop_map(|(id, call)| TextOp::Call { id, call }),
    ]
}

fn fresh_db(views: bool) -> TempDatabase {
    TempDatabase::builder()
        .with_views(views)
        .with_opts(turso_core::DatabaseOpts::new().with_custom_types(true))
        .build()
}

fn fail(context: &str, e: impl std::fmt::Display) -> String {
    format!("{context}: {e}")
}

/// Every op's outcome and every stored array equal a model of the written
/// words, read from the table and from a matview over it.
fn text_arrays_keep_their_words(config: &TextConfig, ops: &[TextOp]) -> Result<(), String> {
    let db = fresh_db(config.view);
    let conn = db.connect_limbo();
    let (ty, type_ddl) = config.element.setup();
    let mut setup = vec![];
    if !type_ddl.is_empty() {
        setup.push(type_ddl.to_string());
    }
    setup.push(format!(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, words {ty}[]) STRICT"
    ));
    if config.index_on_words {
        setup.push("CREATE INDEX a_words ON a(words)".to_string());
    }
    if config.view {
        setup.push("CREATE MATERIALIZED VIEW am AS SELECT id, words FROM a".to_string());
    }
    for sql in &setup {
        conn.execute(sql).map_err(|e| fail(sql, e))?;
    }
    let mut model: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
    for (step, op) in ops.iter().enumerate() {
        let at = format!("{config:?} step {step} {op:?}");
        // A text that is no array is refused naming the column and its storage
        // type, which for a custom type is its BASE or the ANY its ENCODE reads.
        let refused = |sql: &str, result: Result<Vec<Vec<Value>>, LimboError>| match result {
            Err(e) if e.to_string().contains(ty) => Ok(()),
            Err(e)
                if e.to_string().contains("non-array value in")
                    && e.to_string().contains(" column a.words") =>
            {
                Ok(())
            }
            other => Err(format!(
                "{at}: `{sql}` gave {other:?}, expected a refusal naming {ty} or the column"
            )),
        };
        match op {
            TextOp::Insert { id, words, replace } => {
                let (array, param) = words.sql();
                let verb = if *replace {
                    "INSERT OR REPLACE"
                } else {
                    "INSERT"
                };
                let sql = format!("{verb} INTO a VALUES ({id}, {array})");
                let result = query(&conn, &sql, param.as_deref());
                match stored_values(config.element, words) {
                    _ if !replace && model.contains_key(id) => {
                        if result.is_ok() {
                            return Err(format!("{at}: `{sql}` stored over a taken id"));
                        }
                    }
                    None => refused(&sql, result)?,
                    Some(values) => {
                        result.map_err(|e| fail(&format!("{at}: `{sql}`"), e))?;
                        model.insert(*id, values);
                    }
                }
            }
            TextOp::Update { id, words } => {
                let (array, param) = words.sql();
                let sql = format!("UPDATE a SET words = {array} WHERE id = {id}");
                let result = query(&conn, &sql, param.as_deref());
                match (stored_values(config.element, words), model.get_mut(id)) {
                    (None, Some(_)) => refused(&sql, result)?,
                    (Some(values), Some(stored)) => {
                        result.map_err(|e| fail(&format!("{at}: `{sql}`"), e))?;
                        *stored = values;
                    }
                    (_, None) => {
                        result.map_err(|e| fail(&format!("{at}: `{sql}`"), e))?;
                    }
                }
            }
            TextOp::Delete { id } => {
                let sql = format!("DELETE FROM a WHERE id = {id}");
                conn.execute(&sql).map_err(|e| fail(&at, e))?;
                model.remove(id);
            }
            TextOp::Cast { words } => {
                let (array, param) = words.sql();
                let cast = format!("CAST({array} AS {ty}[])");
                let got = read_elements(&conn, &cast, "", words.elements.len(), param.as_deref());
                match stored_values(config.element, words) {
                    // A CAST has no column to name in its refusal.
                    None => {
                        if let Ok(rows) = got {
                            return Err(format!("{at}: cast read {rows:?}, expected a refusal"));
                        }
                    }
                    Some(values) => {
                        let got = got.map_err(|e| fail(&format!("{at}: cast"), e))?;
                        let want = vec![element_row(&values)];
                        if !same_rows(&got, &want) {
                            return Err(format!("{at}: cast read {got:?}, wrote {want:?}"));
                        }
                    }
                }
            }
            // The engine refuses to compare an ANY[] column with an ARRAY[...] of texts.
            TextOp::Lookup { words }
                if matches!(config.element, TextElement::Any)
                    && matches!(words.form, Form::Constructor) => {}
            TextOp::Lookup { words } => {
                let (array, param) = words.sql();
                let sql = format!("SELECT id FROM a WHERE words = {array} ORDER BY id");
                let got = query(&conn, &sql, param.as_deref());
                let want: Vec<Vec<Value>> = match stored_values(config.element, words) {
                    None => vec![],
                    Some(values) => model
                        .iter()
                        .filter(|(_, stored)| same_rows(&[stored.to_vec()], &[values.clone()]))
                        .map(|(id, _)| vec![Value::from_i64(*id)])
                        .collect(),
                };
                let got = got.map_err(|e| fail(&format!("{at}: `{sql}`"), e))?;
                if got != want {
                    return Err(format!("{at}: `{sql}` found {got:?}, the model {want:?}"));
                }
            }
            TextOp::Call { id, call } => {
                let (value, param) = call.sql();
                let sql = format!("UPDATE a SET words = {value} WHERE id = {id}");
                let result = query(&conn, &sql, param.as_deref());
                match model.get_mut(id) {
                    None => {
                        result.map_err(|e| fail(&format!("{at}: `{sql}`"), e))?;
                    }
                    Some(stored) => match call.apply(config.element, stored) {
                        None => refused(&sql, result)?,
                        Some(values) => {
                            result.map_err(|e| fail(&format!("{at}: `{sql}`"), e))?;
                            *stored = values;
                        }
                    },
                }
            }
        }
        let mut sources = vec!["a"];
        if config.view {
            sources.push("am");
        }
        for source in sources {
            let ids = query(&conn, &format!("SELECT id FROM {source} ORDER BY id"), None)
                .map_err(|e| fail(&at, e))?;
            let want_ids: Vec<Vec<Value>> =
                model.keys().map(|id| vec![Value::from_i64(*id)]).collect();
            if ids != want_ids {
                return Err(format!(
                    "{at}: {source} holds ids {ids:?}, the model {want_ids:?}"
                ));
            }
            for (id, stored) in &model {
                let got = read_elements(
                    &conn,
                    "words",
                    &format!("FROM {source} WHERE id = {id}"),
                    stored.len(),
                    None,
                )
                .map_err(|e| fail(&at, e))?;
                let want = vec![element_row(stored)];
                if !same_rows(&got, &want) {
                    return Err(format!(
                        "{at}: {source} id {id} reads {got:?}, wrote {want:?}"
                    ));
                }
            }
        }
        // A matview reads an array column back as its stored record, so the
        // two are compared by value.
        if config.view {
            let read = |sql: &str| query(&conn, sql, None).map_err(|e| fail(&at, e));
            let same = read(
                "SELECT a.id FROM a JOIN am ON a.id = am.id WHERE a.words IS am.words ORDER BY a.id",
            )?;
            let ids = read("SELECT id FROM a ORDER BY id")?;
            if same != ids {
                return Err(format!(
                    "{at}: am holds the words of a for ids {same:?}, a has {ids:?}"
                ));
            }
        }
    }
    Ok(())
}

/// A numeric value a word stands for, read as SQL reads a number literal.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Number {
    Integer(i64),
    Real(f64),
}

/// The number `word` spells, by SQL's numeric literal grammar with the
/// surrounding whitespace a numeric affinity ignores; `None` for any other
/// word, also for nan, inf and a value too large for a finite REAL.
fn number(word: &str) -> Option<Number> {
    let w = trim(word);
    let unsigned = w.strip_prefix(['+', '-']).unwrap_or(w);
    if digits(unsigned) {
        if let Ok(i) = w.parse::<i64>() {
            return Some(Number::Integer(i));
        }
    }
    decimal(w).filter(|f| f.is_finite()).map(Number::Real)
}

fn trim(word: &str) -> &str {
    word.trim_matches(|c: char| c.is_ascii_whitespace() || c == '\x0B')
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// The value of a word in SQL's decimal literal grammar
/// `[+-]?(digits[.digits?]|.digits)([eE][+-]?digits)?`, infinite when it overflows.
fn decimal(w: &str) -> Option<f64> {
    let unsigned = w.strip_prefix(['+', '-']).unwrap_or(w);
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let mantissa_ok = match mantissa.split_once('.') {
        Some((int, frac)) => {
            (digits(int) || int.is_empty())
                && (digits(frac) || frac.is_empty())
                && !(int.is_empty() && frac.is_empty())
        }
        None => digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|e| digits(e.strip_prefix(['+', '-']).unwrap_or(e)));
    (mantissa_ok && exponent_ok).then(|| w.parse().unwrap())
}

/// A word that reads as a float that is not finite: nan, inf or infinity in
/// any case and with any sign, or a decimal too large for a REAL.
fn non_finite(word: &str) -> bool {
    let w = trim(word);
    let unsigned = w.strip_prefix(['+', '-']).unwrap_or(w);
    ["nan", "inf", "infinity"]
        .iter()
        .any(|n| unsigned.eq_ignore_ascii_case(n))
        || decimal(w).is_some_and(|f| !f.is_finite())
}

#[derive(Clone, Copy, Debug)]
enum NumericElement {
    Integer,
    Int,
    Real,
    Blob,
    CustomInteger,
}

impl NumericElement {
    fn setup(self) -> (&'static str, &'static str) {
        match self {
            NumericElement::Integer => ("INTEGER", ""),
            NumericElement::Int => ("INT", ""),
            NumericElement::Real => ("REAL", ""),
            NumericElement::Blob => ("BLOB", ""),
            NumericElement::CustomInteger => ("count", "CREATE TYPE count BASE integer"),
        }
    }

    /// The storage type a refusal names.
    fn named_type(self) -> &'static str {
        match self {
            NumericElement::Integer | NumericElement::CustomInteger => "INTEGER",
            NumericElement::Int => "INT",
            NumericElement::Real => "REAL",
            NumericElement::Blob => "BLOB",
        }
    }

    /// The stored element for `word`, or `None` when the type refuses it as
    /// a STRICT column of the element type refuses a value. A `bare` word is
    /// read by the array parser, which refuses a float that is not finite; any
    /// other word reaches the element type as a text, and REAL reads a decimal
    /// too large for it as an infinity, as SQLite's REAL affinity does.
    fn store(self, word: &str, bare: bool) -> Option<Value> {
        match self {
            NumericElement::Integer | NumericElement::Int | NumericElement::CustomInteger => {
                match number(word)? {
                    Number::Integer(i) => Some(Value::from_i64(i)),
                    Number::Real(f) => {
                        (f.fract() == 0.0 && f >= -(2f64.powi(63)) && f < 2f64.powi(63))
                            .then(|| Value::from_i64(f as i64))
                    }
                }
            }
            NumericElement::Real => match number(word) {
                Some(Number::Integer(i)) => Some(Value::from_f64(i as f64)),
                Some(Number::Real(f)) => Some(Value::from_f64(f)),
                None if !bare => decimal(trim(word)).map(Value::from_f64),
                None => None,
            },
            NumericElement::Blob => None,
        }
    }
}

fn numeric_element() -> impl Strategy<Value = NumericElement> {
    prop_oneof![
        Just(NumericElement::Integer),
        Just(NumericElement::Int),
        Just(NumericElement::Real),
        Just(NumericElement::Blob),
        Just(NumericElement::CustomInteger),
    ]
}

/// An array of number words, with at most one word drawn from all words.
fn numeric_elements() -> impl Strategy<Value = Vec<Element>> {
    let number_word = prop_oneof![
        "[+-]?0{0,3}[0-9]{1,4}",
        "[+-]?[0-9]{1,3}\\.[0-9]{1,3}",
        "[+-]?[0-9]{1,2}[eE][+-]?[0-9]{1,2}",
    ];
    (
        prop::collection::vec(prop::option::weighted(0.9, number_word), 0..4),
        prop::option::of(word()),
        any::<prop::sample::Index>(),
    )
        .prop_map(|(mut elements, odd, at)| {
            if let Some(w) = odd {
                let i = at.index(elements.len() + 1);
                elements.insert(i, Some(w));
            }
            elements
        })
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Numeric(Numeric::Float(x)), Value::Numeric(Numeric::Float(y))) => {
            f64::from(*x) == f64::from(*y)
        }
        _ => a == b,
    }
}

/// A numeric array stores number words as their numbers and refuses an array
/// with any word that is not a number, in a column and in a CAST.
fn numeric_arrays_refuse_non_numbers(
    element: NumericElement,
    words: &Spelled,
    cast: bool,
) -> Result<(), String> {
    let db = fresh_db(false);
    let conn = db.connect_limbo();
    let (ty, type_ddl) = element.setup();
    if !type_ddl.is_empty() {
        conn.execute(type_ddl).map_err(|e| fail(type_ddl, e))?;
    }
    let create = format!("CREATE TABLE n (id INTEGER PRIMARY KEY, v {ty}[]) STRICT");
    conn.execute(&create).map_err(|e| fail(&create, e))?;
    let want: Option<Vec<Value>> = words
        .elements
        .iter()
        .zip(&words.spellings)
        .map(|(e, s)| match e {
            None => Some(Value::Null),
            Some(w) => element.store(w, words.bare(w, *s)),
        })
        .collect::<Option<_>>()
        .filter(|_| words.is_array());
    let (array, param) = words.sql();
    let at = format!("{ty}[] cast={cast} {words:?} `{}`", words.pg_text());
    let n = words.elements.len();
    let got = if cast {
        let cast = format!("CAST({array} AS {ty}[])");
        read_elements(&conn, &cast, "", n, param.as_deref())
    } else {
        let sql = format!("INSERT INTO n VALUES (1, {array})");
        match query(&conn, &sql, param.as_deref()) {
            Ok(_) => read_elements(&conn, "v", "FROM n WHERE id = 1", n, None),
            Err(e) => Err(e),
        }
    };
    match (want, got) {
        // A CAST has no column to name in its refusal.
        (None, Err(_)) if cast => Ok(()),
        (None, Err(e)) if e.to_string().contains(element.named_type()) => Ok(()),
        (None, Err(e)) => Err(format!(
            "{at}: refused with `{e}`, which does not name {}",
            element.named_type()
        )),
        (None, Ok(rows)) => Err(format!("{at}: stored {rows:?}, expected a refusal")),
        (Some(_), Err(e)) => Err(format!("{at}: refused: {e}")),
        (Some(values), Ok(rows)) => {
            let want = element_row(&values);
            let equal = rows.len() == 1
                && rows[0].len() == want.len()
                && rows[0].iter().zip(&want).all(|(a, b)| same_value(a, b));
            if equal {
                Ok(())
            } else {
                Err(format!("{at}: read {rows:?}, expected {want:?}"))
            }
        }
    }
}

fn runner() -> TestRunner {
    TestRunner::new(RunnerConfig {
        cases: std::env::var("PROPTEST_CASES").map_or(64, |n| n.parse().unwrap()),
        failure_persistence: None,
        ..RunnerConfig::default()
    })
}

fn literal(elements: &[&str]) -> Spelled {
    Spelled {
        elements: elements.iter().map(|w| Some(w.to_string())).collect(),
        form: Form::Literal,
        spellings: vec![Spelling::Bare; elements.len()],
        padding: vec![(0, 0); elements.len() + 1],
        junk: (None, None),
    }
}

#[test]
fn text_arrays_store_every_word_as_written() {
    let float_words = literal(&[
        "NAn", "007", "1.50", "1e2", "-inf", "Infinity", "nan", "INF",
    ]);
    let fixed: Vec<(TextConfig, Vec<TextOp>)> = vec![
        (
            TextConfig {
                element: TextElement::Text,
                index_on_words: false,
                view: false,
            },
            vec![
                TextOp::Insert {
                    id: 1,
                    words: literal(&["\u{a0}"]),
                    replace: false,
                },
                TextOp::Insert {
                    id: 2,
                    words: literal(&["x\u{3000}"]),
                    replace: false,
                },
                TextOp::Insert {
                    id: 3,
                    words: Spelled {
                        junk: (None, Some('\u{a0}')),
                        ..literal(&["x"])
                    },
                    replace: false,
                },
                TextOp::Insert {
                    id: 4,
                    words: Spelled {
                        padding: vec![(2, 1), (0, 2)],
                        ..literal(&["x"])
                    },
                    replace: false,
                },
            ],
        ),
        (
            TextConfig {
                element: TextElement::Text,
                index_on_words: false,
                view: false,
            },
            vec![TextOp::Insert {
                id: 1,
                words: literal(&["NAn"]),
                replace: false,
            }],
        ),
        (
            TextConfig {
                element: TextElement::Text,
                index_on_words: true,
                view: true,
            },
            vec![
                TextOp::Insert {
                    id: 1,
                    words: float_words.clone(),
                    replace: true,
                },
                TextOp::Lookup {
                    words: float_words.clone(),
                },
                TextOp::Lookup {
                    words: literal(&["NAn", "7", "1.5", "100.0", "-inf", "Infinity", "nan", "INF"]),
                },
                TextOp::Cast {
                    words: float_words.clone(),
                },
                TextOp::Update {
                    id: 1,
                    words: literal(&["007"]),
                },
            ],
        ),
        (
            TextConfig {
                element: TextElement::Any,
                index_on_words: false,
                view: true,
            },
            vec![
                TextOp::Insert {
                    id: 3,
                    words: float_words.clone(),
                    replace: false,
                },
                TextOp::Insert {
                    id: 4,
                    words: Spelled {
                        spellings: vec![
                            Spelling::Bare,
                            Spelling::Bare,
                            Spelling::Bare,
                            Spelling::Bare,
                            Spelling::Quoted,
                        ],
                        ..literal(&["007", "1.50", "1e2", "abc", "nan"])
                    },
                    replace: false,
                },
                TextOp::Cast {
                    words: float_words.clone(),
                },
            ],
        ),
        (
            TextConfig {
                element: TextElement::Text,
                index_on_words: false,
                view: false,
            },
            vec![TextOp::Insert {
                id: 1,
                words: Spelled {
                    spellings: vec![Spelling::Escaped],
                    ..literal(&["é"])
                },
                replace: false,
            }],
        ),
        (
            TextConfig {
                element: TextElement::CustomType,
                index_on_words: true,
                view: false,
            },
            vec![TextOp::Insert {
                id: 2,
                words: float_words,
                replace: false,
            }],
        ),
        (
            TextConfig {
                element: TextElement::CustomTypeWithEncode,
                index_on_words: true,
                view: false,
            },
            vec![
                TextOp::Insert {
                    id: 1,
                    words: literal(&["nan"]),
                    replace: false,
                },
                TextOp::Lookup {
                    words: literal(&["nan"]),
                },
            ],
        ),
    ];
    for (config, ops) in &fixed {
        text_arrays_keep_their_words(config, ops).unwrap();
    }
    runner()
        .run(
            &(text_config(), prop::collection::vec(text_op(), 0..8)),
            |(config, ops)| {
                text_arrays_keep_their_words(&config, &ops).map_err(TestCaseError::fail)
            },
        )
        .unwrap();
}

/// A CAST to a custom-type array converts each element by the scalar CAST
/// of that type, not by the column's check, so it is not drawn.
fn cast_drawn(element: NumericElement, cast: bool) -> bool {
    !(cast && matches!(element, NumericElement::CustomInteger))
}

#[test]
fn numeric_arrays_refuse_words_that_are_not_numbers() {
    let fixed = [
        (NumericElement::Integer, literal(&["nan"])),
        (NumericElement::Real, literal(&["nan"])),
        (NumericElement::Real, literal(&["-Inf"])),
        (NumericElement::Integer, literal(&["abc"])),
        (NumericElement::Integer, literal(&["007", "1e2"])),
        (NumericElement::Real, literal(&["1.50", "007"])),
        (NumericElement::CustomInteger, literal(&["abc"])),
        (NumericElement::CustomInteger, literal(&["0.1"])),
        (NumericElement::Blob, literal(&["1"])),
        (NumericElement::Integer, literal(&["\u{a0}7"])),
    ];
    for (element, words) in &fixed {
        for cast in [false, true]
            .into_iter()
            .filter(|c| cast_drawn(*element, *c))
        {
            numeric_arrays_refuse_non_numbers(*element, words, cast).unwrap();
        }
    }
    runner()
        .run(
            &(
                numeric_element(),
                spelled(numeric_elements()),
                any::<bool>(),
            )
                .prop_filter("a drawn CAST target", |(element, _, cast)| {
                    cast_drawn(*element, *cast)
                }),
            |(element, words, cast)| {
                numeric_arrays_refuse_non_numbers(element, &words, cast)
                    .map_err(TestCaseError::fail)
            },
        )
        .unwrap();
}

/// An array literal without a type, as the array functions read it: a bare
/// word that reads as a non-finite float, or junk outside the braces, makes the
/// text no array, whose length is NULL; any other array is counted.
fn untyped_literals_keep_refusing_non_finite_words(words: &Spelled) -> Result<(), String> {
    let db = fresh_db(false);
    let conn = db.connect_limbo();
    let (array, param) = words.sql();
    let got = query(
        &conn,
        &format!("SELECT array_length({array})"),
        param.as_deref(),
    );
    let spelled_bare = |(e, s): (&Element, &Spelling)| {
        e.as_deref()
            .is_some_and(|w| matches!(s, Spelling::Bare) && can_be_bare(w) && non_finite(w))
    };
    let pairs = || words.elements.iter().zip(&words.spellings);
    let at = format!("{words:?} `{}`", words.pg_text());
    if !words.is_array() || pairs().any(spelled_bare) {
        return match got {
            Ok(rows) if rows == vec![vec![Value::Null]] => Ok(()),
            other => Err(format!("{at}: got {other:?}, expected the length NULL")),
        };
    }
    match got {
        Ok(rows) if rows == vec![vec![Value::from_i64(words.elements.len() as i64)]] => Ok(()),
        other => Err(format!(
            "{at}: got {other:?}, expected the count {}",
            words.elements.len()
        )),
    }
}

fn untyped_spelled() -> impl Strategy<Value = Spelled> {
    spelled(elements()).prop_filter("a text literal or parameter", |s| {
        matches!(s.form, Form::Literal | Form::Parameter)
    })
}

#[test]
fn array_functions_refuse_untyped_literals_with_non_finite_words() {
    let fixed = [
        literal(&["NAn"]),
        literal(&["a", "-inf"]),
        literal(&["Infinity"]),
        literal(&["1e400"]),
        literal(&["abc", "007", "1.50", "1e2"]),
        literal(&[]),
        Spelled {
            junk: (Some('\u{3000}'), None),
            ..literal(&["x"])
        },
    ];
    for words in &fixed {
        untyped_literals_keep_refusing_non_finite_words(words).unwrap();
    }
    runner()
        .run(&untyped_spelled(), |words| {
            untyped_literals_keep_refusing_non_finite_words(&words).map_err(TestCaseError::fail)
        })
        .unwrap();
}
