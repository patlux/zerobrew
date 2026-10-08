//! Durable app ownership is scoped to the destination directory, not a formula
//! Cellar. Two zerobrew roots cannot independently own the same app directory.
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use zb_core::{Error, cask::AppCask};

use super::{BundleMetadata, fingerprint, inspect_bundle, invalid, stage_app};

#[cfg(all(test, target_os = "macos"))]
#[path = "state_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppReceipt {
    pub cask: AppCask,
    pub bundle: BundleMetadata,
    pub installed_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Pending {
    old: Option<AppReceipt>,
    new: Option<AppReceipt>,
    stage_name: String,
}

impl Pending {
    fn receipt(&self) -> Result<&AppReceipt, Error> {
        self.new
            .as_ref()
            .or(self.old.as_ref())
            .ok_or_else(|| invalid("empty app transaction"))
    }
}

/// One fail-fast cross-process lock covers inspection, validation, publication,
/// receipt updates and recovery. Keep this object alive throughout an operation.
pub struct AppManager {
    appdir: PathBuf,
    state: PathBuf,
    db: Connection,
    _lock: AppLock,
    #[cfg(test)]
    fault: Option<&'static str>,
}

// Declared after the connection so SQLite closes before the lock is released.
struct AppLock(File);

impl Drop for AppLock {
    fn drop(&mut self) {
        // Closing alone can leave flock held briefly by another thread's
        // forked child until exec closes its inherited descriptor. Explicitly
        // release our lock while the owning object is still alive.
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "cannot release app state lock");
        }
    }
}

