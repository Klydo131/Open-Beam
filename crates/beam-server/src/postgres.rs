//! Read-only access over the PostgreSQL wire protocol, for tools that speak
//! it (psql, DBeaver, Python, R, many BI tools). Off unless configured.
//!
//! What a connection can do is deliberately small:
//!
//! - Sign in with an Open Beam username and one of that account's personal
//!   API tokens as the password. Account passwords are never accepted here,
//!   because this protocol sends the password as plain text.
//! - Run one `SELECT` at a time over the datasets that account can see, and
//!   a few catalog tables describing only those datasets. Every statement is
//!   parsed by the engine and checked before it runs; see [`crate::pgsql`].
//! - Everything else that a client sends to set itself up (`SET`, `BEGIN`,
//!   `SHOW`, ...) is answered without touching any data.

use crate::app::AppState;
use crate::pgsql::{self, Checked, Column};
use beam_accounts::Account;
use beam_engine::Param;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};

/// Connections served at once; more are refused straight away.
const MAX_CONNECTIONS: usize = 64;
/// Longest any single message from a client may be.
const MAX_MESSAGE: usize = 1024 * 1024;
/// Time allowed to send the start-up messages and sign in.
const SIGN_IN_TIME: Duration = Duration::from_secs(15);
/// A connection that sends nothing for this long is closed.
const IDLE_TIME: Duration = Duration::from_secs(30 * 60);
/// A client that stops reading for this long is disconnected.
const WRITE_TIME: Duration = Duration::from_secs(60);
/// Prepared statements and portals one connection may keep.
const MAX_PREPARED: usize = 256;
/// Rows passed from the engine to the connection in one go.
const BATCH_ROWS: usize = 512;

const PROTOCOL_3: i32 = 196_608;
const SSL_REQUEST: i32 = 80_877_103;
const GSS_REQUEST: i32 = 80_877_104;
const CANCEL_REQUEST: i32 = 80_877_102;

/// Accepts connections until the process ends.
pub async fn serve(listener: TcpListener, state: Arc<AppState>) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            let mut stream = stream;
            let _ = stream
                .write_all(&error_message("53300", "too many connections to Open Beam"))
                .await;
            continue;
        };
        let state = state.clone();
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            let _ = Connection::new(stream, state).run().await;
            drop(slot);
        });
    }
}

/// Why a statement could not be answered, as PostgreSQL error fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgError {
    pub code: &'static str,
    pub message: String,
}

impl PgError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// A statement ready to run, kept by name for the extended protocol.
#[derive(Clone)]
enum Prepared {
    Empty,
    /// Accepted and answered with this command tag; nothing runs.
    Ignored(&'static str),
    /// Answered from a fixed list of settings.
    Show {
        name: String,
        value: String,
    },
    Select(Checked),
}

impl Prepared {
    fn columns(&self) -> Vec<Column> {
        match self {
            Prepared::Show { name, .. } => vec![Column::text(name)],
            Prepared::Select(checked) => checked.columns.clone(),
            _ => Vec::new(),
        }
    }

