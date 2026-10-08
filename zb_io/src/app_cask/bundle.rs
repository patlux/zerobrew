use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;
use zb_core::Error;

use super::invalid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleMetadata {
    pub identifier: String,
    pub version: String,
    pub executable: String,
    pub fingerprint: String,
}

pub(crate) fn validate_tree(bundle: &Path) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(bundle).map_err(Error::file("missing app bundle"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid("app bundle must be a real directory"));
    }
    let root = bundle
        .canonicalize()
        .map_err(Error::file("cannot resolve app bundle"))?;
    for entry in WalkDir::new(bundle).follow_links(false) {
        let entry = entry.map_err(Error::file("cannot inspect app bundle"))?;
        let kind = entry.file_type();
        if kind.is_symlink() {
            let target =
                fs::read_link(entry.path()).map_err(Error::file("cannot read app symlink"))?;
            if target.is_absolute() {
                return Err(invalid("absolute symlink in app bundle"));
            }
            let actual = entry
                .path()
                .canonicalize()
                .map_err(Error::file("dangling or cyclic app symlink"))?;
            if !actual.starts_with(&root) {
                return Err(invalid("symlink escapes app bundle"));
            }
        } else if !kind.is_dir() && !kind.is_file() {
            return Err(invalid("special file in app bundle"));
        }
    }
    Ok(())
}

/// A content/type/mode digest detects external updates without relying on the
/// version string alone. Access times and mutable Finder metadata are excluded.
/// It never follows a symlink or reads outside the verified bundle.
pub fn fingerprint(bundle: &Path) -> Result<String, Error> {
    validate_tree(bundle)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    for entry in WalkDir::new(bundle).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(Error::file("cannot hash app bundle"))?;
        let relative = entry
            .path()
            .strip_prefix(bundle)
            .map_err(Error::file("invalid app path"))?;
        let name = relative
            .to_str()
            .ok_or_else(|| invalid("non-UTF-8 app bundle path"))?;
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        let meta =
            fs::symlink_metadata(entry.path()).map_err(Error::file("cannot stat app bundle"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hash.update((meta.permissions().mode() & 0o7777).to_le_bytes());
        }
        if meta.file_type().is_symlink() {
            hash.update(b"L");
            let target =
                fs::read_link(entry.path()).map_err(Error::file("cannot hash app symlink"))?;
            let target = target
                .to_str()
                .ok_or_else(|| invalid("non-UTF-8 app symlink"))?;
            hash.update((target.len() as u64).to_le_bytes());
            hash.update(target.as_bytes());
        } else if meta.is_dir() {
            hash.update(b"D");
        } else {
            hash.update(b"F");
            hash.update(meta.len().to_le_bytes());
            let mut file = File::open(entry.path()).map_err(Error::file("cannot hash app file"))?;
            loop {
                let n = file
                    .read(&mut buffer)
                    .map_err(Error::file("cannot hash app content"))?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
        }
    }
    Ok(crate::checksum::sha256_hex(hash))
}

/// Validate the vendor's existing signature. Never ad-hoc re-sign the app or
/// remove security attributes to make an invalid artifact appear valid.
#[cfg(target_os = "macos")]
pub fn inspect_bundle(bundle: &Path) -> Result<BundleMetadata, Error> {
    let digest = fingerprint(bundle)?;
    let verification = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(bundle)
        .output()
        .map_err(Error::exec("cannot verify app signature"))?;
    if !verification.status.success() {
        return Err(invalid(format!(
            "app signature verification failed: {}",
            String::from_utf8_lossy(&verification.stderr)
        )));
    }
    let plist = Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(bundle.join("Contents/Info.plist"))
        .output()
        .map_err(Error::exec("cannot read app Info.plist"))?;
    if !plist.status.success() {
        return Err(invalid("app Info.plist is invalid"));
    }
    let value: serde_json::Value = serde_json::from_slice(&plist.stdout)
        .map_err(Error::file("cannot parse app Info.plist"))?;
    let get = |key: &str| -> Result<String, Error> {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
            .map(str::to_owned)
            .ok_or_else(|| invalid(format!("app is missing {key}")))
    };
    let executable = get("CFBundleExecutable")?;
    if executable.contains('/') || executable == "." || executable == ".." {
        return Err(invalid("unsafe CFBundleExecutable"));
    }
    let executable_path = bundle.join("Contents/MacOS").join(&executable);
    if !executable_path.is_file() {
        return Err(invalid("app executable is missing"));
    }
    use std::os::unix::fs::PermissionsExt;
    if fs::metadata(&executable_path)
        .map_err(Error::file("cannot stat app executable"))?
        .permissions()
        .mode()
        & 0o111
        == 0
    {
        return Err(invalid("app executable is not executable"));
    }
    let version = get("CFBundleShortVersionString")?;
    Ok(BundleMetadata {
        identifier: get("CFBundleIdentifier")?,
        version,
        executable,
        fingerprint: digest,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn inspect_bundle(_bundle: &Path) -> Result<BundleMetadata, Error> {
    Err(invalid("standalone app casks require macOS"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_detects_content_and_mode_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("Test.app");
        fs::create_dir(&app).unwrap();
        let file = app.join("file");
        fs::write(&file, b"one").unwrap();
        let before = fingerprint(&app).unwrap();
        assert_eq!(before, fingerprint(&app).unwrap());
        fs::write(&file, b"two").unwrap();
        assert_ne!(before, fingerprint(&app).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let before = fingerprint(&app).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o700)).unwrap();
            assert_ne!(before, fingerprint(&app).unwrap());
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn signed_fixture_is_inspectable_and_tampering_fails() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("Fixture.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        let exe = app.join("Contents/MacOS/Fixture");
        fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            br#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.zerobrew.fixture</string>
<key>CFBundleShortVersionString</key><string>1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>CFBundleExecutable</key><string>Fixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#,
        )
        .unwrap();
        let signed = Command::new("/usr/bin/codesign")
            .args(["--sign", "-", "--force"])
            .arg(&app)
            .output()
            .unwrap();
        assert!(
            signed.status.success(),
            "{}",
            String::from_utf8_lossy(&signed.stderr)
        );
        let inspected = inspect_bundle(&app).unwrap();
        assert_eq!(inspected.identifier, "dev.zerobrew.fixture");
        assert_eq!(inspected.version, "1.0");
        assert_eq!(inspected.fingerprint, fingerprint(&app).unwrap());
        fs::write(&exe, b"#!/bin/sh\nexit 1\n").unwrap();
        assert!(inspect_bundle(&app).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn rejects_symlinked_bundle_and_external_chains() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("Test.app");
        fs::create_dir(&app).unwrap();
        std::os::unix::fs::symlink("..", app.join("escape")).unwrap();
        assert!(fingerprint(&app).is_err());
        std::os::unix::fs::symlink(&app, tmp.path().join("Alias.app")).unwrap();
        assert!(fingerprint(&tmp.path().join("Alias.app")).is_err());
    }
}
