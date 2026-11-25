//! Accounts, roles, sessions, contacts and dataset ownership for Open Beam.
//!
//! Everything is kept in one SQLite file that belongs to whoever runs Open
//! Beam. Passwords are stored as Argon2id hashes. Session tokens are random,
//! handed to the browser once, and stored only as SHA-256 digests, so a copy
//! of the file does not let anyone sign in.

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use rusqlite::{params, Connection, OptionalExtension, Row};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Mutex;

/// NIST SP 800-63B-4 (2025): a password that is the only factor at sign-in
/// is at least 15 characters. Existing, shorter passwords keep working until
/// they are next changed.
pub const MIN_PASSWORD_CHARS: usize = 15;
/// Upper bound so that hashing cannot be turned into a way to waste CPU.
pub const MAX_PASSWORD_BYTES: usize = 256;
const SESSION_HOURS: u32 = 12;

/// The watches table. Kept in one place because adding a kind means
/// rebuilding older tables with it.
const WATCHES_COLUMNS: &str = "(
    id           INTEGER PRIMARY KEY,
    dataset      TEXT NOT NULL COLLATE NOCASE,
    name         TEXT NOT NULL,
    kind         TEXT NOT NULL CHECK (kind IN ('total', 'change', 'repeats', 'steady')),
    settings     TEXT NOT NULL,
    threshold    REAL NOT NULL,
    created_by   INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    created_at   TEXT NOT NULL DEFAULT (datetime('now')),
    last_run     TEXT,
    last_status  TEXT CHECK (last_status IN ('ok', 'alert', 'error')),
    last_message TEXT,
    last_value   REAL
)";

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("Usernames are 3 to 32 characters: lowercase letters, digits, dot, dash or underscore.")]
    InvalidUsername,
    #[error("Passwords must be between {MIN_PASSWORD_CHARS} characters and {MAX_PASSWORD_BYTES} bytes long.")]
    InvalidPassword,
    #[error("That password is too easy to guess: it is a common password, a keyboard or number run, or mostly one character. Try a few unrelated words.")]
    WeakPassword,
    #[error("That username is already in use.")]
    UsernameTaken,
    #[error("There must always be at least one active administrator.")]
    LastAdministrator,
    #[error("An administrator account already exists.")]
    AlreadySetUp,
    #[error("No such account.")]
    NotFound,
    #[error("Names and teams are at most {MAX_PROFILE_CHARS} characters, without control characters.")]
    InvalidProfile,
    #[error("That dataset name is already in use.")]
    NameTaken,
    #[error("No such dataset.")]
    UploadNotFound,
    #[error("You already have the most tokens allowed. Revoke one you no longer use.")]
    TooManyTokens,
    #[error("Descriptions are at most {MAX_NOTE_CHARS} characters and questions and names at most {MAX_TITLE_CHARS}, without control characters other than line breaks.")]
    InvalidText,
    #[error("This dataset already has the most {0} allowed. Remove one you no longer need.")]
    TooMany(&'static str),
    #[error("No such question or watch.")]
    ItemNotFound,
    #[error("{0}")]
    BadBackup(&'static str),
    #[error("account database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("password hashing failed")]
    Hashing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Manages accounts, and can do everything a user can.
    Admin,
    /// Reads datasets in the web interface and through the data feed.
    User,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::User => "user",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "admin" => Some(Role::Admin),
            "user" => Some(Role::User),
            _ => None,
        }
    }
}

/// How much of the interface a person sees. A preference, not a permission:
/// it changes which tools are shown, never what an account may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Basic,
    Advanced,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Basic => "basic",
            Mode::Advanced => "advanced",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "basic" => Some(Mode::Basic),
            "advanced" => Some(Mode::Advanced),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub active: bool,
    /// UTC, `YYYY-MM-DD HH:MM:SS`.
    pub created_at: String,
    pub last_sign_in: Option<String>,
    /// How colleagues see this person, for example "Maria Santos".
    pub display_name: Option<String>,
    /// Team, department or job title, for example "Finance".
    pub team: Option<String>,
    pub mode: Mode,
}

impl Account {
    /// The name to show people: the display name when set, else the username.
    pub fn name(&self) -> &str {
        self.display_name.as_deref().unwrap_or(&self.username)
    }
}

/// Clean-up applied when a dataset is read. The stored file is never
/// changed, so what was removed can always be seen again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Cleaning {
    /// Leave out rows in which every cell is empty.
    pub drop_empty_rows: bool,
    /// Keep one copy of rows that are identical in every column.
    pub drop_duplicate_rows: bool,
}

/// A dataset a person added through the web interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    pub id: i64,
    /// The dataset name, unique across all datasets.
    pub name: String,
    pub owner_id: i64,
    /// File name inside the upload directory. Chosen by the server, never by
    /// the person uploading.
    pub file: String,
    pub created_at: String,
    pub cleaning: Cleaning,
}

/// A file someone added but has not yet reviewed and saved. Drafts are not
/// datasets: nobody else can see them, and nothing can read them but the
/// review page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: i64,
    pub owner_id: i64,
    /// File name inside the upload directory, chosen by the server.
    pub file: String,
    /// The name of the file on the person's computer, for display only.
    pub original_name: String,
    pub created_at: String,
}

/// A personal API token as listed in Settings. The secret itself is shown
/// once, when the token is created, and stored only as a digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiToken {
    pub id: i64,
    pub account_id: i64,
    /// What the person called it, for example "Power BI on my laptop".
    pub name: String,
    pub created_at: String,
    pub last_used: Option<String>,
}

/// Every token starts with this, so secret scanners and people can
/// recognise one that was pasted somewhere it should not be.
pub const TOKEN_PREFIX: &str = "obt_";
/// Most tokens one account may hold.
pub const MAX_TOKENS: i64 = 20;

const UPLOAD_COLUMNS: &str = "id, name, owner_id, file, created_at, drop_empty_rows, drop_duplicate_rows";
const DRAFT_COLUMNS: &str = "id, owner_id, file, original_name, created_at";

pub const MAX_PROFILE_CHARS: usize = 64;
/// Longest description of a dataset or a column.
pub const MAX_NOTE_CHARS: usize = 2000;
/// Longest verified question or watch name.
pub const MAX_TITLE_CHARS: usize = 200;
/// Most verified questions, and most watches, on one dataset.
pub const MAX_PER_DATASET: i64 = 50;
/// Most watches across all datasets, which bounds the hourly checks.
pub const MAX_WATCHES: i64 = 500;
/// Longest stored settings of a question or watch, in bytes.
pub const MAX_SETTINGS_BYTES: usize = 4096;

/// A signed-in browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub account: Account,
    /// Echoed in every form so that other sites cannot submit on the user's behalf.
    pub csrf: String,
}

/// Returned once when a session starts; `token` goes in the cookie.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    pub token: String,
    pub csrf: String,
}

pub struct Store {
    conn: Mutex<Connection>,
    /// Verified against when a username is unknown, so that a wrong username
    /// takes as long to reject as a wrong password.
    decoy_hash: String,
}

const ACCOUNT_COLUMNS: &str = "id, username, role, active, created_at, last_sign_in, display_name, team, mode";
/// Number of columns in `ACCOUNT_COLUMNS`; extra columns in a query follow it.
const ACCOUNT_WIDTH: usize = 9;

impl Store {
    /// Opens the account database, creating it if needed.
    pub fn open(path: &Path) -> Result<Self, AccountError> {
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Best effort: the file holds password hashes, so keep it private.
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        // Wait for another process (such as `beam create-admin` while the
        // server runs) instead of failing at once, and make every committed
        // change reach the disk before it is reported as saved.
        conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA synchronous = FULL;")?;
        Self::prepare(conn)
    }

