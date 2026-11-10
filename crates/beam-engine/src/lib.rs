//! The embedded analytical engine behind Open Beam.
//!
//! An [`Engine`] owns an in-memory DuckDB database in which every configured
//! dataset is a view over a file the operator named. Once the views exist the
//! database is sealed: file access is restricted to exactly those files, and
//! the configuration is locked so no later statement can widen it.

use duckdb::types::{Value as DuckValue, ValueRef};
use duckdb::{params_from_iter, Connection};
use serde_json::{Number, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Name of the row-number key added to datasets that declare no key column.
pub const ROW_KEY: &str = "_row";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("dataset '{0}': name must match [A-Za-z_][A-Za-z0-9_]* and be unique")]
    InvalidName(String),
    #[error("dataset '{dataset}': cannot read '{path}'")]
    Unreadable { dataset: String, path: PathBuf },
    #[error("dataset '{dataset}': key column '{key}' does not exist")]
    UnknownKey { dataset: String, key: String },
    #[error("dataset '{0}': unrecognised file type; set format to \"csv\" or \"parquet\"")]
    UnknownFormat(String),
    #[error("dataset '{0}' has no columns")]
    Empty(String),
    #[error("dataset '{dataset}': invalid query: {reason}")]
    InvalidQuery { dataset: String, reason: String },
    #[error(transparent)]
    Database(#[from] duckdb::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    Parquet,
}

#[derive(Debug, Clone)]
pub enum Source {
    /// A data file. `format` is inferred from the extension when `None`.
    File { path: PathBuf, format: Option<Format> },
    /// A single `SELECT` statement over datasets declared earlier. Used to
    /// publish cleaned, joined or de-duplicated results as their own dataset.
    Query(String),
}

#[derive(Debug, Clone)]
pub struct DatasetSpec {
    pub name: String,
    pub source: Source,
    /// A column whose values are unique and never null. When `None` the
    /// engine adds a [`ROW_KEY`] row-number column instead.
    pub key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Boolean,
    Int32,
    Int64,
    Float64,
    Decimal { precision: u8, scale: u8 },
    Text,
    Date,
    Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dataset {
    pub name: String,
    pub key: String,
    pub columns: Vec<Column>,
}

/// A value bound to a `?` placeholder.
#[derive(Debug, Clone, PartialEq)]
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

pub struct Engine {
    conn: Mutex<Connection>,
    datasets: Vec<Dataset>,
}

impl Engine {
    pub fn open(specs: &[DatasetSpec]) -> Result<Self, EngineError> {
        let conn = Connection::open_in_memory()?;
        let mut datasets = Vec::new();
        let mut allowed_paths = Vec::new();
        let mut names = HashSet::new();

        for spec in specs {
            if !is_identifier(&spec.name) || !names.insert(spec.name.to_ascii_lowercase()) {
                return Err(EngineError::InvalidName(spec.name.clone()));
            }
            let source = match &spec.source {
                Source::File { path, format } => {
                    let unreadable = || EngineError::Unreadable {
                        dataset: spec.name.clone(),
                        path: path.clone(),
                    };
                    let path = path.canonicalize().map_err(|_| unreadable())?;
                    let path_text = path.to_str().ok_or_else(unreadable)?;
                    let reader = match format.or_else(|| infer_format(&path)) {
                        Some(Format::Csv) => "read_csv",
                        Some(Format::Parquet) => "read_parquet",
                        None => return Err(EngineError::UnknownFormat(spec.name.clone())),
                    };
                    allowed_paths.push(quote_literal(path_text));
                    format!("{reader}({})", quote_literal(path_text))
                }
                // Wrapping the text as a subquery means only a SELECT can parse.
                Source::Query(sql) => format!("({}) AS query", sql.trim().trim_end_matches(';')),
            };
            let dataset = create_view(&conn, spec, &source).map_err(|e| match (&spec.source, e) {
                (Source::Query(_), EngineError::Database(e)) => EngineError::InvalidQuery {
                    dataset: spec.name.clone(),
                    reason: e.to_string(),
                },
                (_, e) => e,
            })?;
            datasets.push(dataset);
        }

        conn.execute_batch(&format!(
            "SET allowed_paths = [{}]; SET enable_external_access = false; SET lock_configuration = true;",
            allowed_paths.join(", ")
        ))?;
        // A query that reads a file which is not itself a configured dataset
        // only fails once the database is sealed, so check every dataset now
        // rather than on a client's first request.
        for dataset in &datasets {
            let probe = format!("SELECT * FROM {} LIMIT 0", quote_ident(&dataset.name));
            conn.execute_batch(&probe).map_err(|e| EngineError::InvalidQuery {
                dataset: dataset.name.clone(),
                reason: e.to_string(),
            })?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
            datasets,
        })
    }

