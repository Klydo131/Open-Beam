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

mod runtime;
use runtime::{parse_size, Gate, Metrics};
pub use runtime::{Limits, Outcome, SlowQuery, Stats, LATENCY_BUCKETS_MS};

/// Bumped whenever the shape of cached files changes, so old ones are rebuilt.
const CACHE_VERSION: u32 = 2;
/// Streamed queries that may run at once, apart from the others.
const STREAMS_AT_ONCE: usize = 2;
/// Folder inside the upload directory for column-store copies of datasets.
const CACHE_DIR: &str = ".cache";
/// Folder inside the upload directory for temporary files of queries too
/// large for memory.
const SPILL_DIR: &str = ".spill";

/// Most columns an uploaded file may have. Wider files are refused: every
/// check on the review page and every tidying rule scales with the width.
pub const MAX_UPLOAD_COLUMNS: usize = 1_000;

/// How long a query may run unless configured otherwise.
pub const DEFAULT_QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
    /// The file has a column whose name Open Beam uses for its own bookkeeping.
    #[error("the file has a column named {POSITION}, which is reserved")]
    ReservedColumn,
    #[error("the query took longer than the time limit")]
    TimedOut,
    #[error("uploads are not enabled")]
    UploadsDisabled,
    /// The query needed more memory than the engine is allowed.
    #[error("the query needed more memory than allowed")]
    OutOfMemory,
    /// Every place for a running query stayed taken for the whole time limit.
    #[error("too many queries are running")]
    Busy,
    /// The engine's parser refused a statement. The message is safe to show.
    #[error("{0}")]
    Syntax(String),
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
    /// Held while an upload becomes a dataset, so two uploads cannot claim
    /// one name. Reading and type-checking the file happen on a connection of
    /// their own, so other people's queries carry on meanwhile.
    adding: Mutex<()>,
    /// Longest a single query may run before it is cancelled, in milliseconds.
    query_timeout_ms: std::sync::atomic::AtomicU64,
    datasets: RwLock<Vec<Dataset>>,
    /// Column-store copies of file datasets, by dataset name.
    cache: Mutex<std::collections::HashMap<String, PathBuf>>,
    gate: Gate,
    /// Places for queries streamed to a client (PostgreSQL access). A
    /// streaming query keeps its place while the client reads, so slow
    /// readers get places of their own and cannot hold everyone else's.
    stream_gate: Gate,
    metrics: Metrics,
    memory_limit_bytes: u64,
    spill_limit_bytes: u64,
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
        Self::open_with_options(specs, upload_dir, &Limits::default())
    }

    /// Like [`Engine::open_with_uploads`], within `limits`.
    ///
    /// With an upload directory, every file dataset is also kept as a
    /// column-store copy there, built once: queries then scan compressed
    /// columns instead of parsing the original file each time. Queries too
    /// large for memory spill to a capped folder beside it.
    pub fn open_with_options(
        specs: &[DatasetSpec],
        upload_dir: Option<&Path>,
        limits: &Limits,
    ) -> Result<Self, EngineError> {
        let upload_dir = match upload_dir {
            Some(dir) => {
                let unusable = |_| EngineError::UploadDir(dir.to_path_buf());
                std::fs::create_dir_all(dir).map_err(unusable)?;
                Some(dir.canonicalize().map_err(unusable)?)
            }
            None => None,
        };
        let conn = Connection::open_in_memory()?;
        let (memory_limit_bytes, spill_limit_bytes) = apply_limits(&conn, upload_dir.as_deref(), limits)?;
        let mut cache = std::collections::HashMap::new();
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
            if let (Some(dir), Source::File { path, format }) = (upload_dir.as_deref(), &spec.source) {
                let key = cache_key(path, &format!("{format:?}|{:?}", spec.key));
                match key.and_then(|key| materialize(&conn, dir, &dataset, key).ok()) {
                    Some(file) => {
                        cache.insert(dataset.name.clone(), file);
                    }
                    None => eprintln!("beam: dataset '{}' is read from its file directly", dataset.name),
                }
            }
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
             SET enable_external_access = false; \
             SET autoinstall_known_extensions = false; SET autoload_known_extensions = false; \
             SET allow_community_extensions = false; SET lock_configuration = true;",
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
            query_timeout_ms: std::sync::atomic::AtomicU64::new(DEFAULT_QUERY_TIMEOUT.as_millis() as u64),
            conn: Mutex::new(conn),
            adding: Mutex::new(()),
            datasets: RwLock::new(datasets),
            cache: Mutex::new(cache),
            gate: Gate::new(limits.max_concurrent),
            stream_gate: Gate::new(STREAMS_AT_ONCE.min(limits.max_concurrent)),
            metrics: Metrics::new(),
            memory_limit_bytes,
            spill_limit_bytes,
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
        let _adding = self.adding.lock().unwrap_or_else(|e| e.into_inner());
        let taken = || {
            self.datasets
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|d| d.name.eq_ignore_ascii_case(name))
        };
        if !is_identifier(name) || taken() {
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
        // The slow part: the whole file is read to choose column types.
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        let created = positioned_source(&conn, &base, cleaning)
            .and_then(|source| create_view(&conn, &spec, &source))
            .and_then(|dataset| {
                conn.execute_batch(&format!("SELECT * FROM {} LIMIT 0", quote_ident(name)))?;
                Ok(dataset)
            });
        match created {
            Ok(mut dataset) => {
                dataset.uploaded = true;
                // A column-store copy makes later reads fast; without one the
                // file is still read directly, so a failure here only costs speed.
                let flags = format!("upload|{format:?}|{cleaning:?}");
                let dir = self.upload_dir.as_deref();
                let key = spec_path(&spec).and_then(|path| cache_key(path, &flags));
                if let (Some(dir), Some(key)) = (dir, key) {
                    match materialize(&conn, dir, &dataset, key) {
                        Ok(file) => {
                            self.cache
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(dataset.name.clone(), file);
                        }
                        Err(e) => eprintln!("beam: dataset '{name}' is read from its file directly: {e}"),
                    }
                }
                self.datasets
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(dataset.clone());
                Ok(dataset)
            }
            Err(e) => {
                // Safe: the name was free and no other upload can take it meanwhile.
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
        // On a connection of its own, so reading the file holds no one up.
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        let dataset = {
            let source = positioned_source(&conn, &base, Cleaning::default())?;
            create_view(&conn, &spec, &source)?
        };
        let result = self.describe_preview(&name, &dataset, limit);
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
        if let Some(file) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).remove(name) {
            let _ = std::fs::remove_file(file);
        }
        Ok(())
    }

    /// Deletes column-store copies that no dataset uses any more: left by a
    /// removed dataset, or by a file that has since changed. Call once every
    /// dataset, uploads included, has been added.
    pub fn prune_cache(&self) -> usize {
        let Some(dir) = self.upload_dir.as_ref().map(|d| d.join(CACHE_DIR)) else {
            return 0;
        };
        let keep: HashSet<PathBuf> = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        let mut removed = 0;
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if !keep.contains(&path) && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// How the engine is doing right now. Cheap: counters, the engine's own
    /// memory report, and the sizes of the cache files.
    pub fn stats(&self) -> Stats {
        use std::sync::atomic::Ordering::Relaxed;
        let (memory_used_bytes, spill_used_bytes) = self
            .conn
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .try_clone()
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT CAST(coalesce(sum(memory_usage_bytes), 0) AS BIGINT), \
                     CAST(coalesce(sum(temporary_storage_bytes), 0) AS BIGINT) FROM duckdb_memory()",
                    [],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .ok()
            })
            .map_or((0, 0), |(m, s)| (m.max(0) as u64, s.max(0) as u64));
        let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let cache_bytes = cache
            .values()
            .filter_map(|file| std::fs::metadata(file).ok())
            .map(|m| m.len())
            .sum();
        let m = &self.metrics;
        Stats {
            uptime: m.uptime(),
            datasets: self.datasets.read().unwrap_or_else(|e| e.into_inner()).len(),
            cached_datasets: cache.len(),
            cache_bytes,
            memory_limit_bytes: self.memory_limit_bytes,
            memory_used_bytes,
            spill_used_bytes,
            spill_limit_bytes: self.spill_limit_bytes,
            queries_running: self.gate.running() + self.stream_gate.running(),
            max_concurrent: self.gate.max() + self.stream_gate.max(),
            queries_total: m.total.load(Relaxed),
            queries_failed: m.failed.load(Relaxed),
            queries_timed_out: m.timed_out.load(Relaxed),
            queries_out_of_memory: m.out_of_memory.load(Relaxed),
            queries_busy: m.busy.load(Relaxed),
            query_seconds_total: m.micros.load(Relaxed) as f64 / 1e6,
            latency_buckets: m.buckets.iter().map(|b| b.load(Relaxed)).collect(),
            slow: m.slow(),
        }
    }

    /// Sets the longest a single query may run before it is cancelled.
    pub fn set_query_timeout(&self, limit: std::time::Duration) {
        let millis = u64::try_from(limit.as_millis()).unwrap_or(u64::MAX).max(1);
        self.query_timeout_ms
            .store(millis, std::sync::atomic::Ordering::Relaxed);
    }

    /// Runs a statement and returns its rows, each `width` values wide. A
    /// statement still running after the time limit is cancelled and
    /// reported as [`EngineError::TimedOut`].
    pub fn query(&self, sql: &str, params: &[Param], width: usize) -> Result<Vec<Vec<Value>>, EngineError> {
        // Each query gets its own connection so slow queries do not queue
        // behind the lock; the lock only guards the clone itself.
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        self.within_time_limit(&conn, sql, || Self::run(&conn, sql, params, width))
    }

    /// Runs `work` on `conn` once a place among the running queries is
    /// free, cancelling it if the wait and the work together outlast the
    /// time limit. `label` is the statement, for the counters and the
    /// slow-query log; housekeeping passes an empty label and is not counted.
    fn within_time_limit<T>(
        &self,
        conn: &Connection,
        label: &str,
        work: impl FnOnce() -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        self.within_time_limit_on(&self.gate, conn, label, work)
    }

    fn within_time_limit_on<T>(
        &self,
        gate: &Gate,
        conn: &Connection,
        label: &str,
        work: impl FnOnce() -> Result<T, EngineError>,
    ) -> Result<T, EngineError> {
        let limit = std::time::Duration::from_millis(self.query_timeout_ms.load(std::sync::atomic::Ordering::Relaxed));
        let started = std::time::Instant::now();
        let record = |outcome: Outcome| {
            if !label.is_empty() {
                self.metrics.record(label, started.elapsed(), outcome);
            }
        };
        let Some(_slot) = gate.enter(limit) else {
            record(Outcome::Busy);
            return Err(EngineError::Busy);
        };
        let left = limit
            .saturating_sub(started.elapsed())
            .max(std::time::Duration::from_millis(1));
        let result = limited(conn, left, work);
        record(match &result {
            Ok(_) => Outcome::Ok,
            Err(EngineError::TimedOut) => Outcome::TimedOut,
            Err(EngineError::OutOfMemory) => Outcome::OutOfMemory,
            Err(_) => Outcome::Failed,
        });
        result
    }

    /// Runs a `SELECT` and returns its result as a Parquet file. The file is
    /// written inside the upload directory, read back and deleted, so the
    /// sealed database never writes anywhere else. Needs uploads enabled.
    pub fn query_to_parquet(&self, sql: &str, params: &[Param]) -> Result<Vec<u8>, EngineError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let dir = self.upload_dir.as_ref().ok_or(EngineError::UploadsDisabled)?;
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("export-{}-{n}.parquet", std::process::id()));
        let path_text = path.to_str().ok_or(EngineError::UploadsDisabled)?.to_string();
        let table = format!("__export_{n}");
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        let result = self.within_time_limit(&conn, sql, || {
            let bound: Vec<DuckValue> = params.iter().map(to_duck).collect();
            conn.execute(
                &format!("CREATE TEMP TABLE {} AS {sql}", quote_ident(&table)),
                params_from_iter(bound),
            )?;
            conn.execute_batch(&format!(
                "COPY {} TO {} (FORMAT parquet)",
                quote_ident(&table),
                quote_literal(&path_text)
            ))?;
            std::fs::read(&path).map_err(|_| EngineError::UploadDir(path.clone()))
        });
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", quote_ident(&table)));
        let _ = std::fs::remove_file(&path);
        result
    }

    /// Parses one statement with the engine's own parser and returns its
    /// syntax tree as JSON. Only `SELECT` statements can be parsed this way,
    /// and checking the tree that will actually run leaves no room for two
    /// parsers to disagree about what a statement means.
    pub fn parse_select(&self, sql: &str) -> Result<Value, EngineError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        let text: String = self.within_time_limit(&conn, "", || {
            Ok(conn.query_row("SELECT json_serialize_sql(?::VARCHAR)", [sql], |row| row.get(0))?)
        })?;
        let tree: Value =
            serde_json::from_str(&text).map_err(|_| EngineError::Syntax("unreadable syntax tree".into()))?;
        if tree["error"] == Value::Bool(true) {
            let message = tree["error_message"].as_str().unwrap_or("syntax error");
            return Err(EngineError::Syntax(message.to_string()));
        }
        Ok(tree)
    }

    /// Turns a syntax tree from [`Engine::parse_select`] back into SQL.
    pub fn render_select(&self, tree: &Value) -> Result<String, EngineError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        let json = tree.to_string();
        self.within_time_limit(&conn, "", || {
            Ok(
                conn.query_row("SELECT json_deserialize_sql(?::JSON)", [json.as_str()], |row| {
                    row.get::<_, String>(0)
                })?,
            )
        })
    }

    /// The names and engine types of the columns `sql` returns, without
    /// running it.
    pub fn describe_select(&self, sql: &str) -> Result<Vec<(String, String)>, EngineError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        self.within_time_limit(&conn, "", || {
            let mut describe = conn.prepare(&format!("DESCRIBE SELECT * FROM ({sql}) AS beam_result"))?;
            let columns = describe
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<_, _>>()?;
            Ok(columns)
        })
    }

    /// Runs `sql` with every value turned into text, handing rows to `each`
    /// as they are produced; `each` returns `false` to stop early. Returns
    /// the number of rows handed over. Runs under the time limit.
    pub fn stream_text(
        &self,
        sql: &str,
        params: &[Param],
        width: usize,
        mut each: impl FnMut(Vec<Option<String>>) -> bool,
    ) -> Result<u64, EngineError> {
        let columns = (1..=width)
            .map(|n| format!("CAST(#{n} AS VARCHAR)"))
            .collect::<Vec<_>>()
            .join(", ");
        let wrapped = format!("SELECT {columns} FROM ({sql}) AS beam_result");
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner()).try_clone()?;
        self.within_time_limit_on(&self.stream_gate, &conn, sql, || {
            let mut statement = conn.prepare(&wrapped)?;
            let mut rows = statement.query(params_from_iter(params.iter().map(to_duck)))?;
            let mut sent = 0;
            while let Some(row) = rows.next()? {
                let mut values = Vec::with_capacity(width);
                for index in 0..width {
                    values.push(row.get::<_, Option<String>>(index)?);
                }
                sent += 1;
                if !each(values) {
                    break;
                }
            }
            Ok(sent)
        })
    }

    fn run(conn: &Connection, sql: &str, params: &[Param], width: usize) -> Result<Vec<Vec<Value>>, EngineError> {
        let mut statement = conn.prepare(sql)?;
        let mut rows = statement.query(params_from_iter(params.iter().map(to_duck)))?;
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

/// Sets the memory limit and where and how much a query too large for
/// memory may spill; returns the limits in force, in bytes.
fn apply_limits(conn: &Connection, upload_dir: Option<&Path>, limits: &Limits) -> Result<(u64, u64), EngineError> {
    // DuckDB's own default is 80% of the computer's memory. A server shares
    // the machine, so half is the default here, as DuckDB's guide suggests.
    let current = |conn: &Connection| {
        conn.query_row("SELECT current_setting('memory_limit')", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|text| parse_size(&text))
    };
    let wanted = limits
        .memory_limit_bytes
        .or_else(|| current(conn).map(|default| default / 8 * 5))
        .unwrap_or(2 * 1024 * 1024 * 1024)
        .max(64 * 1024 * 1024);
    conn.execute_batch(&format!("SET memory_limit = '{}KiB'", wanted / 1024))?;
    // Hand freed memory back to the operating system after each task larger
    // than 16 MB, in the background, rather than keeping up to 128 MB of it.
    conn.execute_batch("SET allocator_flush_threshold = '16MB'; SET allocator_background_threads = true;")?;
    let spill = match upload_dir {
        Some(dir) => {
            let spill = dir.join(SPILL_DIR);
            std::fs::create_dir_all(&spill).map_err(|_| EngineError::UploadDir(spill.clone()))?;
            // Left over from a previous run that stopped mid-query.
            for entry in std::fs::read_dir(&spill).into_iter().flatten().flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
            let text = spill.to_str().ok_or_else(|| EngineError::UploadDir(spill.clone()))?;
            conn.execute_batch(&format!(
                "SET temp_directory = {}; SET max_temp_directory_size = '{}KiB';",
                quote_literal(text),
                limits.spill_limit_bytes / 1024
            ))?;
            limits.spill_limit_bytes
        }
        // Nowhere safe to spill: a query too large for memory fails instead.
        None => {
            conn.execute_batch("SET temp_directory = '';")?;
            0
        }
    };
    Ok((current(conn).unwrap_or(wanted), spill))
}

fn spec_path(spec: &DatasetSpec) -> Option<&Path> {
    match &spec.source {
        Source::File { path, .. } => Some(path),
        Source::Query(_) => None,
    }
}

/// Identifies one version of a source file: its path, size and time of
/// last change, with `extra` for anything else that changes the result. A
/// changed file gets a new key, so a stale copy is never read.
/// The first and last megabyte of a file (all of it, if smaller).
fn edges(path: &Path, len: u64) -> Option<[Vec<u8>; 2]> {
    use std::io::{Read, Seek, SeekFrom};
    const EDGE: u64 = 1024 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = Vec::new();
    (&mut file).take(EDGE).read_to_end(&mut head).ok()?;
    let mut tail = Vec::new();
    if len > EDGE {
        file.seek(SeekFrom::Start(len.saturating_sub(EDGE).max(EDGE))).ok()?;
        file.take(EDGE).read_to_end(&mut tail).ok()?;
    }
    Some([head, tail])
}

fn cache_key(path: &Path, extra: &str) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let meta = std::fs::metadata(path).ok()?;
    let changed = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        CACHE_VERSION,
        path.canonicalize().ok()?,
        meta.len(),
        changed.as_nanos(),
        extra,
    )
        .hash(&mut hasher);
    // Some filesystems keep times to the second, and copying tools can set
    // them back, so a file rewritten at the same size could look unchanged.
    // Its first and last megabyte are part of the key too.
    for chunk in edges(path, meta.len())? {
        chunk.hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Writes a column-store copy of `dataset` (as its view now reads it) into
/// the cache folder, unless one for this exact source already exists, and
/// points the view at it. The copy is written under a temporary name and
/// renamed, so a half-written file is never read.
fn materialize(conn: &Connection, dir: &Path, dataset: &Dataset, key: u64) -> Result<PathBuf, EngineError> {
    let folder = dir.join(CACHE_DIR);
    std::fs::create_dir_all(&folder).map_err(|_| EngineError::UploadDir(folder.clone()))?;
    let file = folder.join(format!("{}-{key:016x}.parquet", dataset.name.to_ascii_lowercase()));
    let text = |p: &Path| {
        p.to_str()
            .map(str::to_string)
            .ok_or_else(|| EngineError::UploadDir(p.to_path_buf()))
    };
    if !file.exists() {
        let partial = folder.join(format!(
            "{}-{key:016x}.{}.partial",
            dataset.name.to_ascii_lowercase(),
            std::process::id()
        ));
        let order = if dataset.key == ROW_KEY {
            format!(" ORDER BY {}", quote_ident(ROW_KEY))
        } else {
            String::new()
        };
        let written = conn.execute_batch(&format!(
            "COPY (SELECT * FROM {}{order}) TO {} (FORMAT parquet, COMPRESSION zstd)",
            quote_ident(&dataset.name),
            quote_literal(&text(&partial)?)
        ));
        if let Err(e) = written
            .map_err(EngineError::from)
            .and_then(|()| std::fs::rename(&partial, &file).map_err(|_| EngineError::UploadDir(file.clone())))
        {
            let _ = std::fs::remove_file(&partial);
            return Err(e);
        }
    }
    conn.execute_batch(&format!(
        "CREATE OR REPLACE VIEW {} AS SELECT * FROM read_parquet({})",
        quote_ident(&dataset.name),
        quote_literal(&text(&file)?)
    ))?;
    Ok(file)
}

/// How often a cancellation is repeated once the time limit has passed.
const CANCEL_EVERY: std::time::Duration = std::time::Duration::from_millis(20);

/// Runs `work` on `conn`, cancelling it if it outlasts `limit`.
///
/// The engine discards a cancellation that arrives before a statement has
/// started, so one signal is not enough: on a busy machine the limit can pass
/// while a statement is still being prepared, and that statement would then
/// run with nothing left to stop it. The signal is repeated until `work`
/// returns, which also stops any later statement `work` goes on to run.
fn limited<T>(
    conn: &Connection,
    limit: std::time::Duration,
    work: impl FnOnce() -> Result<T, EngineError>,
) -> Result<T, EngineError> {
    use std::sync::mpsc::RecvTimeoutError::Timeout;
    let handle = conn.interrupt_handle();
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let watchdog = {
        let fired = fired.clone();
        std::thread::spawn(move || {
            if finished.recv_timeout(limit) != Err(Timeout) {
                return;
            }
            fired.store(true, std::sync::atomic::Ordering::SeqCst);
            loop {
                handle.interrupt();
                if finished.recv_timeout(CANCEL_EVERY) != Err(Timeout) {
                    return;
                }
            }
        })
    };
    let result = work();
    let _ = done.send(());
    let _ = watchdog.join();
    match result {
        // Only a statement the engine says it interrupted timed out. Any
        // other failure is reported as itself, even when it took too long.
        Err(EngineError::Database(e))
            if fired.load(std::sync::atomic::Ordering::SeqCst) && e.to_string().starts_with("INTERRUPT Error") =>
        {
            Err(EngineError::TimedOut)
        }
        Err(EngineError::Database(e)) if e.to_string().starts_with("Out of Memory Error") => {
            Err(EngineError::OutOfMemory)
        }
        other => other,
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
    // Column names compare without regard to case, so any spelling would
    // clash with the hidden column that keeps the file's row order.
    if names.iter().any(|n| n.eq_ignore_ascii_case(POSITION)) {
        return Err(EngineError::ReservedColumn);
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

fn to_duck(param: &Param) -> DuckValue {
    match param {
        Param::Null => DuckValue::Null,
        Param::Bool(b) => DuckValue::Boolean(*b),
        Param::Int(i) => DuckValue::BigInt(*i),
        Param::Float(f) => DuckValue::Double(*f),
        Param::Text(s) => DuckValue::Text(s.clone()),
    }
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
        // No extension can be loaded, even one already on disk, and the
        // switches that would allow it are locked off.
        for name in [
            "autoload_known_extensions",
            "autoinstall_known_extensions",
            "allow_community_extensions",
        ] {
            let value = engine
                .query(&format!("SELECT current_setting('{name}')"), &[], 1)
                .unwrap();
            assert_eq!(value, vec![vec![json!(false)]], "{name}");
            assert!(
                engine.query(&format!("SET {name} = true"), &[], 0).is_err(),
                "{name} is locked"
            );
        }
        assert!(engine.query("LOAD aws", &[], 0).is_err());
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
        fs::write(uploads.join("clash.csv"), "id,__BEAM_Position\n1,2\n2,1\n").unwrap();
        assert!(matches!(
            engine.preview("clash.csv", Format::Csv, 5),
            Err(EngineError::ReservedColumn)
        ));
        assert!(matches!(
            engine.add_upload("clash", "clash.csv", Format::Csv),
            Err(EngineError::ReservedColumn)
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

    #[test]
    fn slow_queries_are_cancelled_at_the_time_limit() {
        let engine = Engine::open(&[]).unwrap();
        engine.set_query_timeout(std::time::Duration::from_millis(200));
        let started = std::time::Instant::now();
        let slow = engine.query("SELECT count(*) FROM range(1000000000000) AS a", &[], 1);
        assert!(matches!(slow, Err(EngineError::TimedOut)), "{slow:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "cancelled promptly"
        );
        // Ordinary queries still answer, and failures that are not timeouts say so.
        assert_eq!(engine.query("SELECT 41 + 1", &[], 1).unwrap(), vec![vec![json!(42)]]);
        assert!(matches!(
            engine.query("SELECT * FROM missing", &[], 1),
            Err(EngineError::Database(_))
        ));
    }

    #[test]
    fn the_time_limit_holds_even_when_it_passes_before_the_query_starts() {
        let conn = Connection::open_in_memory().unwrap();
        // The limit has already passed by the time the statement begins, as
        // happens on a busy machine. It must still be cancelled.
        let started = std::time::Instant::now();
        let slow = limited(&conn, std::time::Duration::ZERO, || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            Ok(
                conn.query_row("SELECT count(*) FROM range(1000000000000) AS a", [], |row| {
                    row.get::<_, i64>(0)
                })?,
            )
        });
        assert!(matches!(slow, Err(EngineError::TimedOut)), "{slow:?}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "cancelled promptly"
        );

        // A mistake in the query is reported as that mistake, however long it took.
        let wrong = limited(&conn, std::time::Duration::ZERO, || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            Ok(conn.query_row("SELECT * FROM missing", [], |row| row.get::<_, i64>(0))?)
        });
        assert!(matches!(wrong, Err(EngineError::Database(_))), "{wrong:?}");

        // Work that finishes is kept, and the connection is usable afterwards.
        let fine = limited(&conn, std::time::Duration::from_secs(30), || {
            Ok(conn.query_row("SELECT 41 + 1", [], |row| row.get::<_, i64>(0))?)
        });
        assert_eq!(fine.unwrap(), 42);
    }

    #[test]
    fn views_export_as_real_parquet_and_leave_no_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        let engine = Engine::open_with_uploads(&[spec("sales", &sample(dir.path()), None)], Some(&uploads)).unwrap();
        let bytes = engine
            .query_to_parquet(
                "SELECT \"Region\", amount FROM sales WHERE amount > ? ORDER BY _row",
                &[Param::Int(4)],
            )
            .unwrap();
        assert!(bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1"));
        let exports = fs::read_dir(&uploads)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().starts_with("export-"))
            .count();
        assert_eq!(exports, 0, "the temporary file is removed");
        // Read it back with a separate DuckDB to prove it is what was asked for.
        let copy = dir.path().join("check.parquet");
        fs::write(&copy, &bytes).unwrap();
        let reader = Connection::open_in_memory().unwrap();
        let rows: Vec<(Option<String>, i64)> = reader
            .prepare(&format!(
                "SELECT \"Region\", amount FROM read_parquet({})",
                quote_literal(copy.to_str().unwrap())
            ))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows, vec![(Some("US".into()), 5), (None, 7)]);
        assert!(matches!(
            Engine::open(&[]).unwrap().query_to_parquet("SELECT 1", &[]),
            Err(EngineError::UploadsDisabled)
        ));
    }

    #[test]
    fn reading_a_large_upload_does_not_hold_up_other_queries() {
        let dir = tempfile::tempdir().unwrap();
        let uploads = dir.path().join("uploads");
        let engine = std::sync::Arc::new(Engine::open_with_uploads(&[], Some(&uploads)).unwrap());
        let mut csv = String::from("a,b,c\n");
        for n in 0..400_000 {
            csv.push_str(&format!("{n},x{n},{}\n", n % 7));
        }
        std::fs::write(uploads.join("big.csv"), csv).unwrap();
        let started = std::time::Instant::now();
        engine.add_upload("first", "big.csv", Format::Csv).unwrap();
        let alone = started.elapsed();

        // A small query asked while another upload, then a preview, reads
        // the same file must not wait for that read to finish.
        type Job = Box<dyn FnOnce(&Engine) + Send>;
        let jobs: [Job; 2] = [
            Box::new(|e| drop(e.add_upload("second", "big.csv", Format::Csv).unwrap())),
            Box::new(|e| drop(e.preview("big.csv", Format::Csv, 5).unwrap())),
        ];
        for job in jobs {
            let worker = engine.clone();
            let reading = std::thread::spawn(move || job(&worker));
            std::thread::sleep(alone / 4);
            let asked = std::time::Instant::now();
            engine.query("SELECT 1", &[], 1).unwrap();
            let waited = asked.elapsed();
            reading.join().unwrap();
            assert!(
                waited < alone / 8,
                "a small query waited {waited:?} behind a {alone:?} file read"
            );
        }
    }

    #[test]
    fn file_datasets_are_kept_as_column_store_copies_with_identical_answers() {
        let dir = tempfile::tempdir().unwrap();
        let data = sample(dir.path());
        let uploads = dir.path().join("uploads");
        let plain = Engine::open(&[spec("sales", &data, None)]).unwrap();
        let cached = Engine::open_with_uploads(&[spec("sales", &data, None)], Some(&uploads)).unwrap();
        let sql = "SELECT * FROM sales ORDER BY _row";
        let width = plain.dataset("sales").unwrap().columns.len();
        assert_eq!(
            plain.query(sql, &[], width).unwrap(),
            cached.query(sql, &[], width).unwrap()
        );
        assert_eq!(
            plain.dataset("sales"),
            cached.dataset("sales"),
            "same columns and types"
        );
        assert_eq!(cached.stats().cached_datasets, 1);
        let view: String = cached
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT sql FROM duckdb_views() WHERE view_name = 'sales'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(view.contains("read_parquet"), "the view reads the copy: {view}");

        // A changed file gets a fresh copy; the stale one is pruned.
        drop(cached);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut text = fs::read_to_string(&data).unwrap();
        text.push_str("4,EU,7,1.00,2024-04-01,d,true\n");
        fs::write(&data, text).unwrap();
        let again = Engine::open_with_uploads(&[spec("sales", &data, None)], Some(&uploads)).unwrap();
        assert_eq!(
            again.query("SELECT count(*) FROM sales", &[], 1).unwrap(),
            vec![vec![json!(4)]]
        );
        assert_eq!(fs::read_dir(uploads.join(".cache")).unwrap().count(), 2, "old and new");
        assert_eq!(again.prune_cache(), 1);
        assert_eq!(fs::read_dir(uploads.join(".cache")).unwrap().count(), 1);
        drop(again);

        // Rewritten at the same size with its time set back, as copying
        // tools and coarse filesystems can leave it: still a fresh copy.
        let when = fs::metadata(&data).unwrap().modified().unwrap();
        let text = fs::read_to_string(&data).unwrap().replace("4,EU,7,", "4,EU,9,");
        fs::write(&data, &text).unwrap();
        fs::File::options()
            .write(true)
            .open(&data)
            .unwrap()
            .set_modified(when)
            .unwrap();
        assert_eq!(fs::metadata(&data).unwrap().modified().unwrap(), when);
        let third = Engine::open_with_uploads(&[spec("sales", &data, None)], Some(&uploads)).unwrap();
        assert_eq!(
            third.query("SELECT max(amount) FROM sales", &[], 1).unwrap(),
            vec![vec![json!(9)]]
        );
    }

    #[test]
    fn memory_is_capped_and_a_query_past_the_cap_says_so() {
        let limits = Limits {
            memory_limit_bytes: Some(64 * 1024 * 1024),
            ..Limits::default()
        };
        // No upload folder: nowhere to spill, so a query past the cap fails.
        let engine = Engine::open_with_options(&[], None, &limits).unwrap();
        let stats = engine.stats();
        assert!(
            (60 * 1024 * 1024..=68 * 1024 * 1024).contains(&stats.memory_limit_bytes),
            "{}",
            stats.memory_limit_bytes
        );
        let greedy = engine.query(
            "SELECT count(DISTINCT s) FROM (SELECT repeat('x', 200) || i AS s FROM range(3000000) t(i))",
            &[],
            1,
        );
        assert!(matches!(greedy, Err(EngineError::OutOfMemory)), "{greedy:?}");
        assert_eq!(engine.stats().queries_out_of_memory, 1);
        // The engine is fine afterwards.
        assert_eq!(engine.query("SELECT 1", &[], 1).unwrap(), vec![vec![json!(1)]]);
    }

    #[test]
    fn queries_wait_their_turn_and_are_turned_away_when_none_comes() {
        let limits = Limits {
            max_concurrent: 1,
            ..Limits::default()
        };
        let engine = Engine::open_with_options(&[], None, &limits).unwrap();
        engine.set_query_timeout(std::time::Duration::from_millis(100));
        let held = engine.gate.enter(std::time::Duration::ZERO).unwrap();
        assert!(matches!(engine.query("SELECT 1", &[], 1), Err(EngineError::Busy)));
        drop(held);
        assert!(engine.query("SELECT 1", &[], 1).is_ok(), "a freed place is used");
        let stats = engine.stats();
        assert_eq!(
            (stats.queries_busy, stats.queries_total, stats.queries_running),
            (1, 2, 0)
        );
    }

    #[test]
    fn slow_streamed_readers_cannot_hold_the_places_other_queries_need() {
        let engine = Engine::open_with_options(&[], None, &Limits::default()).unwrap();
        engine.set_query_timeout(std::time::Duration::from_millis(100));
        // Every streaming place is taken, as by clients that stopped reading.
        let held: Vec<_> = (0..STREAMS_AT_ONCE)
            .map(|_| engine.stream_gate.enter(std::time::Duration::ZERO).unwrap())
            .collect();
        assert!(matches!(
            engine.stream_text("SELECT 1", &[], 1, |_| true),
            Err(EngineError::Busy)
        ));
        // Everyone else is unaffected.
        assert_eq!(engine.query("SELECT 1", &[], 1).unwrap(), vec![vec![json!(1)]]);
        drop(held);
        assert_eq!(engine.stream_text("SELECT 1", &[], 1, |_| true).unwrap(), 1);
    }
}
