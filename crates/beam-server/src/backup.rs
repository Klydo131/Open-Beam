//! `beam backup` and `beam restore`: everything Open Beam keeps that cannot
//! be made again, copied to a folder and put back from one.
//!
//! A backup holds the account database (accounts, descriptions, verified
//! questions, watches, sharing), the model file, and the uploaded files. It
//! leaves out the fast copies and temporary files, which are made again,
//! and the data files named in `beam.toml`, which are yours already.
//!
//! - The account database is copied with SQLite's `VACUUM INTO`, which
//!   gives a consistent copy while the server keeps running and leaves out
//!   deleted rows.
//! - `SHA256SUMS` lists every file with its digest, in the format
//!   `sha256sum -c` reads, and is written last: a folder without it is not
//!   a finished backup.
//! - Restoring checks every digest and the database before changing
//!   anything, refuses while the server is running, and keeps what it
//!   replaces beside it, so a restore can itself be undone.

use crate::config::Config;
use beam_accounts::Store;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

const ACCOUNTS: &str = "accounts.sqlite";
const MODEL: &str = "model.toml";
const UPLOADS: &str = "uploads";
const SUMS: &str = "SHA256SUMS";

fn digest_of(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Copies `from` to `to` and makes sure it reached the disk.
fn copy_synced(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to).map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
    std::fs::File::open(to)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", to.display()))
}

/// Uploaded files: plain files directly in the folder, without the
/// working folders (`.cache`, `.spill`) and unfinished writes.
fn uploaded_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        if kind.is_file() && !name.starts_with('.') && !name.ends_with(".partial") {
            files.push(entry.path());
        }
    }
    files.sort();
    Ok(files)
}

/// Writes a backup into `to`, a folder that is new or empty.
pub fn backup(config: &Config, to: &Path) -> Result<String, String> {
    if to.exists()
        && std::fs::read_dir(to)
            .map_err(|e| format!("cannot read {}: {e}", to.display()))?
            .next()
            .is_some()
    {
        return Err(format!("{} is not empty; back up into a new folder", to.display()));
    }
    std::fs::create_dir_all(to.join(UPLOADS)).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    let mut written = Vec::new();

    let accounts =
        Store::open(&config.accounts).map_err(|e| format!("cannot open {}: {e}", config.accounts.display()))?;
    accounts
        .backup_to(&to.join(ACCOUNTS))
        .map_err(|e| format!("cannot copy the account database: {e}"))?;
    written.push(ACCOUNTS.to_string());

    if let Some(model) = config.ontology.as_deref().filter(|p| p.exists()) {
        copy_synced(model, &to.join(MODEL))?;
        written.push(MODEL.to_string());
    }
    for file in uploaded_files(&config.uploads)? {
        let name = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
        copy_synced(&file, &to.join(UPLOADS).join(&name))?;
        written.push(format!("{UPLOADS}/{name}"));
    }

    let mut sums = String::new();
    let mut bytes = 0u64;
    for name in &written {
        let path = to.join(name);
        bytes += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        sums.push_str(&format!("{}  {name}\n", digest_of(&path)?));
    }
    let partial = to.join(format!("{SUMS}.partial"));
    std::fs::write(&partial, sums).map_err(|e| format!("cannot write {}: {e}", partial.display()))?;
    std::fs::File::open(&partial)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("cannot write {}: {e}", partial.display()))?;
    std::fs::rename(&partial, to.join(SUMS)).map_err(|e| format!("cannot finish the backup: {e}"))?;
    Ok(format!(
        "Backed up {} files ({:.1} MB) to {}. Check them any time with: sha256sum -c {SUMS}",
        written.len(),
        bytes as f64 / 1_000_000.0,
        to.display()
    ))
}

/// A path named in `SHA256SUMS`, accepted only if it is one a backup
/// writes: a crafted list must not reach outside the folder.
fn allowed(name: &str) -> bool {
    let path = Path::new(name);
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return false;
    }
    match name.split_once('/') {
        None => name == ACCOUNTS || name == MODEL,
        Some((UPLOADS, file)) => {
            !file.is_empty() && !file.contains(['/', '\\']) && !file.starts_with('.') && !file.ends_with(".partial")
        }
        Some(_) => false,
    }
}

