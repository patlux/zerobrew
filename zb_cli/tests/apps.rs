//! Network-free macOS CLI acceptance tests. Every artifact, destination and
//! preference sentinel belongs to a fresh temporary directory. No Homebrew calls.
#![cfg(target_os = "macos")]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    root: tempfile::TempDir,
    manifest: PathBuf,
    archive: PathBuf,
}

impl Fixture {
    fn new(version: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let app = root.path().join("Fixture.app");
        fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        let executable = app.join("Contents/MacOS/Fixture");
        fs::write(&executable, format!("#!/bin/sh\necho {version}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            app.join("Contents/Info.plist"),
            format!(
                r#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>dev.zerobrew.cli-fixture</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleVersion</key><string>{version}</string>
<key>CFBundleExecutable</key><string>Fixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#
            ),
        )
        .unwrap();
        success(
            Command::new("/usr/bin/codesign")
                .args(["--sign", "-", "--force"])
                .arg(&app)
                .output()
                .unwrap(),
        );
        let archive = root.path().join("fixture.tar.xz");
        success(
            Command::new("/usr/bin/tar")
                .arg("-cJf")
                .arg(&archive)
                .arg("-C")
                .arg(root.path())
                .arg("Fixture.app")
                .env("COPYFILE_DISABLE", "1")
                .output()
                .unwrap(),
        );
        let sha = checksum(&fs::read(&archive).unwrap());
        let manifest = root.path().join("manifest.json");
        fs::write(&manifest, serde_json::to_vec(&json!({
            "token": "fixture", "version": version, "url": "https://example.invalid/fixture.tar.xz",
            "sha256": sha, "artifacts": [{"app": ["Fixture.app"]}, {"zap": [{"trash": ["~/Library/Preferences/dev.zerobrew.fixture.plist"]}]}],
            "depends_on": {"macos": {">=": ["11"]}}, "variations": {}, "auto_updates": true,
        })).unwrap()).unwrap();
        Self {
            root,
            manifest,
            archive,
        }
    }
    fn change(&self, f: impl FnOnce(&mut Value)) {
        let mut v = serde_json::from_slice(&fs::read(&self.manifest).unwrap()).unwrap();
        f(&mut v);
        fs::write(&self.manifest, serde_json::to_vec(&v).unwrap()).unwrap();
    }
    fn cmd(&self, appdir: &Path, action: &str) -> Output {
        let mut cmd = command(appdir);
        cmd.arg("--app-manifest").arg(&self.manifest);
        if action == "outdated" {
            cmd.args(["outdated", "--json"]);
        } else {
            cmd.arg("--app-archive")
                .arg(&self.archive)
                .args([action, "cask:fixture"]);
        }
        cmd.output().unwrap()
    }
}

fn checksum(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn command(appdir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zb"));
    for key in [
        "ZEROBREW_ROOT",
        "ZEROBREW_PREFIX",
        "ZEROBREW_AUTO_INIT",
        "ZEROBREW_API_URL",
    ] {
        cmd.env_remove(key);
    }
    cmd.arg("--appdir").arg(appdir);
    cmd
}
fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}
fn failure(output: Output) -> String {
    assert!(
        !output.status.success(),
        "unexpected success: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stderr).unwrap()
}
fn info(appdir: &Path) -> Value {
    serde_json::from_str(&success(
        command(appdir)
            .args(["info", "cask:fixture"])
            .output()
            .unwrap(),
    ))
    .unwrap()
}

#[test]
fn full_offline_cli_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let apps = root.path().join("apps");
    fs::create_dir(&apps).unwrap();
    let preferences = root.path().join("preferences");
    fs::write(&preferences, b"keep").unwrap();
    let v1 = Fixture::new("1.0");
    let v2 = Fixture::new("2.0");
    success(v1.cmd(&apps, "install"));
    let receipt = info(&apps);
    assert_eq!(receipt["cask"]["version"], "1.0");
    assert!(success(command(&apps).arg("list").output().unwrap()).contains("cask:fixture 1.0"));
    assert!(success(v1.cmd(&apps, "install")).contains("already installed and unchanged"));
    assert_eq!(info(&apps), receipt);
    assert_eq!(success(v1.cmd(&apps, "outdated")).trim(), "[]");
    let outdated: Value = serde_json::from_str(&success(v2.cmd(&apps, "outdated"))).unwrap();
    assert_eq!(outdated[0]["installed_version"], "1.0");
    assert_eq!(outdated[0]["current_version"], "2.0");
    failure(v2.cmd(&apps, "install"));
    success(v2.cmd(&apps, "upgrade"));
    assert_eq!(info(&apps)["cask"]["version"], "2.0");
    failure(v1.cmd(&apps, "upgrade"));
    assert_eq!(success(v2.cmd(&apps, "outdated")).trim(), "[]");
    success(
        command(&apps)
            .args(["uninstall", "cask:fixture"])
            .output()
            .unwrap(),
    );
    assert!(!apps.join("Fixture.app").exists());
    assert!(success(command(&apps).arg("list").output().unwrap()).is_empty());
    assert_eq!(fs::read(preferences).unwrap(), b"keep");
    assert!(!apps.join("Cellar").exists());
    assert!(!apps.join("store").exists());
}

#[test]
fn unsupported_actions_and_arguments_fail_before_state_creation() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new("1.0");
    fixture.change(|v| {
        v["artifacts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"postflight_steps": ["must not execute"]}))
    });
    assert!(failure(fixture.cmd(root.path(), "install")).contains("unsupported app cask artifact"));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    for args in [
        vec!["reset", "--yes"],
        vec!["uninstall", "--all"],
        vec!["upgrade"],
        vec!["install", "jq"],
        vec!["install", "cask:fixture", "cask:other"],
        vec!["install", "cask:fixture", "--no-link"],
    ] {
        failure(command(root.path()).args(args).output().unwrap());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }
}

#[test]
fn archive_signature_and_download_failures_keep_old_receipt() {
    let root = tempfile::tempdir().unwrap();
    let v1 = Fixture::new("1.0");
    let v2 = Fixture::new("2.0");
    success(v1.cmd(root.path(), "install"));
    let old = info(root.path());
    // Actual downloader failure, without opening a listener or relying on DNS.
    // Deny all network access only for this child; existing app must survive.
    let blocked = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", "(version 1)(allow default)(deny network*)"])
        .arg(env!("CARGO_BIN_EXE_zb"))
        .arg("--appdir")
        .arg(root.path())
        .arg("--app-manifest")
        .arg(&v2.manifest)
        .args(["upgrade", "cask:fixture"])
        .env_remove("ZEROBREW_ROOT")
        .env_remove("ZEROBREW_PREFIX")
        .output()
        .unwrap();
    failure(blocked);
    assert_eq!(info(root.path()), old);
    // Missing local download input.
    fs::rename(&v2.archive, v2.root.path().join("saved.tar.xz")).unwrap();
    failure(v2.cmd(root.path(), "upgrade"));
    assert_eq!(info(root.path()), old);
    // Truncated content with the wrong SHA.
    fs::write(&v2.archive, b"corrupt").unwrap();
    assert!(failure(v2.cmd(root.path(), "upgrade")).contains("SHA-256"));
    assert_eq!(info(root.path()), old);
    // A correctly checksummed malformed archive still fails before publication.
    v2.change(|v| v["sha256"] = json!(checksum(b"corrupt")));
    failure(v2.cmd(root.path(), "upgrade"));
    assert_eq!(info(root.path()), old);
    // Repack a bundle whose signed content has changed.
    fs::write(
        v2.root.path().join("Fixture.app/Contents/MacOS/Fixture"),
        b"#!/bin/sh\nexit 7\n",
    )
    .unwrap();
    success(
        Command::new("/usr/bin/tar")
            .arg("-cJf")
            .arg(&v2.archive)
            .arg("-C")
            .arg(v2.root.path())
            .arg("Fixture.app")
            .env("COPYFILE_DISABLE", "1")
            .output()
            .unwrap(),
    );
    v2.change(|v| v["sha256"] = json!(checksum(&fs::read(&v2.archive).unwrap())));
    assert!(failure(v2.cmd(root.path(), "upgrade")).contains("signature verification failed"));
    assert_eq!(info(root.path()), old);
}

#[test]
fn foreign_and_self_updated_apps_remain_untouched() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new("1.0");
    fs::create_dir(root.path().join("Fixture.app")).unwrap();
    assert!(failure(fixture.cmd(root.path(), "install")).contains("not owned"));
    fs::remove_dir(root.path().join("Fixture.app")).unwrap();
    success(fixture.cmd(root.path(), "install"));
    let external = root.path().join("Fixture.app/external");
    fs::write(&external, b"external self-update").unwrap();
    failure(fixture.cmd(root.path(), "install"));
    failure(fixture.cmd(root.path(), "outdated"));
    failure(
        command(root.path())
            .args(["uninstall", "cask:fixture"])
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read(external).unwrap(), b"external self-update");
}