    pub fn open_in_memory() -> Result<Self, AccountError> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Self, AccountError> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS accounts (
                 id            INTEGER PRIMARY KEY,
                 username      TEXT NOT NULL UNIQUE,
                 password_hash TEXT NOT NULL,
                 role          TEXT NOT NULL CHECK (role IN ('admin', 'user')),
                 active        INTEGER NOT NULL DEFAULT 1,
                 created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                 last_sign_in  TEXT
             );
             CREATE TABLE IF NOT EXISTS sessions (
                 token_hash TEXT PRIMARY KEY,
                 account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 csrf       TEXT NOT NULL,
                 expires_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS contacts (
                 owner_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 contact_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 PRIMARY KEY (owner_id, contact_id),
                 CHECK (owner_id <> contact_id)
             );
             CREATE TABLE IF NOT EXISTS uploads (
                 id         INTEGER PRIMARY KEY,
                 name       TEXT NOT NULL UNIQUE COLLATE NOCASE,
                 owner_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 file       TEXT NOT NULL UNIQUE,
                 created_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS api_tokens (
                 id         INTEGER PRIMARY KEY,
                 account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 name       TEXT NOT NULL,
                 token_hash TEXT NOT NULL UNIQUE,
                 created_at TEXT NOT NULL DEFAULT (datetime('now')),
                 last_used  TEXT
             );
             CREATE TABLE IF NOT EXISTS drafts (
                 id            INTEGER PRIMARY KEY,
                 owner_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 file          TEXT NOT NULL UNIQUE,
                 original_name TEXT NOT NULL,
                 created_at    TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS notes (
                 dataset     TEXT NOT NULL COLLATE NOCASE,
                 column_name TEXT NOT NULL DEFAULT '',
                 text        TEXT NOT NULL,
                 updated_by  INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
                 updated_at  TEXT NOT NULL DEFAULT (datetime('now')),
                 PRIMARY KEY (dataset, column_name)
             );
             CREATE TABLE IF NOT EXISTS questions (
                 id          INTEGER PRIMARY KEY,
                 dataset     TEXT NOT NULL COLLATE NOCASE,
                 question    TEXT NOT NULL,
                 settings    TEXT NOT NULL,
                 approved_by INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
                 approved_at TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS shares (
                 upload_id  INTEGER NOT NULL REFERENCES uploads(id) ON DELETE CASCADE,
                 account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
                 PRIMARY KEY (upload_id, account_id)
             );
             DELETE FROM sessions WHERE expires_at <= datetime('now');",
        )?;
        conn.execute_batch(&format!("CREATE TABLE IF NOT EXISTS watches {WATCHES_COLUMNS};"))?;
        // Watch kinds added later widen the table's check. SQLite cannot
        // change a check in place, so an older table is copied into a new
        // one in a single transaction: all of it moves, or none.
        let watches_sql: String = conn.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'watches'",
            [],
            |row| row.get(0),
        )?;
        if !watches_sql.contains("'steady'") {
            conn.execute_batch(&format!(
                "BEGIN IMMEDIATE;
                 CREATE TABLE watches_widened {WATCHES_COLUMNS};
                 INSERT INTO watches_widened (id, dataset, name, kind, settings, threshold, created_by,
                     created_at, last_run, last_status, last_message, last_value)
                   SELECT id, dataset, name, kind, settings, threshold, created_by,
                     created_at, last_run, last_status, last_message, last_value FROM watches;
                 DROP TABLE watches;
                 ALTER TABLE watches_widened RENAME TO watches;
                 COMMIT;"
            ))?;
        }
        // Columns added after 0.2. Adding is the only kind of change made to
        // an existing file, so older account files open unchanged.
        for (table, column, definition) in [
            ("accounts", "display_name", "TEXT"),
            ("accounts", "team", "TEXT"),
            (
                "accounts",
                "mode",
                "TEXT NOT NULL DEFAULT 'basic' CHECK (mode IN ('basic', 'advanced'))",
            ),
            ("uploads", "drop_empty_rows", "INTEGER NOT NULL DEFAULT 0"),
            ("uploads", "drop_duplicate_rows", "INTEGER NOT NULL DEFAULT 0"),
        ] {
            let existing: Vec<String> = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            if !existing.iter().any(|name| name == column) {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
            }
        }
        let decoy_hash = hash_password(&random_token()?)?;
        Ok(Self {
            conn: Mutex::new(conn),
            decoy_hash,
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many descriptions, verified questions and watches are kept for
    /// `dataset` (matched by name, as everywhere).
    pub fn curation_count(&self, dataset: &str) -> Result<[u64; 3], AccountError> {
        let conn = self.conn();
        let mut counts = [0; 3];
        for (slot, table) in counts.iter_mut().zip(["notes", "questions", "watches"]) {
            let n: i64 = conn.query_row(
                &format!("SELECT count(*) FROM {table} WHERE dataset = ?1"),
                [dataset],
                |row| row.get(0),
            )?;
            *slot = n.unsigned_abs();
        }
        Ok(counts)
    }

    /// Deletes what was written about a dataset that no longer exists, so a
    /// later dataset of the same name starts empty.
    pub fn forget_dataset(&self, dataset: &str) -> Result<(), AccountError> {
        let mut conn = self.conn();
        let transaction = conn.transaction()?;
        forget_curation(&transaction, dataset)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn count(&self) -> Result<u64, AccountError> {
        let count: i64 = self
            .conn()
            .query_row("SELECT count(*) FROM accounts", [], |row| row.get(0))?;
        Ok(count.unsigned_abs())
    }

    pub fn list(&self) -> Result<Vec<Account>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!("SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY username"))?;
        let accounts = statement.query_map([], read_account)?.collect::<Result<_, _>>()?;
        Ok(accounts)
    }

    pub fn get(&self, id: i64) -> Result<Account, AccountError> {
        self.conn()
            .query_row(
                &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = ?1"),
                [id],
                read_account,
            )
            .optional()?
            .ok_or(AccountError::NotFound)
    }

    pub fn create(&self, username: &str, password: &str, role: Role) -> Result<Account, AccountError> {
        self.insert(username, password, role, false)
    }

    /// Creates the first administrator. Fails once any account exists, so it
    /// is safe to offer before anyone has signed in.
    pub fn create_first_admin(&self, username: &str, password: &str) -> Result<Account, AccountError> {
        self.insert(username, password, Role::Admin, true)
    }

    fn insert(&self, username: &str, password: &str, role: Role, only_if_empty: bool) -> Result<Account, AccountError> {
        let username = normalise_username(username)?;
        check_password(password)?;
        let hash = hash_password(password)?;
        let conn = self.conn();
        // The emptiness check and the insert are one statement, so two
        // simultaneous set-up requests cannot both succeed.
        let inserted = conn.execute(
            "INSERT INTO accounts (username, password_hash, role)
             SELECT ?1, ?2, ?3 WHERE NOT ?4 OR NOT EXISTS (SELECT 1 FROM accounts)",
            params![username, hash, role.as_str(), only_if_empty],
        );
        match inserted {
            Ok(0) => Err(AccountError::AlreadySetUp),
            Ok(_) => Ok(conn.query_row(
                &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = ?1"),
                [conn.last_insert_rowid()],
                read_account,
            )?),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                Err(AccountError::UsernameTaken)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Returns the account when the username and password match an active
    /// account. Takes about the same time whether or not the username exists.
    /// Writes a consistent copy of the database to `to`, which must not
    /// exist yet, while the server keeps running (SQLite's `VACUUM INTO`).
    /// Deleted rows are left out of the copy.
    pub fn backup_to(&self, to: &Path) -> Result<(), AccountError> {
        let target = to
            .to_str()
            .ok_or(AccountError::BadBackup("the backup path must be valid UTF-8"))?;
        // Created empty and private first (VACUUM INTO accepts an empty
        // file), so the copy is never readable by others, not even briefly.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(to)
            .map_err(|_| AccountError::BadBackup("the backup file could not be created"))?;
        self.conn().execute("VACUUM INTO ?1", [target])?;
        Ok(())
    }

    /// Checks that `path` is a whole, undamaged Open Beam account database,
    /// without changing it.
    pub fn check_file(path: &Path) -> Result<(), AccountError> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let verdict: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        if verdict != "ok" {
            return Err(AccountError::BadBackup("the account database is damaged"));
        }
        let accounts: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = 'accounts'",
            [],
            |row| row.get(0),
        )?;
        if accounts != 1 {
            return Err(AccountError::BadBackup("this is not an Open Beam account database"));
        }
        Ok(())
    }

    /// The account a typed username names, if any: one indexed lookup.
    pub fn find(&self, username: &str) -> Result<Option<Account>, AccountError> {
        let Ok(username) = normalise_username(username) else {
            return Ok(None);
        };
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE username = ?1"),
                [username],
                read_account,
            )
            .optional()?)
    }

    pub fn verify(&self, username: &str, password: &str) -> Result<Option<Account>, AccountError> {
        let found = match normalise_username(username) {
            Ok(username) => self
                .conn()
                .query_row(
                    &format!("SELECT {ACCOUNT_COLUMNS}, password_hash FROM accounts WHERE username = ?1"),
                    [username],
                    |row| Ok((read_account(row)?, row.get::<_, String>(ACCOUNT_WIDTH)?)),
                )
                .optional()?,
            Err(_) => None,
        };
        let too_long = password.len() > MAX_PASSWORD_BYTES;
        let candidate = if too_long { "" } else { password };
        Ok(match found {
            Some((account, hash)) => {
                (verify_password(candidate, &hash) && account.active && !too_long).then_some(account)
            }
            None => {
                verify_password(candidate, &self.decoy_hash);
                None
            }
        })
    }

    /// Sets a new password and signs the account out everywhere.
    pub fn set_password(&self, id: i64, password: &str) -> Result<(), AccountError> {
        check_password(password)?;
        let hash = hash_password(password)?;
        let conn = self.conn();
        if conn.execute(
            "UPDATE accounts SET password_hash = ?1 WHERE id = ?2",
            params![hash, id],
        )? == 0
        {
            return Err(AccountError::NotFound);
        }
        conn.execute("DELETE FROM sessions WHERE account_id = ?1", [id])?;
        Ok(())
    }

    pub fn set_role(&self, id: i64, role: Role) -> Result<(), AccountError> {
        self.change(id, "role = ?1", role.as_str(), role != Role::Admin, false)
    }

    /// Deactivating an account signs it out everywhere.
    pub fn set_active(&self, id: i64, active: bool) -> Result<(), AccountError> {
        self.change(id, "active = ?1", active, !active, !active)
    }

    fn change(
        &self,
        id: i64,
        assignment: &str,
        value: impl rusqlite::ToSql,
        removes_admin: bool,
        sign_out: bool,
    ) -> Result<(), AccountError> {
        let mut conn = self.conn();
        let transaction = conn.transaction()?;
        let target: Option<(String, bool)> = transaction
            .query_row("SELECT role, active FROM accounts WHERE id = ?1", [id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let (role, active) = target.ok_or(AccountError::NotFound)?;
        if removes_admin && role == "admin" && active {
            let others: i64 = transaction.query_row(
                "SELECT count(*) FROM accounts WHERE role = 'admin' AND active AND id <> ?1",
                [id],
                |row| row.get(0),
            )?;
            if others == 0 {
                return Err(AccountError::LastAdministrator);
            }
        }
        transaction.execute(
            &format!("UPDATE accounts SET {assignment} WHERE id = ?2"),
            params![value, id],
        )?;
        if sign_out {
            transaction.execute("DELETE FROM sessions WHERE account_id = ?1", [id])?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Sets how colleagues see an account. Blank values clear the field.
    pub fn set_profile(&self, id: i64, display_name: &str, team: &str) -> Result<(), AccountError> {
        let display_name = clean_profile(display_name)?;
        let team = clean_profile(team)?;
        let changed = self.conn().execute(
            "UPDATE accounts SET display_name = ?1, team = ?2 WHERE id = ?3",
            params![display_name, team, id],
        )?;
        if changed == 0 {
            return Err(AccountError::NotFound);
        }
        Ok(())
    }

    pub fn set_mode(&self, id: i64, mode: Mode) -> Result<(), AccountError> {
        let changed = self.conn().execute(
            "UPDATE accounts SET mode = ?1 WHERE id = ?2",
            params![mode.as_str(), id],
        )?;
        if changed == 0 {
            return Err(AccountError::NotFound);
        }
        Ok(())
    }

    // ---------------------------------------------------------- contacts

    /// The ids of the accounts `owner` keeps as contacts.
    pub fn contacts(&self, owner: i64) -> Result<Vec<i64>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare("SELECT contact_id FROM contacts WHERE owner_id = ?1")?;
        let ids = statement
            .query_map([owner], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Adds a contact. Only active accounts can be added; adding twice is harmless.
    pub fn add_contact(&self, owner: i64, contact: i64) -> Result<(), AccountError> {
        if owner == contact || !self.get(contact)?.active {
            return Err(AccountError::NotFound);
        }
        self.conn().execute(
            "INSERT OR IGNORE INTO contacts (owner_id, contact_id) VALUES (?1, ?2)",
            params![owner, contact],
        )?;
        Ok(())
    }

    pub fn remove_contact(&self, owner: i64, contact: i64) -> Result<(), AccountError> {
        self.conn().execute(
            "DELETE FROM contacts WHERE owner_id = ?1 AND contact_id = ?2",
            params![owner, contact],
        )?;
        Ok(())
    }

    // ----------------------------------------------------------- uploads

    pub fn create_upload(&self, name: &str, owner: i64, file: &str) -> Result<Upload, AccountError> {
        let conn = self.conn();
        let inserted = conn.execute(
            "INSERT INTO uploads (name, owner_id, file) VALUES (?1, ?2, ?3)",
            params![name, owner, file],
        );
        match inserted {
            Ok(_) => Ok(conn.query_row(
                &format!("SELECT {UPLOAD_COLUMNS} FROM uploads WHERE id = ?1"),
                [conn.last_insert_rowid()],
                read_upload,
            )?),
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                Err(AccountError::NameTaken)
            }
            Err(e) => Err(e.into()),
        }
    }

    // -------------------------------------------------------- API tokens

    /// Creates a token and returns it with its secret. The secret is not
    /// stored and cannot be shown again.
    pub fn create_token(&self, account: i64, name: &str) -> Result<(ApiToken, String), AccountError> {
        let name = clean_profile(name)?.ok_or(AccountError::InvalidProfile)?;
        let secret = format!("{TOKEN_PREFIX}{}", random_token()?);
        let conn = self.conn();
        let held: i64 = conn.query_row(
            "SELECT count(*) FROM api_tokens WHERE account_id = ?1",
            [account],
            |r| r.get(0),
        )?;
        if held >= MAX_TOKENS {
            return Err(AccountError::TooManyTokens);
        }
        conn.execute(
            "INSERT INTO api_tokens (account_id, name, token_hash) VALUES (?1, ?2, ?3)",
            params![account, name, digest(&secret)],
        )?;
        let token = conn.query_row(
            "SELECT id, account_id, name, created_at, last_used FROM api_tokens WHERE id = ?1",
            [conn.last_insert_rowid()],
            read_token,
        )?;
        Ok((token, secret))
    }

    pub fn tokens(&self, account: i64) -> Result<Vec<ApiToken>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, account_id, name, created_at, last_used FROM api_tokens WHERE account_id = ?1 ORDER BY id",
        )?;
        let tokens = statement.query_map([account], read_token)?.collect::<Result<_, _>>()?;
        Ok(tokens)
    }

    /// Revokes one of `account`'s tokens. Another account's token is "not found".
    pub fn revoke_token(&self, account: i64, id: i64) -> Result<(), AccountError> {
        let removed = self.conn().execute(
            "DELETE FROM api_tokens WHERE id = ?1 AND account_id = ?2",
            params![id, account],
        )?;
        if removed == 0 {
            return Err(AccountError::NotFound);
        }
        Ok(())
    }

    /// The active account a token belongs to. Records when it was used.
    pub fn token_account(&self, secret: &str) -> Result<Option<Account>, AccountError> {
        if !secret.starts_with(TOKEN_PREFIX) || secret.len() > 128 {
            return Ok(None);
        }
        let conn = self.conn();
        let columns = ACCOUNT_COLUMNS.replace(", ", ", a.");
        let found = conn
            .query_row(
                &format!(
                    "SELECT a.{columns}, t.id FROM api_tokens AS t JOIN accounts AS a ON a.id = t.account_id
                     WHERE t.token_hash = ?1 AND a.active"
                ),
                [digest(secret)],
                |row| Ok((read_account(row)?, row.get::<_, i64>(ACCOUNT_WIDTH)?)),
            )
            .optional()?;
        let Some((account, token_id)) = found else {
            return Ok(None);
        };
        conn.execute(
            "UPDATE api_tokens SET last_used = datetime('now') WHERE id = ?1",
            [token_id],
        )?;
        Ok(Some(account))
    }

    // ------------------------------------------------------------ drafts

    pub fn create_draft(&self, owner: i64, file: &str, original_name: &str) -> Result<Draft, AccountError> {
        let original: String = original_name.chars().filter(|c| !c.is_control()).take(200).collect();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO drafts (owner_id, file, original_name) VALUES (?1, ?2, ?3)",
            params![owner, file, original],
        )?;
        Ok(conn.query_row(
            &format!("SELECT {DRAFT_COLUMNS} FROM drafts WHERE id = ?1"),
            [conn.last_insert_rowid()],
            read_draft,
        )?)
    }

    pub fn draft(&self, id: i64) -> Result<Option<Draft>, AccountError> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {DRAFT_COLUMNS} FROM drafts WHERE id = ?1"),
                [id],
                read_draft,
            )
            .optional()?)
    }

    pub fn drafts_of(&self, owner: i64) -> Result<Vec<Draft>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT {DRAFT_COLUMNS} FROM drafts WHERE owner_id = ?1 ORDER BY id"
        ))?;
        let drafts = statement.query_map([owner], read_draft)?.collect::<Result<_, _>>()?;
        Ok(drafts)
    }

    /// Drafts nobody saved or discarded within `days` days.
    pub fn expired_drafts(&self, days: u32) -> Result<Vec<Draft>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT {DRAFT_COLUMNS} FROM drafts WHERE created_at <= datetime('now', ?1)"
        ))?;
        let drafts = statement
            .query_map([format!("-{days} days")], read_draft)?
            .collect::<Result<_, _>>()?;
        Ok(drafts)
    }

    pub fn delete_draft(&self, id: i64) -> Result<(), AccountError> {
        if self.conn().execute("DELETE FROM drafts WHERE id = ?1", [id])? == 0 {
            return Err(AccountError::UploadNotFound);
        }
        Ok(())
    }

    /// Turns a reviewed draft into a dataset in one step: either both the
    /// dataset is recorded and the draft removed, or neither happens.
    pub fn save_draft(&self, draft: i64, name: &str, cleaning: Cleaning) -> Result<Upload, AccountError> {
        let mut conn = self.conn();
        let transaction = conn.transaction()?;
        let found: Option<(i64, String)> = transaction
            .query_row("SELECT owner_id, file FROM drafts WHERE id = ?1", [draft], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .optional()?;
        let (owner, file) = found.ok_or(AccountError::UploadNotFound)?;
        let inserted = transaction.execute(
            "INSERT INTO uploads (name, owner_id, file, drop_empty_rows, drop_duplicate_rows) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![name, owner, file, cleaning.drop_empty_rows, cleaning.drop_duplicate_rows],
        );
        match inserted {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _)) if e.code == rusqlite::ErrorCode::ConstraintViolation => {
                return Err(AccountError::NameTaken)
            }
            Err(e) => return Err(e.into()),
        }
        let id = transaction.last_insert_rowid();
        transaction.execute("DELETE FROM drafts WHERE id = ?1", [draft])?;
        // A new dataset never inherits what was written about an older one
        // that had the same name.
        forget_curation(&transaction, name)?;
        let upload = transaction.query_row(
            &format!("SELECT {UPLOAD_COLUMNS} FROM uploads WHERE id = ?1"),
            [id],
            read_upload,
        )?;
        transaction.commit()?;
        Ok(upload)
    }

    pub fn uploads(&self) -> Result<Vec<Upload>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!("SELECT {UPLOAD_COLUMNS} FROM uploads ORDER BY name"))?;
        let uploads = statement.query_map([], read_upload)?.collect::<Result<_, _>>()?;
        Ok(uploads)
    }

    pub fn upload_named(&self, name: &str) -> Result<Option<Upload>, AccountError> {
        Ok(self
            .conn()
            .query_row(
                &format!("SELECT {UPLOAD_COLUMNS} FROM uploads WHERE name = ?1"),
                [name],
                read_upload,
            )
            .optional()?)
    }

    pub fn delete_upload(&self, id: i64) -> Result<(), AccountError> {
        let mut conn = self.conn();
        let transaction = conn.transaction()?;
        let name: Option<String> = transaction
            .query_row("SELECT name FROM uploads WHERE id = ?1", [id], |row| row.get(0))
            .optional()?;
        let name = name.ok_or(AccountError::UploadNotFound)?;
        transaction.execute("DELETE FROM uploads WHERE id = ?1", [id])?;
        forget_curation(&transaction, &name)?;
        transaction.commit()?;
        Ok(())
    }

    /// The accounts an upload is shared with, besides its owner.
    pub fn shares(&self, upload: i64) -> Result<Vec<i64>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare("SELECT account_id FROM shares WHERE upload_id = ?1")?;
        let ids = statement
            .query_map([upload], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    pub fn share(&self, upload: i64, account: i64) -> Result<(), AccountError> {
        if !self.get(account)?.active {
            return Err(AccountError::NotFound);
        }
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO shares (upload_id, account_id) SELECT ?1, ?2 WHERE EXISTS (SELECT 1 FROM uploads WHERE id = ?1)",
            params![upload, account],
        )?;
        if inserted == 0 && !self.shares(upload)?.contains(&account) {
            return Err(AccountError::UploadNotFound);
        }
        Ok(())
    }

    pub fn unshare(&self, upload: i64, account: i64) -> Result<(), AccountError> {
        self.conn().execute(
            "DELETE FROM shares WHERE upload_id = ?1 AND account_id = ?2",
            params![upload, account],
        )?;
        Ok(())
    }

    /// Whether `account` may read `upload`: its owner, anyone it is shared
    /// with, and administrators.
    pub fn can_read(&self, account: &Account, upload: &Upload) -> Result<bool, AccountError> {
        if account.role == Role::Admin || account.id == upload.owner_id {
            return Ok(true);
        }
        Ok(self.shares(upload.id)?.contains(&account.id))
    }

    pub fn start_session(&self, account_id: i64) -> Result<NewSession, AccountError> {
        let session = NewSession {
            token: random_token()?,
            csrf: random_token()?,
        };
        let conn = self.conn();
        conn.execute(
            "INSERT INTO sessions (token_hash, account_id, csrf, expires_at) VALUES (?1, ?2, ?3, datetime('now', ?4))",
            params![
                digest(&session.token),
                account_id,
                session.csrf,
                format!("+{SESSION_HOURS} hours")
            ],
        )?;
        conn.execute(
            "UPDATE accounts SET last_sign_in = datetime('now') WHERE id = ?1",
            [account_id],
        )?;
        Ok(session)
    }

    /// Looks up an unexpired session that belongs to an active account.
    pub fn session(&self, token: &str) -> Result<Option<Session>, AccountError> {
        let columns = ACCOUNT_COLUMNS.replace(", ", ", a.");
        Ok(self
            .conn()
            .query_row(
                &format!(
                    "SELECT a.{columns}, s.csrf FROM sessions AS s JOIN accounts AS a ON a.id = s.account_id
                     WHERE s.token_hash = ?1 AND s.expires_at > datetime('now') AND a.active"
                ),
                [digest(token)],
                |row| {
                    Ok(Session {
                        account: read_account(row)?,
                        csrf: row.get(ACCOUNT_WIDTH)?,
                    })
                },
            )
            .optional()?)
    }

    /// Signs everyone out, for example after restoring a backup whose
    /// sessions may since have been ended. Returns how many there were.
    pub fn end_all_sessions(&self) -> Result<usize, AccountError> {
        Ok(self.conn().execute("DELETE FROM sessions", [])?)
    }

    pub fn end_session(&self, token: &str) -> Result<(), AccountError> {
        self.conn()
            .execute("DELETE FROM sessions WHERE token_hash = ?1", [digest(token)])?;
        Ok(())
    }
}

