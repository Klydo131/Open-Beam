//! Accounts, roles and sessions for Open Beam.
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

pub const MIN_PASSWORD_CHARS: usize = 10;
/// Upper bound so that hashing cannot be turned into a way to waste CPU.
pub const MAX_PASSWORD_BYTES: usize = 256;
const SESSION_HOURS: u32 = 12;

#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("Usernames are 3 to 32 characters: lowercase letters, digits, dot, dash or underscore.")]
    InvalidUsername,
    #[error("Passwords must be between {MIN_PASSWORD_CHARS} characters and {MAX_PASSWORD_BYTES} bytes long.")]
    InvalidPassword,
    #[error("That username is already in use.")]
    UsernameTaken,
    #[error("There must always be at least one active administrator.")]
    LastAdministrator,
    #[error("An administrator account already exists.")]
    AlreadySetUp,
    #[error("No such account.")]
    NotFound,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: i64,
    pub username: String,
    pub role: Role,
    pub active: bool,
    /// UTC, `YYYY-MM-DD HH:MM:SS`.
    pub created_at: String,
    pub last_sign_in: Option<String>,
}

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

const ACCOUNT_COLUMNS: &str = "id, username, role, active, created_at, last_sign_in";

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
             DELETE FROM sessions WHERE expires_at <= datetime('now');",
        )?;
        let decoy_hash = hash_password(&random_token()?)?;
        Ok(Self {
            conn: Mutex::new(conn),
            decoy_hash,
        })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
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
    pub fn verify(&self, username: &str, password: &str) -> Result<Option<Account>, AccountError> {
        let found = match normalise_username(username) {
            Ok(username) => self
                .conn()
                .query_row(
                    &format!("SELECT {ACCOUNT_COLUMNS}, password_hash FROM accounts WHERE username = ?1"),
                    [username],
                    |row| Ok((read_account(row)?, row.get::<_, String>(6)?)),
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
                        csrf: row.get(6)?,
                    })
                },
            )
            .optional()?)
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
    })
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
    if password.chars().count() >= MIN_PASSWORD_CHARS && password.len() <= MAX_PASSWORD_BYTES {
        Ok(())
    } else {
        Err(AccountError::InvalidPassword)
    }
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

#[cfg(test)]
mod tests {
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
}