    pub fn datasets(&self) -> &[Dataset] {
        &self.datasets
    }

    /// Runs a statement and returns its rows, each `width` values wide.
    pub fn query(&self, sql: &str, params: &[Param], width: usize) -> Result<Vec<Vec<Value>>, EngineError> {
        // Each query gets its own connection so slow queries do not queue
        // behind the lock; the lock only guards the clone itself.
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        let mut statement = conn.prepare(sql)?;
        let bound = params.iter().map(|p| match p {
            Param::Null => DuckValue::Null,
            Param::Bool(b) => DuckValue::Boolean(*b),
            Param::Int(i) => DuckValue::BigInt(*i),
            Param::Float(f) => DuckValue::Double(*f),
            Param::Text(s) => DuckValue::Text(s.clone()),
        });
        let mut rows = statement.query(params_from_iter(bound))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let mut values = Vec::with_capacity(width);
            for index in 0..width {
                values.push(to_json(row.get_ref(index)?));
            }
            out.push(values);
        }
        Ok(out)
    }
}

fn create_view(conn: &Connection, spec: &DatasetSpec, source: &str) -> Result<Dataset, EngineError> {
    let mut describe = conn.prepare(&format!("DESCRIBE SELECT * FROM {source}"))?;
    let source_columns: Vec<(String, String)> = describe
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
        .collect::<Result<_, _>>()?;
    if source_columns.is_empty() {
        return Err(EngineError::Empty(spec.name.clone()));
    }

    let mut taken: HashSet<String> = HashSet::from([ROW_KEY.to_string()]);
    let mut columns = Vec::new();
    let mut select = Vec::new();
    let mut key = None;
    for (source_name, source_type) in &source_columns {
        let name = unique_identifier(source_name, &mut taken);
        let (ty, cast) = normalise(source_type);
        select.push(format!(
            "CAST({} AS {cast}) AS {}",
            quote_ident(source_name),
            quote_ident(&name)
        ));
        if spec.key.as_deref() == Some(source_name.as_str()) {
            key = Some(name.clone());
        }
        columns.push(Column { name, ty });
    }

    let key = match (&spec.key, key) {
        (Some(_), Some(found)) => found,
        (Some(missing), None) => {
            return Err(EngineError::UnknownKey {
                dataset: spec.name.clone(),
                key: missing.clone(),
            })
        }
        (None, _) => {
            select.insert(0, format!("row_number() OVER () AS {}", quote_ident(ROW_KEY)));
            columns.insert(
                0,
                Column {
                    name: ROW_KEY.to_string(),
                    ty: ColumnType::Int64,
                },
            );
            ROW_KEY.to_string()
        }
    };

    conn.execute_batch(&format!(
        "CREATE VIEW {} AS SELECT {} FROM {source}",
        quote_ident(&spec.name),
        select.join(", ")
    ))?;
    Ok(Dataset {
        name: spec.name.clone(),
        key,
        columns,
    })
}

/// Maps a DuckDB type name to the exposed type and the SQL type the view
/// casts to. Types with no exposed equivalent are served as text.
fn normalise(source_type: &str) -> (ColumnType, String) {
    let upper = source_type.to_ascii_uppercase();
    let simple = |ty, sql: &str| (ty, sql.to_string());
    match upper.as_str() {
        "BOOLEAN" => simple(ColumnType::Boolean, "BOOLEAN"),
        "TINYINT" | "SMALLINT" | "INTEGER" | "UTINYINT" | "USMALLINT" => simple(ColumnType::Int32, "INTEGER"),
        "BIGINT" | "UINTEGER" => simple(ColumnType::Int64, "BIGINT"),
        "UBIGINT" | "HUGEINT" => (
            ColumnType::Decimal {
                precision: 38,
                scale: 0,
            },
            "DECIMAL(38,0)".to_string(),
        ),
        "FLOAT" | "DOUBLE" => simple(ColumnType::Float64, "DOUBLE"),
        "DATE" => simple(ColumnType::Date, "DATE"),
        "TIMESTAMP" | "TIMESTAMP_S" | "TIMESTAMP_MS" | "TIMESTAMP_NS" => simple(ColumnType::Timestamp, "TIMESTAMP"),
        _ => match parse_decimal(&upper) {
            Some((precision, scale)) => (ColumnType::Decimal { precision, scale }, upper),
            None => simple(ColumnType::Text, "VARCHAR"),
        },
    }
}

