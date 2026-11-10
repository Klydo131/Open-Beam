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
use std::sync::{Mutex, RwLock};

/// Most columns an uploaded file may have. Wider files are refused: every
/// check on the review page and every tidying rule scales with the width.
pub const MAX_UPLOAD_COLUMNS: usize = 1_000;

/// A hidden column holding each row's position in its file, so that rows
/// keep file order and "the first copy" of a repeated row is well defined.
const POSITION: &str = "__beam_position";

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
    #[error("cannot use upload directory '{0}'")]
    UploadDir(PathBuf),
    #[error("the file has more than {MAX_UPLOAD_COLUMNS} columns")]
    TooWide,
    #[error("uploads are not enabled")]
    UploadsDisabled,
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

/// Clean-up applied when an uploaded dataset is read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cleaning {
    /// Leave out rows in which every cell is empty.
    pub drop_empty_rows: bool,
    /// Keep one copy of rows that are identical in every column.
    pub drop_duplicate_rows: bool,
}

/// What a file looks like before it is saved as a dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct Preview {
    pub columns: Vec<Column>,
    /// The first rows, every value as text (`null` when empty).
    pub rows: Vec<Vec<Value>>,
    pub row_count: u64,
    /// Rows in which every cell is empty.
    pub empty_rows: u64,
    /// Rows that repeat an earlier row exactly, in every column. Empty rows
    /// are not counted here; they are counted in `empty_rows`.
    pub duplicate_rows: u64,
    /// Columns with no values at all.
    pub empty_columns: Vec<String>,
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
    /// `true` for datasets added at run time with [`Engine::add_upload`].
    pub uploaded: bool,
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
    datasets: RwLock<Vec<Dataset>>,
    /// Where files added at run time live. The only directory the sealed
    /// database may read besides the configured files.
    upload_dir: Option<PathBuf>,
}

impl Engine {
    pub fn open(specs: &[DatasetSpec]) -> Result<Self, EngineError> {
        Self::open_with_uploads(specs, None)
    }

    /// Like [`Engine::open`], and also allows datasets to be added later from
    /// files placed in `upload_dir`, which is created if needed.
    pub fn open_with_uploads(specs: &[DatasetSpec], upload_dir: Option<&Path>) -> Result<Self, EngineError> {
        let upload_dir = match upload_dir {
            Some(dir) => {
                let unusable = |_| EngineError::UploadDir(dir.to_path_buf());
                std::fs::create_dir_all(dir).map_err(unusable)?;
                Some(dir.canonicalize().map_err(unusable)?)
            }
            None => None,
        };
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

        // A trailing separator, so that "/data/uploads" does not also admit
        // "/data/uploads-other".
        let allowed_directories = match upload_dir.as_ref().and_then(|dir| dir.to_str()) {
            Some(dir) => quote_literal(&format!(
                "{}{}",
                dir.trim_end_matches(std::path::MAIN_SEPARATOR),
                std::path::MAIN_SEPARATOR
            )),
            None => String::new(),
        };
        conn.execute_batch(&format!(
            "SET allowed_paths = [{}]; SET allowed_directories = [{allowed_directories}]; \
             SET enable_external_access = false; SET lock_configuration = true;",
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
            datasets: RwLock::new(datasets),
            upload_dir,
        })
    }

    /// Every dataset, in the order they were added.
    pub fn datasets(&self) -> Vec<Dataset> {
        self.datasets.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn dataset(&self, name: &str) -> Option<Dataset> {
        self.datasets
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|d| d.name == name)
            .cloned()
    }

    /// The directory uploaded files must be placed in, if uploads are enabled.
    pub fn upload_dir(&self) -> Option<&Path> {
        self.upload_dir.as_deref()
    }

    /// Adds a dataset over `file`, a plain file name inside the upload
    /// directory. The file is read once now, so a file that cannot be parsed
    /// is refused here rather than on someone's first request.
    pub fn add_upload(&self, name: &str, file: &str, format: Format) -> Result<Dataset, EngineError> {
        self.add_upload_cleaned(name, file, format, Cleaning::default())
    }