/// Reads and checks the list of files, then every file against it.
fn verified(from: &Path) -> Result<Vec<String>, String> {
    let sums = std::fs::read_to_string(from.join(SUMS)).map_err(|_| {
        format!(
            "{} has no {SUMS}: it is not a finished Open Beam backup",
            from.display()
        )
    })?;
    let mut names = Vec::new();
    for line in sums.lines().filter(|l| !l.trim().is_empty()) {
        let Some((digest, name)) = line.split_once("  ") else {
            return Err(format!("{SUMS} has a line that cannot be read"));
        };
        if !allowed(name) {
            return Err(format!("{SUMS} names a file a backup never holds: {name}"));
        }
        if digest_of(&from.join(name))? != digest {
            return Err(format!(
                "{name} has changed since the backup was made; nothing was restored"
            ));
        }
        names.push(name.to_string());
    }
    if !names.iter().any(|n| n == ACCOUNTS) {
        return Err(format!("the backup has no {ACCOUNTS}"));
    }
    Store::check_file(&from.join(ACCOUNTS)).map_err(|e| format!("{ACCOUNTS}: {e}; nothing was restored"))?;
    Ok(names)
}

/// Moves `path` aside to `<path>.before-restore-<stamp>` if it exists.
fn set_aside(path: &Path, stamp: &str) -> Result<Option<PathBuf>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".before-restore-{stamp}"));
    let aside = path.with_file_name(name);
    std::fs::rename(path, &aside).map_err(|e| format!("cannot move {} aside: {e}", path.display()))?;
    Ok(Some(aside))
}

/// Puts `from` in place of `to`: copied beside it first, then swapped.
fn put_in_place(from: &Path, to: &Path, stamp: &str) -> Result<(), String> {
    let mut name = to.file_name().unwrap_or_default().to_os_string();
    name.push(".restoring");
    let incoming = to.with_file_name(name);
    copy_synced(from, &incoming)?;
    set_aside(to, stamp)?;
    std::fs::rename(&incoming, to).map_err(|e| format!("cannot put {} in place: {e}", to.display()))
}