fn parse_decimal(upper: &str) -> Option<(u8, u8)> {
    let (precision, scale) = upper.strip_prefix("DECIMAL(")?.strip_suffix(')')?.split_once(',')?;
    Some((precision.trim().parse().ok()?, scale.trim().parse().ok()?))
}

fn to_json(value: ValueRef<'_>) -> Value {
    let decimal = |text: String| text.parse::<Number>().map_or(Value::Null, Value::Number);
    match value {
        ValueRef::Boolean(b) => Value::Bool(b),
        ValueRef::TinyInt(i) => i.into(),
        ValueRef::SmallInt(i) => i.into(),
        ValueRef::Int(i) => i.into(),
        ValueRef::BigInt(i) => i.into(),
        ValueRef::UTinyInt(i) => i.into(),
        ValueRef::USmallInt(i) => i.into(),
        ValueRef::UInt(i) => i.into(),
        ValueRef::UBigInt(i) => i.into(),
        ValueRef::HugeInt(i) => decimal(i.to_string()),
        // JSON has no NaN or infinity; they are served as null.
        ValueRef::Float(f) => Number::from_f64(f64::from(f)).map_or(Value::Null, Value::Number),
        ValueRef::Double(f) => Number::from_f64(f).map_or(Value::Null, Value::Number),
        ValueRef::Decimal(d) => decimal(d.to_string()),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        // Views cast every other type to one of the above.
        _ => Value::Null,
    }
}

fn infer_format(path: &Path) -> Option<Format> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "csv" | "tsv" | "txt" => Some(Format::Csv),
        "parquet" | "pq" => Some(Format::Parquet),
        _ => None,
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.len() <= 128
}