    fn parameters(&self) -> usize {
        match self {
            Prepared::Select(checked) => checked.parameters,
            _ => 0,
        }
    }
}

#[derive(Clone)]
struct Portal {
    statement: Prepared,
    params: Vec<Param>,
    /// Per-column result formats: 0 text, 1 binary.
    formats: Vec<i16>,
}

struct Connection {
    reader: BufReader<tokio::io::ReadHalf<TcpStream>>,
    writer: BufWriter<tokio::io::WriteHalf<TcpStream>>,
    state: Arc<AppState>,
    account: Option<Account>,
    /// Prepared statements with the parameter types the client declared.
    statements: std::collections::HashMap<String, (Prepared, Vec<i32>)>,
    portals: std::collections::HashMap<String, Portal>,
}

/// Reads a whole message body after its length, refusing oversized ones.
async fn read_body(reader: &mut (impl AsyncReadExt + Unpin), length: i32) -> std::io::Result<Vec<u8>> {
    let length = usize::try_from(length)
        .ok()
        .filter(|n| (4..=MAX_MESSAGE).contains(n))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad message length"))?;
    let mut body = vec![0; length - 4];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

impl Connection {
    fn new(stream: TcpStream, state: Arc<AppState>) -> Self {
        let (read, write) = tokio::io::split(stream);
        Self {
            reader: BufReader::new(read),
            writer: BufWriter::new(write),
            state,
            account: None,
            statements: Default::default(),
            portals: Default::default(),
        }
    }

    async fn run(mut self) -> std::io::Result<()> {
        match tokio::time::timeout(SIGN_IN_TIME, self.sign_in()).await {
            Ok(Ok(true)) => {}
            _ => return Ok(()),
        }
        self.serve_messages().await
    }

    async fn send(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        match tokio::time::timeout(WRITE_TIME, self.writer.write_all(bytes)).await {
            Ok(result) => result,
            Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match tokio::time::timeout(WRITE_TIME, self.writer.flush()).await {
            Ok(result) => result,
            Err(_) => Err(std::io::ErrorKind::TimedOut.into()),
        }
    }

    /// Start-up and sign-in. `Ok(false)` means the connection should close.
    async fn sign_in(&mut self) -> std::io::Result<bool> {
        let parameters = loop {
            let length = self.reader.read_i32().await?;
            let body = read_body(&mut self.reader, length).await?;
            let code = body.get(..4).map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]));
            match code {
                // Encryption is not offered: say no and let the client decide.
                Some(SSL_REQUEST | GSS_REQUEST) => {
                    self.send(b"N").await?;
                    self.flush().await?;
                }
                Some(PROTOCOL_3) => break startup_parameters(&body[4..]),
                Some(CANCEL_REQUEST) => return Ok(false),
                _ => {
                    self.send(&error_message("0A000", "unsupported protocol version"))
                        .await?;
                    self.flush().await?;
                    return Ok(false);
                }
            }
        };
        let user = parameters
            .iter()
            .find(|(k, _)| k == "user")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();

        // Ask for the password: an API token, sent as plain text.
        self.send(&message(b'R', &3i32.to_be_bytes())).await?;
        self.flush().await?;
        let tag = self.reader.read_u8().await?;
        let length = self.reader.read_i32().await?;
        let body = read_body(&mut self.reader, length).await?;
        if tag != b'p' {
            return Ok(false);
        }
        let secret = String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(&body)).into_owned();
        let state = self.state.clone();
        let account = tokio::task::spawn_blocking(move || state.accounts.token_account(&secret).ok().flatten())
            .await
            .ok()
            .flatten()
            .filter(|account| account.username.eq_ignore_ascii_case(user.trim()));
        let Some(account) = account else {
            self.send(&error_message(
                "28P01",
                "password authentication failed: use your Open Beam username and a personal API token from Settings",
            ))
            .await?;
            self.flush().await?;
            return Ok(false);
        };

        let mut out = message(b'R', &0i32.to_be_bytes());
        for (name, value) in [
            ("server_version", pgsql::SERVER_VERSION),
            ("server_encoding", "UTF8"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("IntervalStyle", "postgres"),
            ("TimeZone", "UTC"),
            ("integer_datetimes", "on"),
            ("standard_conforming_strings", "on"),
            ("is_superuser", "off"),
            ("default_transaction_read_only", "on"),
            ("in_hot_standby", "off"),
        ] {
            out.extend(message(b'S', &[cstr(name), cstr(value)].concat()));
        }
        out.extend(message(
            b'S',
            &[cstr("session_authorization"), cstr(&account.username)].concat(),
        ));
        // Cancelling is not supported, so the key only has to look valid.
        let mut key = [0u8; 8];
        let _ = getrandom::fill(&mut key);
        out.extend(message(b'K', &key));
        out.extend(ready());
        self.send(&out).await?;
        self.flush().await?;
        self.account = Some(account);
        Ok(true)
    }

    async fn serve_messages(&mut self) -> std::io::Result<()> {
        // After an error in the extended protocol, messages are skipped
        // until the client's next Sync, as PostgreSQL does.
        let mut skipping = false;
        loop {
            let tag = match tokio::time::timeout(IDLE_TIME, self.reader.read_u8()).await {
                Ok(Ok(tag)) => tag,
                _ => return Ok(()),
            };
            let length = self.reader.read_i32().await?;
            let body = read_body(&mut self.reader, length).await?;
            if skipping && tag != b'S' {
                continue;
            }
            let outcome = match tag {
                b'Q' => {
                    self.simple_query(&body).await?;
                    Ok(())
                }
                b'P' => self.parse(&body).await,
                b'B' => self.bind(&body).await,
                b'D' => self.describe(&body).await,
                b'E' => self.execute(&body).await,
                b'C' => self.close(&body).await,
                b'H' => {
                    self.flush().await?;
                    Ok(())
                }
                b'S' => {
                    skipping = false;
                    self.send(&ready()).await?;
                    self.flush().await?;
                    Ok(())
                }
                b'X' => return Ok(()),
                _ => Err(PgError::new("08P01", "unsupported message")),
            };
            match outcome {
                Ok(()) => {}
                Err(error) => {
                    skipping = true;
                    self.send(&error_message(error.code, &error.message)).await?;
                }
            }
        }
    }