impl AppManager {
    pub fn open(appdir: &Path) -> Result<Self, Error> {
        if !cfg!(target_os = "macos") {
            return Err(invalid("app casks require macOS"));
        }
        if !appdir.is_absolute()
            || appdir
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
        {
            return Err(invalid(
                "--appdir must be an absolute directory without '..'",
            ));
        }
        require_directory(appdir)?;
        let appdir = appdir
            .canonicalize()
            .map_err(Error::file("cannot resolve app directory"))?;
        let state = appdir.join(".zerobrew-apps");
        if !state
            .try_exists()
            .map_err(Error::file("cannot inspect app state"))?
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&state)
                    .map_err(Error::file("cannot create app state directory"))?;
            }
        }
        require_directory(&state)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = fs::metadata(&state)
                .map_err(Error::file("cannot inspect app state permissions"))?;
            if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                return Err(invalid(
                    "app state must be owned by this user with mode 0700",
                ));
            }
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW).mode(0o600);
        }
        let lock = options
            .open(state.join("lock"))
            .map_err(Error::file("cannot open app state lock"))?;
        validate_state_file(
            &lock
                .metadata()
                .map_err(Error::file("cannot inspect app lock"))?,
        )?;
        lock.try_lock()
            .map_err(|_| invalid("app directory is busy with another zerobrew operation"))?;
        let dbpath = state.join("state.sqlite3");
        // SQLite also opens sidecars. Refuse redirects and hard links before
        // letting it inspect or recover any file in this private directory.
        for name in [
            "state.sqlite3",
            "state.sqlite3-journal",
            "state.sqlite3-wal",
            "state.sqlite3-shm",
        ] {
            match fs::symlink_metadata(state.join(name)) {
                Ok(meta) => validate_state_file(&meta)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(e) => return Err(invalid(format!("cannot inspect app database file: {e}"))),
            }
        }
        let db = Connection::open(&dbpath).map_err(Error::file("cannot open app database"))?;
        let version: u32 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Error::file("cannot read app database schema"))?;
        if version > 1 {
            return Err(invalid("app database schema is newer than this zerobrew"));
        }
        db.execute_batch("PRAGMA synchronous=FULL; PRAGMA fullfsync=ON;
            CREATE TABLE IF NOT EXISTS receipts (token TEXT PRIMARY KEY, app_name TEXT NOT NULL UNIQUE COLLATE NOCASE, payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS pending (id INTEGER PRIMARY KEY CHECK(id=1), payload TEXT NOT NULL);
            PRAGMA user_version=1;") .map_err(Error::file("cannot initialize app database"))?;
        let mut result = Self {
            appdir,
            state,
            db,
            _lock: AppLock(lock),
            #[cfg(test)]
            fault: None,
        };
        result.recover()?;
        Ok(result)
    }

    pub fn appdir(&self) -> &Path {
        &self.appdir
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.state.join("cache")
    }

    pub fn list(&self) -> Result<Vec<AppReceipt>, Error> {
        let mut statement = self
            .db
            .prepare("SELECT token, app_name, payload FROM receipts ORDER BY token")
            .map_err(Error::file("cannot list app receipts"))?;
        let json = statement
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(Error::file("cannot query app receipts"))?;
        let mut result = Vec::new();
        for row in json {
            let (token, app_name, payload) = row.map_err(Error::file("cannot read app receipt"))?;
            let receipt: AppReceipt =
                serde_json::from_str(&payload).map_err(Error::file("invalid app receipt"))?;
            validate_receipt(&receipt)?;
            if receipt.cask.token != token || receipt.cask.app.name != app_name {
                return Err(invalid("app receipt index mismatch"));
            }
            result.push(receipt);
        }
        Ok(result)
    }

    pub fn get(&self, token: &str) -> Result<Option<AppReceipt>, Error> {
        let raw: Option<(String, String)> = self
            .db
            .query_row(
                "SELECT app_name, payload FROM receipts WHERE token=?1",
                [token],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(Error::file("cannot query app receipt"))?;
        raw.map(|(app_name, raw)| {
            let receipt: AppReceipt =
                serde_json::from_str(&raw).map_err(Error::file("invalid app receipt"))?;
            validate_receipt(&receipt)?;
            if receipt.cask.token != token || receipt.cask.app.name != app_name {
                return Err(invalid("app receipt index mismatch"));
            }
            Ok(receipt)
        })
        .transpose()
    }

    pub fn verify(&self, receipt: &AppReceipt) -> Result<(), Error> {
        validate_receipt(receipt)?;
        let path = self.appdir.join(&receipt.cask.app.name);
        if fingerprint(&path)? != receipt.bundle.fingerprint {
            return Err(invalid(format!(
                "app '{}' changed outside zerobrew; leaving it untouched",
                receipt.cask.token
            )));
        }
        Ok(())
    }

    /// Compare upstream metadata only after verifying the installed bundle.
    pub fn is_outdated(&self, current: &AppCask) -> Result<bool, Error> {
        validate_names(current)?;
        let old = self
            .get(&current.token)?
            .ok_or_else(|| Error::NotInstalled {
                name: format!("cask:{}", current.token),
            })?;
        self.verify(&old)?;
        if old.cask.app.name != current.app.name {
            return Err(invalid("upstream app name changed"));
        }
        if old.cask.version == current.version {
            if old.cask.sha256 != current.sha256 {
                return Err(invalid(
                    "upstream changed an app archive without a new version",
                ));
            }
            return Ok(false);
        }
        if !newer_version(&current.version, &old.cask.version)? {
            return Err(invalid(
                "installed app is newer than upstream; no downgrade permitted",
            ));
        }
        Ok(true)
    }

    /// Re-install is a no-op only when the complete owned bundle is unchanged.
    /// A different upstream version needs explicit upgrade. Downgrades and
    /// same-version artifact substitutions fail closed.
    pub fn needs_install(&self, cask: &AppCask, upgrade: bool) -> Result<bool, Error> {
        validate_names(cask)?;
        if let Some(old) = self.get(&cask.token)? {
            self.verify(&old)?;
            if old.cask.app.name != cask.app.name {
                return Err(invalid("upstream app name changed"));
            }
            if old.cask.version == cask.version && old.cask.sha256 == cask.sha256 {
                return Ok(false);
            }
            if !upgrade {
                return Err(invalid("app already installed; use explicit upgrade"));
            }
            if !newer_version(&cask.version, &old.cask.version)? {
                return Err(invalid("app downgrade or same-version replacement refused"));
            }
        } else {
            if upgrade {
                return Err(Error::NotInstalled {
                    name: format!("cask:{}", cask.token),
                });
            }
            if fs::symlink_metadata(self.appdir.join(&cask.app.name)).is_ok() {
                return Err(invalid(
                    "app destination already exists and is not owned by zerobrew",
                ));
            }
            if self
                .list()?
                .iter()
                .any(|r| r.cask.app.name.eq_ignore_ascii_case(&cask.app.name))
            {
                return Err(invalid("app destination is already owned by another cask"));
            }
        }
        Ok(true)
    }

    pub fn install_archive(
        &mut self,
        cask: &AppCask,
        archive: &Path,
        upgrade: bool,
    ) -> Result<bool, Error> {
        // The public API accepts a resolved cask, but still validates its path
        // fields rather than trusting callers to construct only safe values.
        validate_names(cask)?;
        if !self.needs_install(cask, upgrade)? {
            return Ok(false);
        }
        let old = self.get(&cask.token)?;
        let stage = stage_app(archive, &self.state, cask)?;
        let bundle = inspect_bundle(&stage.path().join(&cask.app.name))?;
        if bundle.version != cask.version {
            return Err(invalid("app bundle version does not match cask metadata"));
        }
        if let Some(ref old) = old {
            if old.bundle.identifier != bundle.identifier {
                return Err(invalid("app bundle identifier changed"));
            }
            self.verify(old)?;
        }
        let new = AppReceipt {
            cask: cask.clone(),
            bundle,
            installed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(Error::file("invalid system clock"))?
                .as_secs(),
        };
        let pending = Pending {
            old,
            new: Some(new),
            stage_name: stage
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        };
        sync_tree(stage.path())?;
        // Retain before the durable journal write. An ambiguous database/sync
        // failure must not drop a stage which the journal may already reference.
        let _retained = stage.keep();
        self.begin(&pending)?;
        self.mutate_and_commit(&pending)?;
        Ok(true)
    }

    pub fn uninstall(&mut self, token: &str) -> Result<(), Error> {
        let old = self.get(token)?.ok_or_else(|| Error::NotInstalled {
            name: format!("cask:{token}"),
        })?;
        self.verify(&old)?;
        let stage = tempfile::Builder::new()
            .prefix(".zb-app-stage-")
            .tempdir_in(&self.state)
            .map_err(Error::file("cannot stage app removal"))?;
        let pending = Pending {
            old: Some(old),
            new: None,
            stage_name: stage
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        };
        let _retained = stage.keep();
        self.begin(&pending)?;
        self.mutate_and_commit(&pending)
    }

    fn begin(&self, pending: &Pending) -> Result<(), Error> {
        let json =
            serde_json::to_string(pending).map_err(Error::file("cannot encode app transaction"))?;
        self.db
            .execute("INSERT INTO pending (id,payload) VALUES (1,?1)", [json])
            .map_err(Error::file("cannot journal app operation"))?;
        sync_dir(&self.state)
    }

    fn mutate_and_commit(&mut self, pending: &Pending) -> Result<(), Error> {
        let result = self.publish(pending);
        if result.is_err()
            && let Err(recovery) = self.recover()
        {
            return Err(invalid(format!(
                "app operation failed; recovery needs attention: {recovery}"
            )));
        }
        result
    }

    fn publish(&mut self, pending: &Pending) -> Result<(), Error> {
        let (target, staged) = self.paths(pending)?;
        self.inject("before_publish")?;
        match (&pending.old, &pending.new) {
            (None, Some(_)) => rename_exclusive(&staged, &target)?,
            (Some(old), Some(_)) => {
                self.verify(old)?;
                swap(&staged, &target)?;
            }
            (Some(old), None) => {
                self.verify(old)?;
                rename_exclusive(&target, &staged)?;
            }
            _ => return Err(invalid("invalid app transaction")),
        }
        sync_dir(&self.appdir)?;
        sync_dir(staged.parent().unwrap())?;
        self.inject("after_publish")?;
        let tx = self
            .db
            .transaction()
            .map_err(Error::file("cannot commit app receipt"))?;
        if let Some(new) = &pending.new {
            let payload =
                serde_json::to_string(new).map_err(Error::file("cannot encode app receipt"))?;
            tx.execute("INSERT INTO receipts (token,app_name,payload) VALUES (?1,?2,?3) ON CONFLICT(token) DO UPDATE SET app_name=excluded.app_name,payload=excluded.payload",params![new.cask.token,new.cask.app.name,payload]).map_err(Error::file("cannot write app receipt"))?;
        } else {
            tx.execute(
                "DELETE FROM receipts WHERE token=?1",
                [&pending.receipt()?.cask.token],
            )
            .map_err(Error::file("cannot remove app receipt"))?;
        }
        tx.execute("DELETE FROM pending WHERE id=1", [])
            .map_err(Error::file("cannot finish app transaction"))?;
        tx.commit()
            .map_err(Error::file("cannot commit app transaction"))?;
        // Cleanup is after commit. Failure must not roll back a successful app.
        if let Err(e) = fs::remove_dir_all(staged.parent().unwrap()) {
            tracing::warn!(error=%e,"committed app transaction left a backup staging directory");
        }
        Ok(())
    }

    fn paths(&self, pending: &Pending) -> Result<(PathBuf, PathBuf), Error> {
        let receipt = pending.receipt()?;
        validate_receipt(receipt)?;
        if let Some(old) = &pending.old {
            validate_receipt(old)?;
            if old.cask.app.name != receipt.cask.app.name || old.cask.token != receipt.cask.token {
                return Err(invalid("inconsistent app journal"));
            }
        }
        if !pending.stage_name.starts_with(".zb-app-stage-") || !safe_component(&pending.stage_name)
        {
            return Err(invalid("invalid app transaction directory"));
        }
        let directory = self.state.join(&pending.stage_name);
        require_directory(&directory)?;
        Ok((
            self.appdir.join(&receipt.cask.app.name),
            directory.join(&receipt.cask.app.name),
        ))
    }

    /// An uncommitted journal always restores the old app. Content checks avoid
    /// deleting or overwriting an app which another program changed meanwhile.
    pub fn recover(&mut self) -> Result<(), Error> {
        let json: Option<String> = self
            .db
            .query_row("SELECT payload FROM pending WHERE id=1", [], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Error::file("cannot read app journal"))?;
        let Some(json) = json else {
            return Ok(());
        };
        let pending: Pending =
            serde_json::from_str(&json).map_err(Error::file("invalid app journal"))?;
        if self.get(&pending.receipt()?.cask.token)? != pending.old {
            return Err(invalid("app journal does not match the committed receipt"));
        }
        let (target, staged) = self.paths(&pending)?;
        let current = optional_fingerprint(&target)?;
        let staged_hash = optional_fingerprint(&staged)?;
        let old_hash = pending.old.as_ref().map(|r| r.bundle.fingerprint.as_str());
        let new_hash = pending.new.as_ref().map(|r| r.bundle.fingerprint.as_str());
        match (&pending.old, &pending.new) {
            (None, Some(_)) if current.as_deref() == new_hash && staged_hash.is_none() => {
                rename_exclusive(&target, &staged)?
            }
            (None, Some(_)) if current.is_none() && staged_hash.as_deref() == new_hash => (),
            (Some(_), Some(_))
                if current.as_deref() == new_hash && staged_hash.as_deref() == old_hash =>
            {
                swap(&staged, &target)?
            }
            (Some(_), Some(_))
                if current.as_deref() == old_hash && staged_hash.as_deref() == new_hash => {}
            (Some(_), None) if current.is_none() && staged_hash.as_deref() == old_hash => {
                rename_exclusive(&staged, &target)?
            }
            (Some(_), None) if current.as_deref() == old_hash && staged_hash.is_none() => (),
            _ => {
                return Err(invalid(
                    "app transaction paths changed externally; recovery leaves them untouched",
                ));
            }
        }
        sync_dir(&self.appdir)?;
        sync_dir(staged.parent().unwrap())?;
        self.db
            .execute("DELETE FROM pending WHERE id=1", [])
            .map_err(Error::file("cannot finish app recovery"))?;
        fs::remove_dir_all(staged.parent().unwrap())
            .map_err(Error::file("cannot clean recovered app staging"))?;
        Ok(())
    }

    fn inject(&self, point: &str) -> Result<(), Error> {
        #[cfg(test)]
        {
            if self.fault == Some("exit_after_publish") && point == "after_publish" {
                // Only in the test binary: terminate without dropping the
                // connection, lock or staging objects, as an abrupt exit would.
                std::process::exit(73);
            }
            if self.fault == Some(point) {
                return Err(invalid(format!("injected fault: {point}")));
            }
        }
        let _ = point;
        Ok(())
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\'))
}
fn validate_names(cask: &AppCask) -> Result<(), Error> {
    if !safe_component(&cask.token)
        || !cask
            .token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'@' | b'.'))
        || !safe_component(&cask.app.name)
        || cask.app.name == ".app"
        || !cask.app.name.ends_with(".app")
    {
        return Err(invalid("unsafe app receipt path"));
    }
    Ok(())
}
fn validate_receipt(receipt: &AppReceipt) -> Result<(), Error> {
    validate_names(&receipt.cask)?;
    if receipt.cask.version != receipt.bundle.version
        || receipt.bundle.identifier.is_empty()
        || !safe_component(&receipt.bundle.executable)
        || receipt.cask.sha256.len() != 64
        || !receipt.cask.sha256.bytes().all(|b| b.is_ascii_hexdigit())
        || receipt.bundle.fingerprint.len() != 64
        || !receipt
            .bundle
            .fingerprint
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("invalid app receipt fingerprint"));
    }
    Ok(())
}
fn validate_state_file(meta: &fs::Metadata) -> Result<(), Error> {
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(invalid("app state files must be regular files"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() != 1 || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0
        {
            return Err(invalid(
                "app state files must be exclusively owned and not group/world writable",
            ));
        }
    }
    Ok(())
}
fn require_directory(path: &Path) -> Result<(), Error> {
    let meta =
        fs::symlink_metadata(path).map_err(Error::file("app directory must already exist"))?;
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(invalid("app/state directory must not be a symlink"));
    }
    Ok(())
}
fn optional_fingerprint(path: &Path) -> Result<Option<String>, Error> {
    match fs::symlink_metadata(path) {
        Ok(_) => fingerprint(path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(invalid(format!("cannot inspect transaction path: {e}"))),
    }
}
fn sync_dir(path: &Path) -> Result<(), Error> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(Error::file("cannot sync app directory"))
}
fn sync_tree(path: &Path) -> Result<(), Error> {
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .contents_first(true)
    {
        let entry = entry.map_err(Error::file("cannot walk staged app"))?;
        if !entry.file_type().is_symlink() {
            File::open(entry.path())
                .and_then(|f| f.sync_all())
                .map_err(Error::file("cannot sync staged app"))?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn rename_with_flags(from: &Path, to: &Path, flags: u32) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;
    let from = std::ffi::CString::new(from.as_os_str().as_bytes())
        .map_err(Error::file("invalid app path"))?;
    let to = std::ffi::CString::new(to.as_os_str().as_bytes())
        .map_err(Error::file("invalid app path"))?;
    if unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), flags) } != 0 {
        return Err(invalid(format!(
            "atomic app rename failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn rename_with_flags(_from: &Path, _to: &Path, _flags: u32) -> Result<(), Error> {
    Err(invalid("atomic app operations require macOS"))
}
fn rename_exclusive(from: &Path, to: &Path) -> Result<(), Error> {
    rename_with_flags(from, to, 4 /* RENAME_EXCL */)
}
fn swap(from: &Path, to: &Path) -> Result<(), Error> {
    rename_with_flags(from, to, 2 /* RENAME_SWAP */)
}

/// The initial pilot accepts numeric versions only for upgrades. Unknown vendor
/// version syntax is an explicit error, never a guessed downgrade decision.
pub fn newer_version(new: &str, old: &str) -> Result<bool, Error> {
    fn parse(v: &str) -> Result<Vec<u64>, Error> {
        let mut parts: Vec<u64> = v
            .split('.')
            .map(|p| {
                if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid("app upgrade requires comparable numeric versions"));
                }
                p.parse().map_err(Error::file("app version overflow"))
            })
            .collect::<Result<_, _>>()?;
        while parts.last() == Some(&0) {
            parts.pop();
        }
        Ok(parts)
    }
    Ok(parse(new)? > parse(old)?)
}