/// Derives a simple identifier from an arbitrary column header, unique
/// within `taken`.
fn unique_identifier(source: &str, taken: &mut HashSet<String>) -> String {
    let mut base: String = source
        .chars()
        .take(100)
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if !base.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        base.insert(0, '_');
    }
    let mut name = base.clone();
    let mut suffix = 2;
    while !taken.insert(name.to_ascii_lowercase()) {
        name = format!("{base}_{suffix}");
        suffix += 1;
    }
    name
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn spec(name: &str, path: &Path, key: Option<&str>) -> DatasetSpec {
        DatasetSpec {
            name: name.into(),
            source: Source::File {
                path: path.into(),
                format: None,
            },
            key: key.map(Into::into),
        }
    }

    fn query_spec(name: &str, sql: &str) -> DatasetSpec {
        DatasetSpec {
            name: name.into(),
            source: Source::Query(sql.into()),
            key: None,
        }
    }

    fn sample(dir: &Path) -> PathBuf {
        let path = dir.join("it's data.csv");
        fs::write(
            &path,
            "order id,Region,amount,price,sold on,2nd,flag\n\
             1,EU,3,19.99,2024-01-05,a,true\n\
             2,US,5,5.50,2024-02-10,b,false\n\
             3,,7,0.10,2024-03-15,c,true\n",
        )
        .unwrap();
        path
    }

    #[test]
    fn exposes_typed_columns_with_safe_names() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(&[spec("sales", &sample(dir.path()), Some("order id"))]).unwrap();
        let dataset = &engine.datasets()[0];
        assert_eq!(dataset.key, "order_id");
        let described: Vec<_> = dataset.columns.iter().map(|c| (c.name.as_str(), c.ty)).collect();
        assert_eq!(
            described,
            vec![
                ("order_id", ColumnType::Int64),
                ("Region", ColumnType::Text),
                ("amount", ColumnType::Int64),
                ("price", ColumnType::Float64),
                ("sold_on", ColumnType::Date),
                ("_2nd", ColumnType::Text),
                ("flag", ColumnType::Boolean),
            ]
        );
    }

    #[test]
    fn queries_bind_parameters() {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::open(&[spec("sales", &sample(dir.path()), None)]).unwrap();
        assert_eq!(engine.datasets()[0].key, ROW_KEY);
        let rows = engine
            .query(
                "SELECT \"_row\", \"Region\", \"price\", strftime(\"sold_on\", '%Y-%m-%d'), \"flag\" \
                 FROM \"sales\" WHERE \"amount\" > ? ORDER BY \"_row\"",
                &[Param::Int(4)],
                5,
            )
            .unwrap();
        assert_eq!(
            rows,
            vec![
                vec![json!(2), json!("US"), json!(5.5), json!("2024-02-10"), json!(false)],
                vec![json!(3), Value::Null, json!(0.1), json!("2024-03-15"), json!(true)],
            ]
        );
    }

    #[test]
    fn sealed_database_cannot_reach_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.csv");
        fs::write(&secret, "token\nhunter2\n").unwrap();
        let engine = Engine::open(&[spec("sales", &sample(dir.path()), None)]).unwrap();

        let read = format!("SELECT * FROM read_csv({})", quote_literal(secret.to_str().unwrap()));
        assert!(engine.query(&read, &[], 1).is_err());
        let copy = format!(
            "COPY sales TO {}",
            quote_literal(dir.path().join("out.csv").to_str().unwrap())
        );
        assert!(engine.query(&copy, &[], 0).is_err());
        assert!(engine.query("SET enable_external_access = true", &[], 0).is_err());
        assert!(engine.query("INSTALL httpfs", &[], 0).is_err());
        // The configured dataset itself still reads.
        assert_eq!(
            engine.query("SELECT count(*) FROM sales", &[], 1).unwrap(),
            vec![vec![json!(3)]]
        );
    }

    #[test]
    fn rejects_bad_specs() {
        let dir = tempfile::tempdir().unwrap();
        let path = sample(dir.path());
        let open = |specs: &[DatasetSpec]| Engine::open(specs).err().map(|e| e.to_string()).unwrap();
        assert!(open(&[spec("bad name", &path, None)]).contains("name must match"));
        assert!(open(&[spec("a", &path, None), spec("A", &path, None)]).contains("unique"));
        assert!(open(&[spec("a", &dir.path().join("missing.csv"), None)]).contains("cannot read"));
        assert!(open(&[spec("a", &path, Some("nope"))]).contains("key column 'nope'"));
    }

    #[test]
    fn query_datasets_derive_from_earlier_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reports.csv");
        fs::write(&path, "person,reported_by\nA,youth\nA,children\nB,youth\n").unwrap();
        let engine = Engine::open(&[
            spec("reports", &path, None),
            query_spec(
                "people",
                "SELECT person, count(*) AS times_reported FROM reports GROUP BY person;",
            ),
        ])
        .unwrap();
        let rows = engine
            .query("SELECT person, times_reported FROM people ORDER BY person", &[], 2)
            .unwrap();
        assert_eq!(rows, vec![vec![json!("A"), json!(2)], vec![json!("B"), json!(1)]]);
    }

    #[test]
    fn query_datasets_cannot_widen_the_sandbox_or_run_other_statements() {
        let dir = tempfile::tempdir().unwrap();
        let listed = sample(dir.path());
        let unlisted = dir.path().join("unlisted.csv");
        fs::write(&unlisted, "token\nhunter2\n").unwrap();
        let open = |sql: &str| {
            Engine::open(&[spec("sales", &listed, None), query_spec("derived", sql)])
                .err()
                .map(|e| e.to_string())
        };
        let read_unlisted = format!("SELECT * FROM read_csv({})", quote_literal(unlisted.to_str().unwrap()));
        assert!(open(&read_unlisted)
            .unwrap()
            .contains("dataset 'derived': invalid query"));
        assert!(open("DROP VIEW sales").unwrap().contains("invalid query"));
        assert!(open("SELECT 1) AS a; DROP VIEW sales; SELECT * FROM (SELECT 1")
            .unwrap()
            .contains("invalid query"));
        assert!(open("SELECT * FROM missing").unwrap().contains("invalid query"));
        assert_eq!(open("SELECT region FROM sales"), None);
    }
}