    fn account(&self) -> &Account {
        self.account.as_ref().expect("signed in")
    }

    async fn prepare(&self, sql: String) -> Result<Prepared, PgError> {
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return Ok(Prepared::Empty);
        }
        if let Some(tag) = pgsql::ignored_command(trimmed) {
            return Ok(Prepared::Ignored(tag));
        }
        if let Some(name) = pgsql::show_name(trimmed) {
            let value = pgsql::setting(&name, &self.account().username)
                .ok_or_else(|| PgError::new("42704", format!("unrecognized configuration parameter \"{name}\"")))?;
            return Ok(Prepared::Show { name, value });
        }
        let (state, account, sql) = (self.state.clone(), self.account().clone(), trimmed.to_string());
        tokio::task::spawn_blocking(move || pgsql::check(&state, &account, &sql))
            .await
            .map_err(|_| PgError::new("XX000", "internal error"))?
            .map(Prepared::Select)
    }

    async fn simple_query(&mut self, body: &[u8]) -> std::io::Result<()> {
        let sql = String::from_utf8_lossy(body.strip_suffix(&[0]).unwrap_or(body)).into_owned();
        match self.prepare(sql).await {
            Ok(statement) => {
                let columns = statement.columns();
                if !columns.is_empty() {
                    let description = row_description(&columns, &[]);
                    self.send(&description).await?;
                }
                let portal = Portal {
                    statement,
                    params: Vec::new(),
                    formats: Vec::new(),
                };
                if let Err(error) = self.run_portal(&portal).await? {
                    self.send(&error_message(error.code, &error.message)).await?;
                }
            }
            Err(error) => self.send(&error_message(error.code, &error.message)).await?,
        }
        self.send(&ready()).await?;
        self.flush().await
    }

    async fn parse(&mut self, body: &[u8]) -> Result<(), PgError> {
        let mut reader = Body::new(body);
        let name = reader.cstr()?;
        let sql = reader.cstr()?;
        let declared = reader.i16()?.max(0);
        let types: Vec<i32> = (0..declared).map(|_| reader.i32()).collect::<Result<_, _>>()?;
        if !self.statements.contains_key(&name) && self.statements.len() >= MAX_PREPARED {
            return Err(PgError::new("53400", "too many prepared statements"));
        }
        let statement = self.prepare(sql).await?;
        self.statements.insert(name, (statement, types));
        self.send(&message(b'1', &[])).await.map_err(io_error)
    }

    async fn bind(&mut self, body: &[u8]) -> Result<(), PgError> {
        let mut reader = Body::new(body);
        let portal = reader.cstr()?;
        let name = reader.cstr()?;
        let (statement, types) = self
            .statements
            .get(&name)
            .cloned()
            .ok_or_else(|| PgError::new("26000", format!("prepared statement \"{name}\" does not exist")))?;
        let format_count = reader.i16()?.max(0) as usize;
        let param_formats: Vec<i16> = (0..format_count).map(|_| reader.i16()).collect::<Result<_, _>>()?;
        let count = reader.i16()?.max(0) as usize;
        if count != statement.parameters() {
            return Err(PgError::new(
                "08P01",
                format!("expected {} parameters, got {count}", statement.parameters()),
            ));
        }
        let mut params = Vec::with_capacity(count);
        for index in 0..count {
            let format = match param_formats.len() {
                0 => 0,
                1 => param_formats[0],
                _ => *param_formats.get(index).unwrap_or(&0),
            };
            let length = reader.i32()?;
            if length < 0 {
                params.push(Param::Null);
                continue;
            }
            let bytes = reader.bytes(length as usize)?;
            let declared = types.get(index).copied().unwrap_or(0);
            params.push(pgsql::parameter(declared, format, bytes)?);
        }
        let result_count = reader.i16()?.max(0) as usize;
        let formats: Vec<i16> = (0..result_count).map(|_| reader.i16()).collect::<Result<_, _>>()?;
        if !self.portals.contains_key(&portal) && self.portals.len() >= MAX_PREPARED {
            return Err(PgError::new("53400", "too many open portals"));
        }
        self.portals.insert(
            portal,
            Portal {
                statement,
                params,
                formats,
            },
        );
        self.send(&message(b'2', &[])).await.map_err(io_error)
    }

    async fn describe(&mut self, body: &[u8]) -> Result<(), PgError> {
        let mut reader = Body::new(body);
        let kind = reader.u8()?;
        let name = reader.cstr()?;
        let mut out = Vec::new();
        let (columns, formats) = if kind == b'S' {
            let (statement, types) = self
                .statements
                .get(&name)
                .ok_or_else(|| PgError::new("26000", format!("prepared statement \"{name}\" does not exist")))?;
            // Parameters keep the type the client declared; the rest are text.
            let count = statement.parameters();
            let mut description = (count as i16).to_be_bytes().to_vec();
            for index in 0..count {
                let declared = types.get(index).copied().filter(|t| *t != 0).unwrap_or(pgsql::TEXT_OID);
                description.extend(declared.to_be_bytes());
            }
            out.extend(message(b't', &description));
            (statement.columns(), Vec::new())
        } else {
            let portal = self
                .portals
                .get(&name)
                .ok_or_else(|| PgError::new("34000", format!("portal \"{name}\" does not exist")))?;
            (portal.statement.columns(), portal.formats.clone())
        };
        if columns.is_empty() {
            out.extend(message(b'n', &[]));
        } else {
            out.extend(row_description(&columns, &formats));
        }
        self.send(&out).await.map_err(io_error)
    }

    async fn execute(&mut self, body: &[u8]) -> Result<(), PgError> {
        let mut reader = Body::new(body);
        let name = reader.cstr()?;
        let portal = self
            .portals
            .get(&name)
            .cloned()
            .ok_or_else(|| PgError::new("34000", format!("portal \"{name}\" does not exist")))?;
        match self.run_portal(&portal).await.map_err(io_error)? {
            Ok(()) => Ok(()),
            Err(error) => Err(error),
        }
    }

    async fn close(&mut self, body: &[u8]) -> Result<(), PgError> {
        let mut reader = Body::new(body);
        let kind = reader.u8()?;
        let name = reader.cstr()?;
        if kind == b'S' {
            self.statements.remove(&name);
        } else {
            self.portals.remove(&name);
        }
        self.send(&message(b'3', &[])).await.map_err(io_error)
    }

    /// Sends a portal's rows and its completion. The outer `Result` is the
    /// connection failing; the inner one is the statement failing.
    async fn run_portal(&mut self, portal: &Portal) -> std::io::Result<Result<(), PgError>> {
        match &portal.statement {
            Prepared::Empty => {
                self.send(&message(b'I', &[])).await?;
            }
            Prepared::Ignored(tag) => {
                self.send(&command_complete(tag)).await?;
            }
            Prepared::Show { value, .. } => {
                let column = Column::text("value");
                let row = data_row(&[Some(value.clone())], &[column], &portal.formats);
                match row {
                    Ok(row) => self.send(&row).await?,
                    Err(error) => return Ok(Err(error)),
                }
                self.send(&command_complete("SHOW")).await?;
            }
            Prepared::Select(checked) => {
                let (sender, mut receiver) = tokio::sync::mpsc::channel::<Vec<Vec<Option<String>>>>(4);
                let (state, sql, params, width) = (
                    self.state.clone(),
                    checked.sql.clone(),
                    portal.params.clone(),
                    checked.columns.len(),
                );
                let worker = tokio::task::spawn_blocking(move || {
                    let mut batch = Vec::with_capacity(BATCH_ROWS);
                    let result = state.engine.stream_text(&sql, &params, width, |row| {
                        batch.push(row);
                        if batch.len() == BATCH_ROWS {
                            let full = std::mem::replace(&mut batch, Vec::with_capacity(BATCH_ROWS));
                            return sender.blocking_send(full).is_ok();
                        }
                        true
                    });
                    if !batch.is_empty() {
                        let _ = sender.blocking_send(batch);
                    }
                    result
                });
                let mut sent: u64 = 0;
                while let Some(batch) = receiver.recv().await {
                    let mut out = Vec::new();
                    for row in &batch {
                        match data_row(row, &checked.columns, &portal.formats) {
                            Ok(row) => out.extend(row),
                            Err(error) => {
                                drop(receiver);
                                let _ = worker.await;
                                return Ok(Err(error));
                            }
                        }
                    }
                    sent += batch.len() as u64;
                    self.send(&out).await?;
                }
                match worker.await {
                    Ok(Ok(_)) => {}
                    Ok(Err(beam_engine::EngineError::TimedOut)) => {
                        return Ok(Err(PgError::new("57014", crate::feed::TOO_LONG)));
                    }
                    Ok(Err(e)) => return Ok(Err(pgsql::engine_error(e))),
                    Err(_) => return Ok(Err(PgError::new("XX000", "internal error"))),
                }
                self.send(&command_complete(&format!("SELECT {sent}"))).await?;
            }
        }
        Ok(Ok(()))
    }
}