    /// Like [`Engine::add_upload`], applying `cleaning` whenever the dataset
    /// is read. The file itself is never changed.
    pub fn add_upload_cleaned(
        &self,
        name: &str,
        file: &str,
        format: Format,
        cleaning: Cleaning,
    ) -> Result<Dataset, EngineError> {
        let (path, base) = self.upload_source(name, file, format)?;
        let mut datasets = self.datasets.write().unwrap_or_else(|e| e.into_inner());
        let taken = datasets.iter().any(|d| d.name.eq_ignore_ascii_case(name));
        if !is_identifier(name) || taken {
            return Err(EngineError::InvalidName(name.to_string()));
        }
        let spec = DatasetSpec {
            name: name.to_string(),
            source: Source::File {
                path,
                format: Some(format),
            },
            key: None,
        };
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let created = positioned_source(&conn, &base, cleaning)
            .and_then(|source| create_view(&conn, &spec, &source))
            .and_then(|dataset| {
                conn.execute_batch(&format!("SELECT * FROM {} LIMIT 0", quote_ident(name)))?;
                Ok(dataset)
            });
        match created {
            Ok(mut dataset) => {
                dataset.uploaded = true;
                datasets.push(dataset.clone());
                Ok(dataset)
            }
            Err(e) => {
                let _ = conn.execute_batch(&format!("DROP VIEW IF EXISTS {}", quote_ident(name)));
                Err(e)
            }
        }
    }

    /// Checks a file name from the upload directory and returns its path and
    /// the expression that reads it.
    fn upload_source(&self, label: &str, file: &str, format: Format) -> Result<(PathBuf, String), EngineError> {
        let dir = self.upload_dir.as_ref().ok_or(EngineError::UploadsDisabled)?;
        let plain = !file.is_empty()
            && file
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_')
            && !file.starts_with('.');
        let unreadable = || EngineError::Unreadable {
            dataset: label.to_string(),
            path: PathBuf::from(file),
        };
        if !plain {
            return Err(unreadable());
        }
        let path = dir.join(file).canonicalize().map_err(|_| unreadable())?;
        if path.parent() != Some(dir.as_path()) {
            return Err(unreadable());
        }
        let path_text = path.to_str().ok_or_else(unreadable)?;
        // Uploaded CSVs are sampled in full before column types are chosen,
        // so a value far down the file cannot make the whole file unreadable.
        let source = match format {
            Format::Csv => format!("read_csv({}, sample_size = -1)", quote_literal(path_text)),
            Format::Parquet => format!("read_parquet({})", quote_literal(path_text)),
        };
        Ok((path, source))
    }