fn read_account(row: &Row<'_>) -> rusqlite::Result<Account> {
    let role: String = row.get(2)?;
    Ok(Account {
        id: row.get(0)?,
        username: row.get(1)?,
        // The CHECK constraint allows only the two known roles.
        role: Role::parse(&role).unwrap_or(Role::User),
        active: row.get(3)?,
        created_at: row.get(4)?,
        last_sign_in: row.get(5)?,
        display_name: row.get(6)?,
        team: row.get(7)?,
        // The CHECK constraint allows only the two known modes.
        mode: Mode::parse(&row.get::<_, String>(8)?).unwrap_or(Mode::Basic),
    })
}

fn read_upload(row: &Row<'_>) -> rusqlite::Result<Upload> {
    Ok(Upload {
        id: row.get(0)?,
        name: row.get(1)?,
        owner_id: row.get(2)?,
        file: row.get(3)?,
        created_at: row.get(4)?,
        cleaning: Cleaning {
            drop_empty_rows: row.get(5)?,
            drop_duplicate_rows: row.get(6)?,
        },
    })
}

fn read_token(row: &Row<'_>) -> rusqlite::Result<ApiToken> {
    Ok(ApiToken {
        id: row.get(0)?,
        account_id: row.get(1)?,
        name: row.get(2)?,
        created_at: row.get(3)?,
        last_used: row.get(4)?,
    })
}