/// Restores the backup in `from` over the files `config` names.
pub fn restore(config: &Config, from: &Path) -> Result<String, String> {
    let names = verified(from)?;
    // A running server would keep using the files it has open.
    if let Err(e) = std::net::TcpListener::bind(config.bind) {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            return Err(format!(
                "something is listening on {}: stop Open Beam before restoring",
                config.bind
            ));
        }
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        .to_string();
    let mut notes = Vec::new();

    // A journal left by a crash would be replayed onto the restored
    // database, so it goes aside with the file it belongs to.
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut name = config.accounts.as_os_str().to_os_string();
        name.push(suffix);
        set_aside(Path::new(&name), &stamp)?;
    }
    put_in_place(&from.join(ACCOUNTS), &config.accounts, &stamp)?;
    notes.push(format!("the account database ({})", config.accounts.display()));

    if names.iter().any(|n| n == MODEL) {
        match &config.ontology {
            Some(model) => {
                put_in_place(&from.join(MODEL), model, &stamp)?;
                notes.push(format!("the model ({})", model.display()));
            }
            None => notes.push(format!(
                "not the model: set ontology in beam.toml, then copy {MODEL} from the backup"
            )),
        }
    }

    let uploads: Vec<&String> = names.iter().filter(|n| n.starts_with("uploads/")).collect();
    if !uploads.is_empty() || config.uploads.exists() {
        set_aside(&config.uploads, &stamp)?;
        std::fs::create_dir_all(&config.uploads)
            .map_err(|e| format!("cannot create {}: {e}", config.uploads.display()))?;
        for name in &uploads {
            let file = name.trim_start_matches("uploads/");
            copy_synced(&from.join(name.as_str()), &config.uploads.join(file))?;
        }
        notes.push(format!(
            "{} uploaded files ({})",
            uploads.len(),
            config.uploads.display()
        ));
    }
    Ok(format!(
        "Restored {}. What was there before is kept beside each, ending in .before-restore-{stamp}.",
        notes.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_accounts::Role;

    const PASSWORD: &str = "a long enough backup passphrase";

    /// A configuration in `dir` with accounts, a model file and one upload.
    fn setup(dir: &Path, port: u16) -> Config {
        std::fs::write(
            dir.join("beam.toml"),
            format!("bind = \"127.0.0.1:{port}\"\nontology = \"model.toml\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("model.toml"), "# the model\n").unwrap();
        let config = Config::load(&dir.join("beam.toml")).unwrap();
        std::fs::create_dir_all(config.uploads.join(".cache")).unwrap();
        std::fs::write(config.uploads.join("u1.csv"), "id\n1\n").unwrap();
        std::fs::write(config.uploads.join(".cache").join("copy.parquet"), "x").unwrap();
        let store = Store::open(&config.accounts).unwrap();
        store.create_first_admin("ada", PASSWORD).unwrap();
        config
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn a_backup_restores_what_was_there_and_keeps_what_it_replaced() {
        let place = tempfile::tempdir().unwrap();
        let config = setup(place.path(), free_port());
        let saved = place.path().join("saved");
        let said = backup(&config, &saved).unwrap();
        assert!(said.contains("3 files"), "{said}");
        let sums = std::fs::read_to_string(saved.join(SUMS)).unwrap();
        assert!(sums.contains("  uploads/u1.csv") && !sums.contains(".cache"), "{sums}");

        // Later: a new account, a changed model, a new upload.
        Store::open(&config.accounts)
            .unwrap()
            .create("bob", PASSWORD, Role::User)
            .unwrap();
        std::fs::write(config.ontology.as_ref().unwrap(), "# changed\n").unwrap();
        std::fs::write(config.uploads.join("u2.csv"), "id\n2\n").unwrap();

        let said = restore(&config, &saved).unwrap();
        assert!(said.contains("before-restore"), "{said}");
        let restored = Store::open(&config.accounts).unwrap();
        assert!(restored.verify("ada", PASSWORD).unwrap().is_some());
        assert!(restored.find("bob").unwrap().is_none(), "bob came after the backup");
        assert_eq!(
            std::fs::read_to_string(config.ontology.as_ref().unwrap()).unwrap(),
            "# the model\n"
        );
        assert!(config.uploads.join("u1.csv").exists());
        assert!(!config.uploads.join("u2.csv").exists());
        // What was replaced is kept beside it.
        let kept: Vec<String> = std::fs::read_dir(place.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".before-restore-"))
            .collect();
        assert_eq!(kept.len(), 3, "{kept:?}");
    }

    #[test]
    fn a_backup_goes_only_into_an_empty_folder() {
        let place = tempfile::tempdir().unwrap();
        let config = setup(place.path(), free_port());
        let full = place.path().join("full");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("x"), "x").unwrap();
        assert!(backup(&config, &full).unwrap_err().contains("not empty"));
    }

    #[test]
    fn a_changed_or_unfinished_backup_changes_nothing() {
        let place = tempfile::tempdir().unwrap();
        let config = setup(place.path(), free_port());
        let saved = place.path().join("saved");
        backup(&config, &saved).unwrap();
        let before = std::fs::read(&config.accounts).unwrap();
        // One changed byte in an upload.
        std::fs::write(saved.join("uploads/u1.csv"), "id\n9\n").unwrap();
        assert!(restore(&config, &saved).unwrap_err().contains("has changed"));
        // No list of files: not finished.
        std::fs::remove_file(saved.join(SUMS)).unwrap();
        assert!(restore(&config, &saved).unwrap_err().contains("not a finished"));
        assert_eq!(std::fs::read(&config.accounts).unwrap(), before);
    }

    #[test]
    fn a_list_cannot_reach_outside_the_backup() {
        for bad in [
            "../beam.toml",
            "/etc/passwd",
            "uploads/../x",
            "uploads/.cache",
            "other.txt",
            "uploads/a/b",
        ] {
            assert!(!allowed(bad), "{bad}");
        }
        for good in ["accounts.sqlite", "model.toml", "uploads/u1.csv"] {
            assert!(allowed(good), "{good}");
        }
        let place = tempfile::tempdir().unwrap();
        let config = setup(place.path(), free_port());
        let saved = place.path().join("saved");
        backup(&config, &saved).unwrap();
        let sums = std::fs::read_to_string(saved.join(SUMS)).unwrap();
        std::fs::write(saved.join(SUMS), format!("{sums}{}  ../beam.toml\n", "0".repeat(64))).unwrap();
        assert!(restore(&config, &saved).unwrap_err().contains("never holds"));
    }

    #[test]
    fn a_damaged_database_is_refused() {
        let place = tempfile::tempdir().unwrap();
        let config = setup(place.path(), free_port());
        let saved = place.path().join("saved");
        backup(&config, &saved).unwrap();
        // Not a database at all, with a matching digest.
        std::fs::write(saved.join(ACCOUNTS), "not a database").unwrap();
        let digest = digest_of(&saved.join(ACCOUNTS)).unwrap();
        let sums = std::fs::read_to_string(saved.join(SUMS)).unwrap();
        let sums: String = sums
            .lines()
            .map(|l| {
                if l.ends_with(ACCOUNTS) {
                    format!("{digest}  {ACCOUNTS}\n")
                } else {
                    format!("{l}\n")
                }
            })
            .collect();
        std::fs::write(saved.join(SUMS), sums).unwrap();
        assert!(restore(&config, &saved).unwrap_err().contains("nothing was restored"));
    }

    #[test]
    fn nothing_is_restored_while_the_server_runs() {
        let place = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let config = setup(place.path(), port);
        let saved = place.path().join("saved");
        // Backing up while it runs is fine.
        backup(&config, &saved).unwrap();
        assert!(restore(&config, &saved).unwrap_err().contains("stop Open Beam"));
        drop(listener);
        assert!(restore(&config, &saved).is_ok());
    }
}