    /// Reads a file from the upload directory without making it a dataset:
    /// its columns, its first `limit` rows as text, and the problems a person
    /// should know about before saving it.
    pub fn preview(&self, file: &str, format: Format, limit: usize) -> Result<Preview, EngineError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let (path, base) = self.upload_source("preview", file, format)?;
        let name = format!("__preview_{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let spec = DatasetSpec {
            name: name.clone(),
            source: Source::File {
                path,
                format: Some(format),
            },
            key: None,
        };
        let dataset = {
            let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            let source = positioned_source(&conn, &base, Cleaning::default())?;
            create_view(&conn, &spec, &source)?
        };
        let result = self.describe_preview(&name, &dataset, limit);
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute_batch(&format!("DROP VIEW IF EXISTS {}", quote_ident(&name)));
        result
    }

    fn describe_preview(&self, view: &str, dataset: &Dataset, limit: usize) -> Result<Preview, EngineError> {
        let columns: Vec<Column> = dataset.columns.iter().filter(|c| c.name != ROW_KEY).cloned().collect();
        let idents: Vec<String> = columns.iter().map(|c| quote_ident(&c.name)).collect();
        let from = quote_ident(view);
        let number = |rows: Vec<Vec<Value>>, index: usize| {
            rows.first()
                .and_then(|r| r.get(index))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };

        let all_empty = idents
            .iter()
            .map(|c| format!("{c} IS NULL"))
            .collect::<Vec<_>>()
            .join(" AND ");
        let counts = self.query(
            &format!(
                // Repeats are counted among rows with at least one value, so
                // blank rows are reported once, as empty, not again as repeats.
                "SELECT count(*), count(*) FILTER (WHERE {all_empty}), \
                 count(*) FILTER (WHERE NOT ({all_empty})) \
                 - (SELECT count(*) FROM (SELECT DISTINCT {cols} FROM {from} WHERE NOT ({all_empty}))) FROM {from}",
                cols = idents.join(", ")
            ),
            &[],
            3,
        )?;
        let filled = self.query(
            &format!(
                "SELECT {} FROM {from}",
                idents
                    .iter()
                    .map(|c| format!("count({c})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            &[],
            idents.len(),
        )?;
        let empty_columns = columns
            .iter()
            .enumerate()
            .filter(|(index, _)| number(filled.clone(), *index) == 0)
            .map(|(_, column)| column.name.clone())
            .collect();
        let rows = self.query(
            &format!(
                "SELECT {} FROM {from} ORDER BY {} LIMIT {limit}",
                idents
                    .iter()
                    .map(|c| format!("CAST({c} AS VARCHAR)"))
                    .collect::<Vec<_>>()
                    .join(", "),
                quote_ident(ROW_KEY)
            ),
            &[],
            idents.len(),
        )?;
        Ok(Preview {
            row_count: number(counts.clone(), 0),
            empty_rows: number(counts.clone(), 1),
            duplicate_rows: number(counts, 2),
            empty_columns,
            columns,
            rows,
        })
    }

    /// Removes a dataset added with [`Engine::add_upload`]. The file is left
    /// for the caller to delete.
    pub fn remove(&self, name: &str) -> Result<(), EngineError> {
        let mut datasets = self.datasets.write().unwrap_or_else(|e| e.into_inner());
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute_batch(&format!("DROP VIEW IF EXISTS {}", quote_ident(name)))?;
        datasets.retain(|d| d.name != name);
        Ok(())
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

/// Wraps an uploaded file's source so its rows carry their file position and
/// `cleaning` applies whenever it is read. Refuses files wider than
/// [`MAX_UPLOAD_COLUMNS`].
///
/// "Repeated" means identical in every column to an earlier row with at least
/// one value; the earliest copy is kept. Empty rows are only ever removed by
/// `drop_empty_rows`, so the rows removed are exactly the ones counted.
fn positioned_source(conn: &Connection, base: &str, cleaning: Cleaning) -> Result<String, EngineError> {
    let mut describe = conn.prepare(&format!("DESCRIBE SELECT * FROM {base}"))?;
    let names: Vec<String> = describe
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;
    if names.len() > MAX_UPLOAD_COLUMNS {
        return Err(EngineError::TooWide);
    }
    let idents: Vec<String> = names.iter().map(|n| quote_ident(n)).collect();
    let all_empty = if idents.is_empty() {
        "false".to_string()
    } else {
        idents
            .iter()
            .map(|c| format!("{c} IS NULL"))
            .collect::<Vec<_>>()
            .join(" AND ")
    };
    let position = quote_ident(POSITION);
    let filter = if cleaning.drop_empty_rows {
        format!(" WHERE NOT ({all_empty})")
    } else {
        String::new()
    };
    let qualify = if cleaning.drop_duplicate_rows {
        format!(
            " QUALIFY ({all_empty}) OR row_number() OVER (PARTITION BY {} ORDER BY {position}) = 1",
            idents.join(", ")
        )
    } else {
        String::new()
    };
    Ok(format!(
        "(SELECT * FROM (SELECT *, row_number() OVER () AS {position} FROM {base}) AS numbered{filter}{qualify}) AS positioned"
    ))
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
    let positioned = source_columns.iter().any(|(name, _)| name == POSITION);
    for (source_name, source_type) in source_columns.iter().filter(|(name, _)| name != POSITION) {
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
            let order = if positioned {
                format!("ORDER BY {}", quote_ident(POSITION))
            } else {
                String::new()
            };
            select.insert(0, format!("row_number() OVER ({order}) AS {}", quote_ident(ROW_KEY)));
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
        uploaded: false,
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
    // Words the filter language reserves cannot be column names there.
    if ["null", "true", "false", "not", "and", "or"].contains(&base.to_ascii_lowercase().as_str()) {
        base.push('_');
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

    #[test]
    fn uploads_are_added_and_removed_at_run_time_inside_their_directory_only() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        let engine = Engine::open_with_uploads(&[spec("sales", &sample(dir.path()), None)], Some(&uploads)).unwrap();
        fs::write(uploads.join("a1.csv"), "team,score\nred,3\nblue,5\n").unwrap();

        let added = engine.add_upload("scores", "a1.csv", Format::Csv).unwrap();
        assert_eq!((added.key.as_str(), added.uploaded), (ROW_KEY, true));
        assert!(!engine.dataset("sales").unwrap().uploaded);
        assert_eq!(engine.datasets().len(), 2);
        let rows = engine.query("SELECT sum(score) FROM scores", &[], 1).unwrap();
        assert_eq!(rows, vec![vec![json!(8)]]);

        // Names must be new and well formed; files must be plain names inside the directory.
        assert!(matches!(
            engine.add_upload("SALES", "a1.csv", Format::Csv),
            Err(EngineError::InvalidName(_))
        ));
        assert!(matches!(
            engine.add_upload("bad name", "a1.csv", Format::Csv),
            Err(EngineError::InvalidName(_))
        ));
        let outside = sample(dir.path());
        for file in [
            "../it's data.csv",
            "",
            ".hidden",
            "missing.csv",
            outside.to_str().unwrap(),
        ] {
            assert!(
                matches!(
                    engine.add_upload("other", file, Format::Csv),
                    Err(EngineError::Unreadable { .. })
                ),
                "{file}"
            );
        }
        // A file that does not parse is refused and leaves nothing behind.
        fs::write(uploads.join("b2.parquet"), "not parquet").unwrap();
        assert!(engine.add_upload("broken", "b2.parquet", Format::Parquet).is_err());
        assert!(engine.dataset("broken").is_none());
        assert!(engine.query("SELECT * FROM broken", &[], 0).is_err());

        // The sandbox still holds: siblings of the upload directory stay unreadable.
        let sibling = dir.path().join("uploads-other");
        fs::create_dir(&sibling).unwrap();
        fs::write(sibling.join("secret.csv"), "token\nhunter2\n").unwrap();
        let read = format!(
            "SELECT * FROM read_csv({})",
            quote_literal(sibling.join("secret.csv").to_str().unwrap())
        );
        assert!(engine.query(&read, &[], 1).is_err());

        engine.remove("scores").unwrap();
        assert!(engine.dataset("scores").is_none());
        assert!(engine.query("SELECT * FROM scores", &[], 0).is_err());
        assert!(matches!(
            Engine::open(&[]).unwrap().add_upload("x_y", "a1.csv", Format::Csv),
            Err(EngineError::UploadsDisabled)
        ));
    }

    #[test]
    fn previews_report_problems_and_cleaning_applies_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        let engine = Engine::open_with_uploads(&[], Some(&uploads)).unwrap();
        fs::write(
            uploads.join("p1.csv"),
            "member,event,note\nM-1,joined,\nM-2,joined,\n,,\nM-1,joined,\nM-3,left,\n,,\n",
        )
        .unwrap();

        let preview = engine.preview("p1.csv", Format::Csv, 3).unwrap();
        assert_eq!(
            preview.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["member", "event", "note"]
        );
        assert_eq!(
            (preview.row_count, preview.empty_rows, preview.duplicate_rows),
            (6, 2, 1)
        );
        assert_eq!(preview.empty_columns, vec!["note".to_string()]);
        assert_eq!(preview.rows.len(), 3);
        assert_eq!(preview.rows[0], vec![json!("M-1"), json!("joined"), Value::Null]);
        assert!(engine.datasets().is_empty(), "a preview is not a dataset");
        assert!(
            engine.query("SELECT * FROM __preview_1", &[], 0).is_err(),
            "and leaves nothing behind"
        );

        let cleaned = Cleaning {
            drop_empty_rows: true,
            drop_duplicate_rows: true,
        };
        engine
            .add_upload_cleaned("members", "p1.csv", Format::Csv, cleaned)
            .unwrap();
        let count = |sql: &str| engine.query(sql, &[], 1).unwrap()[0][0].clone();
        assert_eq!(count("SELECT count(*) FROM members"), json!(3));
        engine
            .add_upload_cleaned(
                "members_empty_only",
                "p1.csv",
                Format::Csv,
                Cleaning {
                    drop_empty_rows: true,
                    drop_duplicate_rows: false,
                },
            )
            .unwrap();
        assert_eq!(count("SELECT count(*) FROM members_empty_only"), json!(4));
        engine.add_upload("members_raw", "p1.csv", Format::Csv).unwrap();
        assert_eq!(count("SELECT count(*) FROM members_raw"), json!(6));
        assert_eq!(
            fs::read_to_string(uploads.join("p1.csv")).unwrap().lines().count(),
            7,
            "the file is untouched"
        );

        assert!(engine.preview("../p1.csv", Format::Csv, 3).is_err());
        fs::write(uploads.join("bad.parquet"), "nope").unwrap();
        assert!(engine.preview("bad.parquet", Format::Parquet, 3).is_err());
    }

    #[test]
    fn removing_repeats_keeps_the_first_copy_in_file_order_and_never_touches_empty_rows() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        let engine = Engine::open_with_uploads(&[], Some(&uploads)).unwrap();
        // 9 rows: 2 empty, and repeats of rows 1 and 2 (three extra copies).
        fs::write(
            uploads.join("d.csv"),
            "id,name,city\n1,Ann,Oslo\n2,Bob,Rome\n,,\n1,Ann,Oslo\n3,Cid,\n,,\n1,Ann,Oslo\n2,Bob,Rome\n4,,\n",
        )
        .unwrap();
        let preview = engine.preview("d.csv", Format::Csv, 20).unwrap();
        assert_eq!(
            (preview.row_count, preview.empty_rows, preview.duplicate_rows),
            (9, 2, 3)
        );

        let rows = |name: &str, cleaning| {
            engine.add_upload_cleaned(name, "d.csv", Format::Csv, cleaning).unwrap();
            engine
                .query(
                    &format!("SELECT _row, CAST(id AS VARCHAR) FROM {name} ORDER BY _row"),
                    &[],
                    2,
                )
                .unwrap()
        };
        let ids = |rows: Vec<Vec<Value>>| rows.into_iter().map(|r| r[1].clone()).collect::<Vec<_>>();
        let repeats_only = rows(
            "repeats_only",
            Cleaning {
                drop_empty_rows: false,
                drop_duplicate_rows: true,
            },
        );
        assert_eq!(repeats_only.len(), 6, "9 rows minus exactly the 3 repeats reported");
        assert_eq!(
            ids(repeats_only.clone()),
            vec![json!("1"), json!("2"), Value::Null, json!("3"), Value::Null, json!("4")],
            "first copies, in file order, empty rows kept"
        );
        let keys: Vec<Value> = repeats_only.iter().map(|r| r[0].clone()).collect();
        assert_eq!(keys, (1..=6).map(|n| json!(n)).collect::<Vec<_>>());
        let both = rows(
            "tidy_both",
            Cleaning {
                drop_empty_rows: true,
                drop_duplicate_rows: true,
            },
        );
        assert_eq!(ids(both), vec![json!("1"), json!("2"), json!("3"), json!("4")]);
        let empty_only = rows(
            "empty_only",
            Cleaning {
                drop_empty_rows: true,
                drop_duplicate_rows: false,
            },
        );
        assert_eq!(empty_only.len(), 7);
        let raw = rows("raw", Cleaning::default());
        assert_eq!(ids(raw)[3], json!("1"), "untidied rows stay in file order");
    }

    #[test]
    fn large_uploads_keep_file_order_after_removing_repeats() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        let engine = Engine::open_with_uploads(&[], Some(&uploads)).unwrap();
        let mut text = String::from("n,group\n");
        for n in 0..300_000 {
            text.push_str(&format!("{},g{}\n", n / 2, n % 7));
        }
        fs::write(uploads.join("big.csv"), text).unwrap();
        engine
            .add_upload_cleaned(
                "big",
                "big.csv",
                Format::Csv,
                Cleaning {
                    drop_empty_rows: false,
                    drop_duplicate_rows: true,
                },
            )
            .unwrap();
        let ordered = engine
            .query(
                "SELECT count(*), count(*) FILTER (WHERE n_prev > n OR (n_prev = n AND g_prev = \"group\")) FROM \
                 (SELECT n, \"group\", lag(n) OVER (ORDER BY _row) AS n_prev, lag(\"group\") OVER (ORDER BY _row) AS g_prev FROM big)",
                &[],
                2,
            )
            .unwrap();
        assert_eq!(ordered[0][1], json!(0), "rows follow the file and no repeat survives");
        let first = engine
            .query("SELECT n, \"group\" FROM big WHERE _row = 1", &[], 2)
            .unwrap();
        assert_eq!(first, vec![vec![json!(0), json!("g0")]]);
    }

    #[test]
    fn uploads_are_bounded_and_read_whole_files_before_choosing_types() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        let engine = Engine::open_with_uploads(&[], Some(&uploads)).unwrap();
        let header: Vec<String> = (0..1_001).map(|n| format!("c{n}")).collect();
        let row: Vec<String> = (0..1_001).map(|n| n.to_string()).collect();
        fs::write(
            uploads.join("wide.csv"),
            format!("{}\n{}\n", header.join(","), row.join(",")),
        )
        .unwrap();
        assert!(matches!(
            engine.preview("wide.csv", Format::Csv, 5),
            Err(EngineError::TooWide)
        ));
        assert!(matches!(
            engine.add_upload("wide", "wide.csv", Format::Csv),
            Err(EngineError::TooWide)
        ));

        let mut late = String::from("amount\n");
        for n in 0..30_000 {
            late.push_str(&format!("{n}\n"));
        }
        late.push_str("not a number\n");
        fs::write(uploads.join("late.csv"), late).unwrap();
        let preview = engine.preview("late.csv", Format::Csv, 1).unwrap();
        assert_eq!((preview.row_count, preview.columns[0].ty), (30_001, ColumnType::Text));

        fs::write(uploads.join("words.csv"), "null,not,true,x\na,b,c,d\n").unwrap();
        let names: Vec<String> = engine
            .preview("words.csv", Format::Csv, 1)
            .unwrap()
            .columns
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["null_", "not_", "true_", "x"]);
    }

    #[test]
    fn real_parquet_files_work_for_uploads_and_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        // Written by a separate, unsealed DuckDB: a genuine Parquet file.
        let writer = Connection::open_in_memory().unwrap();
        let target = uploads.join("p.parquet");
        writer
            .execute_batch(&format!(
                "COPY (SELECT range AS id, 'item ' || range AS label FROM range(5)) TO {} (FORMAT parquet)",
                quote_literal(target.to_str().unwrap())
            ))
            .unwrap();
        let configured = dir.path().join("c.parquet");
        fs::copy(&target, &configured).unwrap();

        let engine = Engine::open_with_uploads(&[spec("configured", &configured, None)], Some(&uploads)).unwrap();
        assert_eq!(
            engine.query("SELECT count(*) FROM configured", &[], 1).unwrap()[0][0],
            json!(5)
        );
        let preview = engine.preview("p.parquet", Format::Parquet, 2).unwrap();
        assert_eq!((preview.row_count, preview.rows[1][1].clone()), (5, json!("item 1")));
        engine.add_upload("items", "p.parquet", Format::Parquet).unwrap();
        assert_eq!(
            engine.query("SELECT label FROM items WHERE _row = 5", &[], 1).unwrap()[0][0],
            json!("item 4")
        );
    }
}