fn read_draft(row: &Row<'_>) -> rusqlite::Result<Draft> {
    Ok(Draft {
        id: row.get(0)?,
        owner_id: row.get(1)?,
        file: row.get(2)?,
        original_name: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn clean_profile(text: &str) -> Result<Option<String>, AccountError> {
    let text = text.trim();
    if text.chars().count() > MAX_PROFILE_CHARS || text.chars().any(char::is_control) {
        return Err(AccountError::InvalidProfile);
    }
    Ok((!text.is_empty()).then(|| text.to_string()))
}

fn normalise_username(username: &str) -> Result<String, AccountError> {
    let username = username.trim().to_ascii_lowercase();
    let valid = (3..=32).contains(&username.len())
        && username
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    if valid {
        Ok(username)
    } else {
        Err(AccountError::InvalidUsername)
    }
}

fn check_password(password: &str) -> Result<(), AccountError> {
    if password.chars().count() < MIN_PASSWORD_CHARS || password.len() > MAX_PASSWORD_BYTES {
        return Err(AccountError::InvalidPassword);
    }
    if easy_to_guess(password) {
        return Err(AccountError::WeakPassword);
    }
    Ok(())
}

/// Words and runs at the top of every leaked-password list. Guessing tools
/// try these first, with digits and symbols added at the end.
const COMMON: &[&str] = &[
    "password",
    "passw0rd",
    "p@ssw0rd",
    "p@ssword",
    "qwerty",
    "qwertyuiop",
    "asdfghjkl",
    "zxcvbnm",
    "1q2w3e4r",
    "1q2w3e4r5t",
    "q1w2e3r4",
    "zaq12wsx",
    "abc123",
    "letmein",
    "welcome",
    "admin",
    "administrator",
    "iloveyou",
    "football",
    "baseball",
    "basketball",
    "dragon",
    "monkey",
    "sunshine",
    "princess",
    "master",
    "shadow",
    "superman",
    "batman",
    "trustno1",
    "changeme",
    "default",
    "secret",
    "login",
    "starwars",
    "whatever",
    "freedom",
    "michael",
    "jennifer",
    "computer",
    "internet",
    "openbeam",
    "open beam",
    "beam",
    "excel",
    "powerbi",
    "database",
    "analytics",
    "summer",
    "winter",
    "spring",
    "autumn",
    "christmas",
    "hello",
    "hellothere",
    "loveyou",
    "mypassword",
    "yourpassword",
    "newpassword",
    "temppassword",
    "useruser",
    "testtest",
    "test",
    "guest",
    "root",
    "toor",
    "user",
];

/// Letters and digits in the orders people type them without thinking.
const RUNS: &[&str] = &[
    "0123456789012345678901234567890",
    "9876543210987654321098765432109",
    "abcdefghijklmnopqrstuvwxyz",
    "zyxwvutsrqponmlkjihgfedcba",
    "qwertyuiopasdfghjklzxcvbnm",
    "mnbvcxzlkjhgfdsapoiuytrewq",
    "1qaz2wsx3edc4rfv5tgb6yhn7ujm8ik9ol0p",
];

/// True for a password that guessing tools find in their first few million
/// tries, however long it is: a common password with digits or symbols
/// around it, a run along the keyboard or the alphabet, or one that is
/// mostly a single character.
fn easy_to_guess(password: &str) -> bool {
    let lower = password.to_lowercase();
    let mut distinct: Vec<char> = lower.chars().collect();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() < 4 {
        return true;
    }
    if RUNS.iter().any(|run| run.contains(lower.as_str())) {
        return true;
    }
    let core = lower.trim_matches(|c: char| !c.is_alphabetic());
    COMMON.contains(&core)
}

fn hash_password(password: &str) -> Result<String, AccountError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|_| AccountError::Hashing)
}

fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
}