fn io_error(_: std::io::Error) -> PgError {
    PgError::new("08006", "connection failure")
}

/// Reads the fields of a client message.
struct Body<'a> {
    bytes: &'a [u8],
}

impl<'a> Body<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn malformed() -> PgError {
        PgError::new("08P01", "malformed message")
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], PgError> {
        if self.bytes.len() < n {
            return Err(Self::malformed());
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, PgError> {
        Ok(self.bytes(1)?[0])
    }

    fn i16(&mut self) -> Result<i16, PgError> {
        let b = self.bytes(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn i32(&mut self) -> Result<i32, PgError> {
        let b = self.bytes(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn cstr(&mut self) -> Result<String, PgError> {
        let end = self.bytes.iter().position(|b| *b == 0).ok_or_else(Self::malformed)?;
        let text = String::from_utf8(self.bytes[..end].to_vec()).map_err(|_| Self::malformed())?;
        self.bytes = &self.bytes[end + 1..];
        Ok(text)
    }
}

fn startup_parameters(mut body: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut next = || {
        let end = body.iter().position(|b| *b == 0)?;
        let text = String::from_utf8_lossy(&body[..end]).into_owned();
        body = &body[end + 1..];
        Some(text)
    };
    while let Some(key) = next() {
        if key.is_empty() {
            break;
        }
        let Some(value) = next() else { break };
        out.push((key, value));
        if out.len() > 64 {
            break;
        }
    }
    out
}

fn cstr(text: &str) -> Vec<u8> {
    let mut out: Vec<u8> = text.bytes().filter(|b| *b != 0).collect();
    out.push(0);
    out
}

fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(tag);
    out.extend(((body.len() + 4) as i32).to_be_bytes());
    out.extend(body);
    out
}

fn ready() -> Vec<u8> {
    // Always idle: nothing here is ever inside a transaction that matters.
    message(b'Z', b"I")
}

fn command_complete(tag: &str) -> Vec<u8> {
    message(b'C', &cstr(tag))
}

pub fn error_message(code: &str, text: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (field, value) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', code), (b'M', text)] {
        body.push(field);
        body.extend(cstr(value));
    }
    body.push(0);
    message(b'E', &body)
}

fn format_of(formats: &[i16], index: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => *formats.get(index).unwrap_or(&0),
    }
}

fn row_description(columns: &[Column], formats: &[i16]) -> Vec<u8> {
    let mut body = (columns.len() as i16).to_be_bytes().to_vec();
    for (index, column) in columns.iter().enumerate() {
        body.extend(cstr(&column.name));
        body.extend(0i32.to_be_bytes()); // table oid
        body.extend(0i16.to_be_bytes()); // column number
        body.extend(column.oid.to_be_bytes());
        body.extend(column.size.to_be_bytes());
        body.extend((-1i32).to_be_bytes()); // type modifier
        body.extend(format_of(formats, index).to_be_bytes());
    }
    message(b'T', &body)
}

fn data_row(values: &[Option<String>], columns: &[Column], formats: &[i16]) -> Result<Vec<u8>, PgError> {
    let mut body = (values.len() as i16).to_be_bytes().to_vec();
    for (index, (value, column)) in values.iter().zip(columns).enumerate() {
        match value {
            None => body.extend((-1i32).to_be_bytes()),
            Some(text) => {
                let encoded = if format_of(formats, index) == 1 {
                    pgsql::binary(column, text)?
                } else {
                    pgsql::text(column, text).into_bytes()
                };
                body.extend((encoded.len() as i32).to_be_bytes());
                body.extend(encoded);
            }
        }
    }
    Ok(message(b'D', &body))
}