/// 256 bits from the operating system, as 64 hex characters.
fn random_token() -> Result<String, AccountError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| AccountError::Hashing)?;
    Ok(hex(&bytes))
}

fn digest(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------- what people write about data

/// What a dataset's owner wrote about it or one of its columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Empty for the dataset itself.
    pub column: String,
    pub text: String,
    pub updated_by: Option<String>,
    pub updated_at: String,
}

/// A question whose answer someone responsible for the data has checked.
/// `settings` holds how to compute it, as URL-encoded chart options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub id: i64,
    pub dataset: String,
    pub question: String,
    pub settings: String,
    pub approved_by: Option<String>,
    pub approved_at: String,
}

/// What a watch checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchKind {
    /// A total goes above or below a number.
    Total,
    /// The latest period differs from the one before by at least a percentage.
    Change,
    /// More than a number of records appear more than once.
    Repeats,
    /// The latest period falls outside the range the measure usually moves
    /// in, or the measure has shifted (a process behaviour chart).
    Steady,
}

impl WatchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchKind::Total => "total",
            WatchKind::Change => "change",
            WatchKind::Repeats => "repeats",
            WatchKind::Steady => "steady",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "total" => Some(WatchKind::Total),
            "change" => Some(WatchKind::Change),
            "repeats" => Some(WatchKind::Repeats),
            "steady" => Some(WatchKind::Steady),
            _ => None,
        }
    }
}

/// Outcome of a watch's latest run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchStatus {
    Ok,
    Alert,
    /// The check itself could not run, for example after a column was removed.
    Error,
}

impl WatchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchStatus::Ok => "ok",
            WatchStatus::Alert => "alert",
            WatchStatus::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Watch {
    pub id: i64,
    pub dataset: String,
    pub name: String,
    pub kind: WatchKind,
    pub settings: String,
    pub threshold: f64,
    pub created_by: Option<String>,
    pub last_run: Option<String>,
    pub status: Option<WatchStatus>,
    pub message: Option<String>,
    pub value: Option<f64>,
}

fn forget_curation(conn: &Connection, dataset: &str) -> Result<(), rusqlite::Error> {
    for table in ["notes", "questions", "watches"] {
        conn.execute(&format!("DELETE FROM {table} WHERE dataset = ?1"), [dataset])?;
    }
    Ok(())
}

/// Trims `text` and checks its length; line breaks are allowed in notes.
fn clean_text(text: &str, max: usize, multi_line: bool) -> Result<String, AccountError> {
    let text = text.trim();
    let bad = |c: char| c.is_control() && !(multi_line && (c == '\n' || c == '\r' || c == '\t'));
    if text.chars().count() > max || text.chars().any(bad) {
        return Err(AccountError::InvalidText);
    }
    Ok(text.replace("\r\n", "\n"))
}

const WATCH_COLUMNS: &str = "w.id, w.dataset, w.name, w.kind, w.settings, w.threshold, a.username, \
     w.last_run, w.last_status, w.last_message, w.last_value";

fn read_watch(row: &rusqlite::Row<'_>) -> rusqlite::Result<Watch> {
    let kind: String = row.get(3)?;
    let status: Option<String> = row.get(8)?;
    Ok(Watch {
        id: row.get(0)?,
        dataset: row.get(1)?,
        name: row.get(2)?,
        kind: WatchKind::parse(&kind).unwrap_or(WatchKind::Total),
        settings: row.get(4)?,
        threshold: row.get(5)?,
        created_by: row.get(6)?,
        last_run: row.get(7)?,
        status: status.as_deref().and_then(|s| match s {
            "ok" => Some(WatchStatus::Ok),
            "alert" => Some(WatchStatus::Alert),
            "error" => Some(WatchStatus::Error),
            _ => None,
        }),
        message: row.get(9)?,
        value: row.get(10)?,
    })
}

impl Store {
    /// Everything written about `dataset`: the dataset's own note first,
    /// then one per column.
    pub fn notes(&self, dataset: &str) -> Result<Vec<Note>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT n.column_name, n.text, a.username, n.updated_at FROM notes n \
             LEFT JOIN accounts a ON a.id = n.updated_by WHERE n.dataset = ?1 ORDER BY n.column_name",
        )?;
        let notes = statement
            .query_map([dataset], |row| {
                Ok(Note {
                    column: row.get(0)?,
                    text: row.get(1)?,
                    updated_by: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(notes)
    }

    /// Writes the note for `dataset`, or one of its columns when `column`
    /// is not empty. Empty text removes the note.
    pub fn set_note(&self, dataset: &str, column: &str, text: &str, by: i64) -> Result<(), AccountError> {
        let max = if column.is_empty() {
            MAX_NOTE_CHARS
        } else {
            MAX_NOTE_CHARS / 4
        };
        let text = clean_text(text, max, true)?;
        let conn = self.conn();
        if text.is_empty() {
            conn.execute(
                "DELETE FROM notes WHERE dataset = ?1 AND column_name = ?2",
                rusqlite::params![dataset, column],
            )?;
        } else {
            conn.execute(
                "INSERT INTO notes (dataset, column_name, text, updated_by) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (dataset, column_name) DO UPDATE SET text = excluded.text, \
                 updated_by = excluded.updated_by, updated_at = datetime('now')",
                rusqlite::params![dataset, column, text, by],
            )?;
        }
        Ok(())
    }

    pub fn questions(&self, dataset: &str) -> Result<Vec<Question>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT q.id, q.dataset, q.question, q.settings, a.username, q.approved_at FROM questions q \
             LEFT JOIN accounts a ON a.id = q.approved_by WHERE q.dataset = ?1 ORDER BY q.id",
        )?;
        let questions = statement
            .query_map([dataset], |row| {
                Ok(Question {
                    id: row.get(0)?,
                    dataset: row.get(1)?,
                    question: row.get(2)?,
                    settings: row.get(3)?,
                    approved_by: row.get(4)?,
                    approved_at: row.get(5)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(questions)
    }

    pub fn add_question(&self, dataset: &str, question: &str, settings: &str, by: i64) -> Result<i64, AccountError> {
        let question = clean_text(question, MAX_TITLE_CHARS, false)?;
        if question.is_empty() || settings.len() > MAX_SETTINGS_BYTES {
            return Err(AccountError::InvalidText);
        }
        let conn = self.conn();
        let held: i64 = conn.query_row("SELECT count(*) FROM questions WHERE dataset = ?1", [dataset], |r| {
            r.get(0)
        })?;
        if held >= MAX_PER_DATASET {
            return Err(AccountError::TooMany("verified questions"));
        }
        conn.execute(
            "INSERT INTO questions (dataset, question, settings, approved_by) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![dataset, question, settings, by],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Removes a question, but only from the dataset named.
    pub fn remove_question(&self, dataset: &str, id: i64) -> Result<(), AccountError> {
        let removed = self.conn().execute(
            "DELETE FROM questions WHERE id = ?1 AND dataset = ?2",
            rusqlite::params![id, dataset],
        )?;
        if removed == 0 {
            return Err(AccountError::ItemNotFound);
        }
        Ok(())
    }

    /// Watches on one dataset, or on every dataset when `dataset` is `None`.
    pub fn watches(&self, dataset: Option<&str>) -> Result<Vec<Watch>, AccountError> {
        let conn = self.conn();
        let mut statement = conn.prepare(&format!(
            "SELECT {WATCH_COLUMNS} FROM watches w LEFT JOIN accounts a ON a.id = w.created_by \
             WHERE ?1 IS NULL OR w.dataset = ?1 ORDER BY w.dataset, w.id"
        ))?;
        let watches = statement.query_map([dataset], read_watch)?.collect::<Result<_, _>>()?;
        Ok(watches)
    }

    pub fn add_watch(
        &self,
        dataset: &str,
        name: &str,
        kind: WatchKind,
        settings: &str,
        threshold: f64,
        by: i64,
    ) -> Result<i64, AccountError> {
        let name = clean_text(name, MAX_TITLE_CHARS, false)?;
        if name.is_empty() || !threshold.is_finite() || settings.len() > MAX_SETTINGS_BYTES {
            return Err(AccountError::InvalidText);
        }
        let conn = self.conn();
        let (held, total): (i64, i64) = conn.query_row(
            "SELECT count(*) FILTER (WHERE dataset = ?1), count(*) FROM watches",
            [dataset],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if held >= MAX_PER_DATASET || total >= MAX_WATCHES {
            return Err(AccountError::TooMany("watches"));
        }
        conn.execute(
            "INSERT INTO watches (dataset, name, kind, settings, threshold, created_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![dataset, name, kind.as_str(), settings, threshold, by],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn remove_watch(&self, dataset: &str, id: i64) -> Result<(), AccountError> {
        let removed = self.conn().execute(
            "DELETE FROM watches WHERE id = ?1 AND dataset = ?2",
            rusqlite::params![id, dataset],
        )?;
        if removed == 0 {
            return Err(AccountError::ItemNotFound);
        }
        Ok(())
    }

    /// Records the outcome of a watch's latest run.
    pub fn record_watch(
        &self,
        id: i64,
        status: WatchStatus,
        message: &str,
        value: Option<f64>,
    ) -> Result<(), AccountError> {
        self.conn().execute(
            "UPDATE watches SET last_run = datetime('now'), last_status = ?2, last_message = ?3, last_value = ?4 WHERE id = ?1",
            rusqlite::params![id, status.as_str(), message, value.filter(|v| v.is_finite())],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn account_files_from_before_steady_watches_are_widened_without_losing_any() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            // An account file as 0.3 wrote it, with one watch in it.
            let old = super::Store::open(&path).unwrap();
            let admin = old.create_first_admin("ada", "correct horse battery").unwrap();
            old.add_watch("sales", "dupes", super::WatchKind::Repeats, "keys=id", 0.0, admin.id)
                .unwrap();
            let conn = old.conn();
            conn.execute_batch(&format!(
                "CREATE TABLE narrow {};
                 INSERT INTO narrow SELECT * FROM watches; DROP TABLE watches;
                 ALTER TABLE narrow RENAME TO watches;",
                super::WATCHES_COLUMNS.replace(", 'steady'", "")
            ))
            .unwrap();
        }
        let store = super::Store::open(&path).unwrap();
        let admin = store.list().unwrap()[0].id;
        assert_eq!(store.watches(None).unwrap().len(), 1, "the old watch survived");
        store
            .add_watch("sales", "usual", super::WatchKind::Steady, "agg=count", 0.0, admin)
            .unwrap();
        assert_eq!(store.watches(None).unwrap().len(), 2);
        // Opening again changes nothing.
        drop(store);
        assert_eq!(super::Store::open(&path).unwrap().watches(None).unwrap().len(), 2);
    }

    #[test]
    fn guessable_passwords_are_refused_and_ordinary_phrases_are_not() {
        for weak in [
            "Password123!",
            "password2026",
            "1234567890",
            "0987654321",
            "qwertyuiop",
            "Summer2026!",
            "aaaaaaaaaaaa",
            "abababababab",
            "!!admin2024!!",
            "abcdefghijk",
        ] {
            assert!(super::easy_to_guess(weak), "{weak} should be refused");
        }
        for fine in [
            "correct horse battery",
            "meadow lantern orbit",
            "a brand new password",
            "8392017465",
            "Kx7#pQ2vLm",
        ] {
            assert!(!super::easy_to_guess(fine), "{fine} should be accepted");
        }
        assert!(matches!(
            super::check_password("Password12345678!"),
            Err(super::AccountError::WeakPassword)
        ));
        // NIST SP 800-63B-4: fourteen characters is one too few.
        assert!(matches!(
            super::check_password("fourteen chars"),
            Err(super::AccountError::InvalidPassword)
        ));
        assert!(super::check_password("fifteen letters").is_ok());
    }

    use super::*;

    const PASSWORD: &str = "correct horse battery";

    fn store_with_admin() -> (Store, Account) {
        let store = Store::open_in_memory().unwrap();
        let admin = store.create_first_admin("Ada", PASSWORD).unwrap();
        (store, admin)
    }

    #[test]
    fn first_admin_can_be_created_exactly_once() {
        let (store, admin) = store_with_admin();
        assert_eq!(
            (admin.username.as_str(), admin.role, admin.active),
            ("ada", Role::Admin, true)
        );
        assert!(matches!(
            store.create_first_admin("eve", PASSWORD),
            Err(AccountError::AlreadySetUp)
        ));
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn rejects_bad_usernames_passwords_and_duplicates() {
        let (store, _) = store_with_admin();
        for username in ["ab", "has space", "<script>", "", &"x".repeat(33)] {
            assert!(matches!(
                store.create(username, PASSWORD, Role::User),
                Err(AccountError::InvalidUsername)
            ));
        }
        assert!(matches!(
            store.create("bob", "short", Role::User),
            Err(AccountError::InvalidPassword)
        ));
        assert!(matches!(
            store.create("bob", &"x".repeat(257), Role::User),
            Err(AccountError::InvalidPassword)
        ));
        assert!(matches!(
            store.create("ADA", PASSWORD, Role::User),
            Err(AccountError::UsernameTaken)
        ));
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn verifies_only_the_right_password_for_an_active_account() {
        let (store, admin) = store_with_admin();
        let bob = store.create("bob", PASSWORD, Role::User).unwrap();
        assert_eq!(store.verify(" Ada ", PASSWORD).unwrap().map(|a| a.id), Some(admin.id));
        assert_eq!(store.verify("ada", "wrong password!").unwrap(), None);
        assert_eq!(store.verify("nobody", PASSWORD).unwrap(), None);
        assert_eq!(store.verify("ada", &"x".repeat(100_000)).unwrap(), None);
        store.set_active(bob.id, false).unwrap();
        assert_eq!(store.verify("bob", PASSWORD).unwrap(), None);
    }

    #[test]
    fn passwords_and_tokens_are_not_stored_in_the_clear() {
        let (store, admin) = store_with_admin();
        let session = store.start_session(admin.id).unwrap();
        let conn = store.conn();
        let hash: String = conn
            .query_row("SELECT password_hash FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(!hash.contains(PASSWORD));
        let stored: String = conn
            .query_row("SELECT token_hash FROM sessions", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, session.token);
        assert_eq!(session.token.len(), 64);
    }

    #[test]
    fn sessions_end_on_sign_out_expiry_password_change_and_deactivation() {
        let (store, admin) = store_with_admin();
        let bob = store.create("bob", PASSWORD, Role::User).unwrap();

        let first = store.start_session(bob.id).unwrap();
        let session = store.session(&first.token).unwrap().unwrap();
        assert_eq!((session.account.id, session.csrf), (bob.id, first.csrf));
        assert!(session.account.last_sign_in.is_some());
        assert_eq!(store.session("not-a-token").unwrap(), None);

        store.end_session(&first.token).unwrap();
        assert_eq!(store.session(&first.token).unwrap(), None);

        let expired = store.start_session(bob.id).unwrap();
        store
            .conn()
            .execute("UPDATE sessions SET expires_at = datetime('now', '-1 second')", [])
            .unwrap();
        assert_eq!(store.session(&expired.token).unwrap(), None);

        let before_change = store.start_session(bob.id).unwrap();
        store.set_password(bob.id, "a different password").unwrap();
        assert_eq!(store.session(&before_change.token).unwrap(), None);
        assert!(store.verify("bob", "a different password").unwrap().is_some());
        assert!(store.verify("bob", PASSWORD).unwrap().is_none());

        let before_disable = store.start_session(bob.id).unwrap();
        store.set_active(bob.id, false).unwrap();
        assert_eq!(store.session(&before_disable.token).unwrap(), None);

        // Other accounts are unaffected.
        let admin_session = store.start_session(admin.id).unwrap();
        assert!(store.session(&admin_session.token).unwrap().is_some());
    }

    #[test]
    fn the_last_active_administrator_cannot_be_removed() {
        let (store, admin) = store_with_admin();
        assert!(matches!(
            store.set_role(admin.id, Role::User),
            Err(AccountError::LastAdministrator)
        ));
        assert!(matches!(
            store.set_active(admin.id, false),
            Err(AccountError::LastAdministrator)
        ));

        let second = store.create("grace", PASSWORD, Role::Admin).unwrap();
        store.set_active(second.id, false).unwrap();
        // A deactivated administrator does not count.
        assert!(matches!(
            store.set_role(admin.id, Role::User),
            Err(AccountError::LastAdministrator)
        ));
        store.set_active(second.id, true).unwrap();
        store.set_role(admin.id, Role::User).unwrap();
        assert_eq!(store.get(admin.id).unwrap().role, Role::User);
        assert!(matches!(store.set_role(999, Role::User), Err(AccountError::NotFound)));
    }

    #[test]
    fn accounts_persist_in_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts.sqlite");
        Store::open(&path).unwrap().create_first_admin("ada", PASSWORD).unwrap();
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.list().unwrap().len(), 1);
        assert!(reopened.verify("ada", PASSWORD).unwrap().is_some());
    }

    #[test]
    fn profiles_and_modes() {
        let (store, admin) = store_with_admin();
        assert_eq!(
            (admin.mode, admin.display_name.as_deref(), admin.name()),
            (Mode::Basic, None, "ada")
        );
        store.set_profile(admin.id, "  Ada Lovelace ", "Analysis").unwrap();
        store.set_mode(admin.id, Mode::Advanced).unwrap();
        let ada = store.get(admin.id).unwrap();
        assert_eq!(
            (ada.name(), ada.team.as_deref(), ada.mode),
            ("Ada Lovelace", Some("Analysis"), Mode::Advanced)
        );
        store.set_profile(admin.id, "", "").unwrap();
        assert_eq!(store.get(admin.id).unwrap().display_name, None);
        assert!(matches!(
            store.set_profile(admin.id, &"x".repeat(65), ""),
            Err(AccountError::InvalidProfile)
        ));
        assert!(matches!(
            store.set_profile(admin.id, "bad\nname", ""),
            Err(AccountError::InvalidProfile)
        ));
        assert!(matches!(store.set_mode(999, Mode::Basic), Err(AccountError::NotFound)));
        // Sessions and sign-in still read the right columns after the additions.
        let session = store.start_session(admin.id).unwrap();
        assert_eq!(store.session(&session.token).unwrap().unwrap().csrf, session.csrf);
        assert!(store.verify("ada", PASSWORD).unwrap().is_some());
    }

    #[test]
    fn contacts_are_personal_and_only_for_active_accounts() {
        let (store, admin) = store_with_admin();
        let bob = store.create("bob", PASSWORD, Role::User).unwrap();
        let cy = store.create("cyd", PASSWORD, Role::User).unwrap();
        store.add_contact(bob.id, admin.id).unwrap();
        store.add_contact(bob.id, admin.id).unwrap();
        assert_eq!(store.contacts(bob.id).unwrap(), vec![admin.id]);
        assert_eq!(store.contacts(admin.id).unwrap(), Vec::<i64>::new());
        assert!(matches!(store.add_contact(bob.id, bob.id), Err(AccountError::NotFound)));
        assert!(matches!(store.add_contact(bob.id, 999), Err(AccountError::NotFound)));
        store.set_active(cy.id, false).unwrap();
        assert!(matches!(store.add_contact(bob.id, cy.id), Err(AccountError::NotFound)));
        store.remove_contact(bob.id, admin.id).unwrap();
        assert!(store.contacts(bob.id).unwrap().is_empty());
    }

    #[test]
    fn notes_questions_and_watches_belong_to_one_dataset_and_go_with_it() {
        let (store, admin) = store_with_admin();
        let upload = store.create_upload("budget", admin.id, "a1.csv").unwrap();
        store
            .set_note("budget", "", "Spending per team.\r\nFrom the ledger.", admin.id)
            .unwrap();
        store
            .set_note("BUDGET", "amount", "In euros, VAT included.", admin.id)
            .unwrap();
        let notes = store.notes("Budget").unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].column, "");
        assert_eq!(notes[0].text, "Spending per team.\nFrom the ledger.");
        assert_eq!(notes[1].updated_by.as_deref(), Some("ada"));
        store.set_note("budget", "amount", "  ", admin.id).unwrap();
        assert_eq!(store.notes("budget").unwrap().len(), 1, "empty text removes a note");
        assert!(matches!(
            store.set_note("budget", "", &"x".repeat(MAX_NOTE_CHARS + 1), admin.id),
            Err(AccountError::InvalidText)
        ));
        assert!(matches!(
            store.add_question("budget", "Total?\u{7}", "group=team", admin.id),
            Err(AccountError::InvalidText)
        ));

        let question = store
            .add_question("budget", "Spending by team", "group=team&agg=sum", admin.id)
            .unwrap();
        let watch = store
            .add_watch(
                "budget",
                "Spending over 10,000",
                WatchKind::Total,
                "agg=sum&measure=amount&when=above",
                10_000.0,
                admin.id,
            )
            .unwrap();
        assert!(matches!(
            store.remove_question("other", question),
            Err(AccountError::ItemNotFound)
        ));
        store
            .record_watch(watch, WatchStatus::Alert, "Sum of amount is 12,000.", Some(12_000.0))
            .unwrap();
        let watched = &store.watches(Some("budget")).unwrap()[0];
        assert_eq!(
            (watched.status, watched.value),
            (Some(WatchStatus::Alert), Some(12_000.0))
        );
        assert_eq!(watched.created_by.as_deref(), Some("ada"));
        assert_eq!(store.watches(None).unwrap().len(), 1);

        // Deleting the dataset deletes what was written about it.
        store.delete_upload(upload.id).unwrap();
        assert!(store.notes("budget").unwrap().is_empty());
        assert!(store.questions("budget").unwrap().is_empty());
        assert!(store.watches(None).unwrap().is_empty());

        // A new dataset that reuses a free name starts with nothing, even if
        // notes were left behind by a built-in dataset of that name.
        store.set_note("ledger", "", "Old built-in ledger.", admin.id).unwrap();
        let draft = store.create_draft(admin.id, "b2.csv", "ledger.csv").unwrap();
        store.save_draft(draft.id, "ledger", Cleaning::default()).unwrap();
        assert!(store.notes("ledger").unwrap().is_empty());

        let huge = "keys=".to_string() + &"a,".repeat(MAX_SETTINGS_BYTES);
        assert!(matches!(
            store.add_watch("budget", "big", WatchKind::Repeats, &huge, 0.0, admin.id),
            Err(AccountError::InvalidText)
        ));
        assert!(matches!(
            store.add_question("budget", "big", &huge, admin.id),
            Err(AccountError::InvalidText)
        ));
        for n in 0..MAX_WATCHES {
            let dataset = format!("d{}", n / MAX_PER_DATASET);
            store
                .add_watch(&dataset, "w", WatchKind::Repeats, "keys=a", 0.0, admin.id)
                .unwrap();
        }
        assert!(
            matches!(
                store.add_watch("fresh", "one more", WatchKind::Repeats, "keys=a", 0.0, admin.id),
                Err(AccountError::TooMany(_))
            ),
            "the total across datasets is capped"
        );
        for n in 0..MAX_PER_DATASET {
            store
                .add_question("full", &format!("q{n}"), "group=a", admin.id)
                .unwrap();
        }
        assert!(matches!(
            store.add_question("full", "one more", "group=a", admin.id),
            Err(AccountError::TooMany(_))
        ));
    }

    #[test]
    fn uploads_are_readable_by_owner_shares_and_administrators_only() {
        let (store, admin) = store_with_admin();
        let bob = store.create("bob", PASSWORD, Role::User).unwrap();
        let cy = store.create("cyd", PASSWORD, Role::User).unwrap();
        let upload = store.create_upload("budget", bob.id, "a1.csv").unwrap();
        assert!(matches!(
            store.create_upload("BUDGET", cy.id, "b2.csv"),
            Err(AccountError::NameTaken)
        ));

        assert!(store.can_read(&bob, &upload).unwrap());
        assert!(store.can_read(&admin, &upload).unwrap());
        assert!(!store.can_read(&cy, &upload).unwrap());
        store.share(upload.id, cy.id).unwrap();
        store.share(upload.id, cy.id).unwrap();
        assert!(store.can_read(&cy, &upload).unwrap());
        assert_eq!(store.shares(upload.id).unwrap(), vec![cy.id]);
        store.unshare(upload.id, cy.id).unwrap();
        assert!(!store.can_read(&cy, &upload).unwrap());
        assert!(matches!(store.share(999, cy.id), Err(AccountError::UploadNotFound)));

        assert_eq!(
            store.upload_named("budget").unwrap().map(|u| u.file),
            Some("a1.csv".into())
        );
        store.share(upload.id, cy.id).unwrap();
        store.delete_upload(upload.id).unwrap();
        assert!(store.uploads().unwrap().is_empty());
        assert!(store.shares(upload.id).unwrap().is_empty(), "shares go with the upload");
        assert!(matches!(
            store.delete_upload(upload.id),
            Err(AccountError::UploadNotFound)
        ));
    }

    #[test]
    fn files_from_0_2_gain_the_new_columns_without_losing_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE accounts (
                     id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE, password_hash TEXT NOT NULL,
                     role TEXT NOT NULL CHECK (role IN ('admin', 'user')), active INTEGER NOT NULL DEFAULT 1,
                     created_at TEXT NOT NULL DEFAULT (datetime('now')), last_sign_in TEXT);",
            )
            .unwrap();
            let hash = hash_password(PASSWORD).unwrap();
            conn.execute(
                "INSERT INTO accounts (username, password_hash, role) VALUES ('ada', ?1, 'admin')",
                [hash],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let ada = store.verify("ada", PASSWORD).unwrap().unwrap();
        assert_eq!((ada.mode, ada.display_name), (Mode::Basic, None));
        drop(store);
        // Opening again must not try to add the columns twice.
        assert_eq!(Store::open(&path).unwrap().count().unwrap(), 1);
    }

    #[test]
    fn drafts_are_saved_into_datasets_all_or_nothing() {
        let (store, admin) = store_with_admin();
        let draft = store.create_draft(admin.id, "f1.csv", "Budget\u{0}.csv").unwrap();
        assert_eq!(draft.original_name, "Budget.csv", "control characters are dropped");
        assert_eq!(store.drafts_of(admin.id).unwrap(), vec![draft.clone()]);
        assert!(store.uploads().unwrap().is_empty(), "a draft is not a dataset");

        store.create_upload("budget", admin.id, "other.csv").unwrap();
        let cleaning = Cleaning {
            drop_empty_rows: true,
            drop_duplicate_rows: false,
        };
        assert!(matches!(
            store.save_draft(draft.id, "BUDGET", cleaning),
            Err(AccountError::NameTaken)
        ));
        assert_eq!(
            store.drafts_of(admin.id).unwrap().len(),
            1,
            "a failed save keeps the draft"
        );

        let saved = store.save_draft(draft.id, "budget_2", cleaning).unwrap();
        assert_eq!((saved.file.as_str(), saved.cleaning), ("f1.csv", cleaning));
        assert_eq!(store.upload_named("budget_2").unwrap().unwrap().cleaning, cleaning);
        assert!(store.drafts_of(admin.id).unwrap().is_empty());
        assert!(matches!(
            store.save_draft(draft.id, "again", cleaning),
            Err(AccountError::UploadNotFound)
        ));

        let old = store.create_draft(admin.id, "f2.csv", "old.csv").unwrap();
        assert!(store.expired_drafts(7).unwrap().is_empty());
        store
            .conn()
            .execute("UPDATE drafts SET created_at = datetime('now', '-8 days')", [])
            .unwrap();
        assert_eq!(
            store.expired_drafts(7).unwrap(),
            vec![store.draft(old.id).unwrap().unwrap()]
        );
        store.delete_draft(old.id).unwrap();
        assert!(matches!(store.delete_draft(old.id), Err(AccountError::UploadNotFound)));
    }

    #[test]
    fn api_tokens_are_shown_once_stored_hashed_and_revocable() {
        let (store, admin) = store_with_admin();
        let bob = store.create("bob", PASSWORD, Role::User).unwrap();
        let (token, secret) = store.create_token(bob.id, "Power BI").unwrap();
        assert!(secret.starts_with(TOKEN_PREFIX) && secret.len() == TOKEN_PREFIX.len() + 64);
        let stored: String = store
            .conn()
            .query_row("SELECT token_hash FROM api_tokens", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, secret);
        assert!(!stored.contains(&secret[4..]));

        assert_eq!(store.token_account(&secret).unwrap().map(|a| a.id), Some(bob.id));
        assert!(store.tokens(bob.id).unwrap()[0].last_used.is_some(), "use is recorded");
        assert_eq!(store.token_account("obt_wrong").unwrap(), None);
        assert_eq!(
            store.token_account(&secret[4..]).unwrap(),
            None,
            "the prefix is required"
        );
        assert_eq!(store.token_account(&"obt_x".repeat(100)).unwrap(), None);

        assert!(
            matches!(store.revoke_token(admin.id, token.id), Err(AccountError::NotFound)),
            "only its owner revokes it"
        );
        store.set_active(bob.id, false).unwrap();
        assert_eq!(
            store.token_account(&secret).unwrap(),
            None,
            "a disabled account's tokens stop working"
        );
        store.set_active(bob.id, true).unwrap();
        store.revoke_token(bob.id, token.id).unwrap();
        assert_eq!(store.token_account(&secret).unwrap(), None);

        assert!(matches!(
            store.create_token(bob.id, "  "),
            Err(AccountError::InvalidProfile)
        ));
        for n in 0..MAX_TOKENS {
            store.create_token(bob.id, &format!("t{n}")).unwrap();
        }
        assert!(matches!(
            store.create_token(bob.id, "one more"),
            Err(AccountError::TooManyTokens)
        ));
    }
}
