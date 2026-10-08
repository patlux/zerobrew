use super::*;
use sha2::{Digest, Sha256};
use std::process::Command;
use tempfile::TempDir;
use xz2::write::XzEncoder;

struct Fixture {
    _root: TempDir,
    archive: PathBuf,
    cask: AppCask,
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
<key>CFBundleIdentifier</key><string>dev.zerobrew.fixture</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
<key>CFBundleVersion</key><string>{version}</string>
<key>CFBundleExecutable</key><string>Fixture</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#
            ),
        )
        .unwrap();
        assert!(
            Command::new("/usr/bin/codesign")
                .args(["--sign", "-", "--force"])
                .arg(&app)
                .output()
                .unwrap()
                .status
                .success()
        );
        let archive = root.path().join("fixture.tar.xz");
        let mut builder = tar::Builder::new(XzEncoder::new(File::create(&archive).unwrap(), 1));
        builder.follow_symlinks(false);
        builder.append_dir_all("Fixture.app", &app).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        let sha256 =
            crate::checksum::sha256_hex(Sha256::new_with_prefix(fs::read(&archive).unwrap()));
        let cask = AppCask {
            token: "fixture".into(),
            version: version.into(),
            url: "https://example.org/fixture.tar.xz".into(),
            sha256,
            app: zb_core::cask::AppArtifact {
                name: "Fixture.app".into(),
            },
            has_zap: true,
            auto_updates: true,
        };
        Self {
            _root: root,
            archive,
            cask,
        }
    }
}

#[test]
fn install_repeat_upgrade_uninstall_preserves_unmanaged_files() {
    let parent = tempfile::tempdir().unwrap();
    let appdir = parent.path().join("apps");
    fs::create_dir(&appdir).unwrap();
    let preference = parent.path().join("preferences");
    fs::write(&preference, b"keep").unwrap();
    let a = Fixture::new("1.0");
    let b = Fixture::new("2.0");
    let mut manager = AppManager::open(&appdir).unwrap();
    assert!(manager.install_archive(&a.cask, &a.archive, false).unwrap());
    assert!(!manager.install_archive(&a.cask, &a.archive, false).unwrap());
    assert!(!manager.is_outdated(&a.cask).unwrap());
    assert!(manager.is_outdated(&b.cask).unwrap());
    let mut substituted = a.cask.clone();
    substituted.sha256 = "0".repeat(64);
    assert!(manager.is_outdated(&substituted).is_err());
    assert!(manager.install_archive(&b.cask, &b.archive, false).is_err());
    assert!(manager.install_archive(&b.cask, &b.archive, true).unwrap());
    assert!(!manager.is_outdated(&b.cask).unwrap());
    assert!(manager.is_outdated(&a.cask).is_err());
    assert_eq!(manager.list().unwrap().len(), 1);
    assert_eq!(manager.get("fixture").unwrap().unwrap().cask.version, "2.0");
    assert!(manager.install_archive(&a.cask, &a.archive, true).is_err());
    manager.uninstall("fixture").unwrap();
    assert!(!appdir.join("Fixture.app").exists());
    assert!(manager.list().unwrap().is_empty());
    assert_eq!(fs::read(preference).unwrap(), b"keep");
}

#[test]
fn failures_preserve_old_app_and_receipt() {
    for fault in ["before_publish", "after_publish", "db_write"] {
        let root = tempfile::tempdir().unwrap();
        let a = Fixture::new("1.0");
        let b = Fixture::new("2.0");
        let mut manager = AppManager::open(root.path()).unwrap();
        manager.install_archive(&a.cask, &a.archive, false).unwrap();
        let old = manager.get("fixture").unwrap().unwrap();
        if fault == "db_write" {
            manager.db.execute_batch("CREATE TRIGGER fail_receipt BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT, 'injected db error'); END;").unwrap();
        } else {
            manager.fault = Some(fault);
        }
        assert!(
            manager.install_archive(&b.cask, &b.archive, true).is_err(),
            "{fault}"
        );
        assert_eq!(manager.get("fixture").unwrap().unwrap(), old);
        manager.verify(&old).unwrap();
        let pending: i64 = manager
            .db
            .query_row("SELECT count(*) FROM pending", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pending, 0);
        manager.fault = None;
        if fault == "db_write" {
            manager
                .db
                .execute_batch("DROP TRIGGER fail_receipt;")
                .unwrap();
        }
        let mut bad = b.cask.clone();
        bad.sha256 = "0".repeat(64);
        assert!(manager.install_archive(&bad, &b.archive, true).is_err());
        manager.verify(&old).unwrap();
    }
}

#[test]
fn refuses_foreign_apps_external_modifications_and_concurrent_owner() {
    let root = tempfile::tempdir().unwrap();
    let a = Fixture::new("1.0");
    let mut manager = AppManager::open(root.path()).unwrap();
    assert!(AppManager::open(root.path()).is_err());
    fs::create_dir(root.path().join("Fixture.app")).unwrap();
    assert!(manager.install_archive(&a.cask, &a.archive, false).is_err());
    fs::remove_dir(root.path().join("Fixture.app")).unwrap();
    manager.install_archive(&a.cask, &a.archive, false).unwrap();
    fs::write(root.path().join("Fixture.app/extra"), b"external change").unwrap();
    assert!(manager.install_archive(&a.cask, &a.archive, false).is_err());
    assert!(manager.uninstall("fixture").is_err());
    assert!(manager.is_outdated(&a.cask).is_err());
    assert!(root.path().join("Fixture.app/extra").exists());
}

#[test]
fn open_recovers_interrupted_install_upgrade_and_removal() {
    for action in ["install", "upgrade", "uninstall"] {
        let root = tempfile::tempdir().unwrap();
        let a = Fixture::new("1.0");
        let b = Fixture::new("2.0");
        let mut manager = AppManager::open(root.path()).unwrap();
        if action != "install" {
            manager.install_archive(&a.cask, &a.archive, false).unwrap();
        }
        let old = manager.get("fixture").unwrap();
        let stage = if action == "uninstall" {
            tempfile::Builder::new()
                .prefix(".zb-app-stage-")
                .tempdir_in(&manager.state)
                .unwrap()
        } else {
            stage_app(&b.archive, &manager.state, &b.cask).unwrap()
        };
        let new = if action == "uninstall" {
            None
        } else {
            Some(AppReceipt {
                cask: b.cask.clone(),
                bundle: inspect_bundle(&stage.path().join("Fixture.app")).unwrap(),
                installed_at: 1,
            })
        };
        let pending = Pending {
            old: old.clone(),
            new,
            stage_name: stage.path().file_name().unwrap().to_str().unwrap().into(),
        };
        manager.begin(&pending).unwrap();
        let kept = stage.keep();
        let (target, staged) = manager.paths(&pending).unwrap();
        match action {
            "install" => rename_exclusive(&staged, &target).unwrap(),
            "upgrade" => swap(&staged, &target).unwrap(),
            _ => rename_exclusive(&target, &staged).unwrap(),
        };
        // Drop the lock/connection as a process exit would. No error recovery runs.
        drop(manager);
        let manager = AppManager::open(root.path()).unwrap();
        assert_eq!(manager.get("fixture").unwrap(), old);
        if let Some(old) = old {
            manager.verify(&old).unwrap();
        } else {
            assert!(!target.exists());
        }
        assert!(!kept.exists());
    }
}

#[test]
fn refuses_symlinked_state_or_appdir() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(other.path(), root.path().join("apps")).unwrap();
    assert!(AppManager::open(&root.path().join("apps")).is_err());
    std::os::unix::fs::symlink(other.path(), root.path().join(".zerobrew-apps")).unwrap();
    assert!(AppManager::open(root.path()).is_err());
}

#[test]
fn interrupted_recovery_refuses_external_changes_and_keeps_backup() {
    let root = tempfile::tempdir().unwrap();
    let a = Fixture::new("1.0");
    let b = Fixture::new("2.0");
    let mut manager = AppManager::open(root.path()).unwrap();
    manager.install_archive(&a.cask, &a.archive, false).unwrap();
    let old = manager.get("fixture").unwrap().unwrap();
    let stage = stage_app(&b.archive, &manager.state, &b.cask).unwrap();
    let pending = Pending {
        old: Some(old.clone()),
        new: Some(AppReceipt {
            cask: b.cask.clone(),
            bundle: inspect_bundle(&stage.path().join("Fixture.app")).unwrap(),
            installed_at: 1,
        }),
        stage_name: stage.path().file_name().unwrap().to_str().unwrap().into(),
    };
    let retained = stage.keep();
    manager.begin(&pending).unwrap();
    let (target, staged) = manager.paths(&pending).unwrap();
    swap(&staged, &target).unwrap();
    fs::write(target.join("external"), b"do not remove").unwrap();
    drop(manager);
    assert!(AppManager::open(root.path()).is_err());
    assert_eq!(fs::read(target.join("external")).unwrap(), b"do not remove");
    assert_eq!(fingerprint(&staged).unwrap(), old.bundle.fingerprint);
    assert!(retained.exists());
    // Only remove the synthetic external edit, then recovery can finish.
    fs::remove_file(target.join("external")).unwrap();
    let manager = AppManager::open(root.path()).unwrap();
    manager.verify(&old).unwrap();
}

#[test]
fn filesystem_rename_failure_leaves_receipt_and_old_bundle_usable() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let a = Fixture::new("1.0");
    let b = Fixture::new("2.0");
    let mut manager = AppManager::open(root.path()).unwrap();
    manager.install_archive(&a.cask, &a.archive, false).unwrap();
    let old = manager.get("fixture").unwrap().unwrap();
    // State remains writable, but rename in its parent must fail for this user.
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o500)).unwrap();
    let result = manager.install_archive(&b.cask, &b.archive, true);
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
    manager.verify(&old).unwrap();
    assert_eq!(manager.get("fixture").unwrap().unwrap(), old);
    manager.recover().unwrap();
    assert!(manager.install_archive(&b.cask, &b.archive, true).unwrap());
}

#[test]
fn receipt_and_journal_failures_preserve_every_lifecycle_operation() {
    for action in ["install", "upgrade", "uninstall"] {
        for trigger in [
            "CREATE TRIGGER fail_begin BEFORE INSERT ON pending BEGIN SELECT RAISE(ABORT, 'journal failure'); END;",
            "CREATE TRIGGER fail_commit BEFORE DELETE ON pending BEGIN SELECT RAISE(ABORT, 'commit failure'); END;",
        ] {
            let root = tempfile::tempdir().unwrap();
            let a = Fixture::new("1.0");
            let b = Fixture::new("2.0");
            let mut manager = AppManager::open(root.path()).unwrap();
            if action != "install" {
                manager.install_archive(&a.cask, &a.archive, false).unwrap();
            }
            let old = manager.get("fixture").unwrap();
            manager.db.execute_batch(trigger).unwrap();
            let result = if action == "uninstall" {
                manager.uninstall("fixture")
            } else {
                manager
                    .install_archive(&b.cask, &b.archive, action == "upgrade")
                    .map(|_| ())
            };
            assert!(result.is_err());
            assert_eq!(manager.get("fixture").unwrap(), old);
            if let Some(old) = &old {
                manager.verify(old).unwrap();
            } else {
                assert!(!root.path().join("Fixture.app").exists());
            }
            manager
                .db
                .execute_batch(
                    "DROP TRIGGER IF EXISTS fail_begin; DROP TRIGGER IF EXISTS fail_commit;",
                )
                .unwrap();
            drop(manager);
            let manager = AppManager::open(root.path()).unwrap();
            assert_eq!(manager.get("fixture").unwrap(), old);
        }
    }
}

#[test]
fn malformed_receipts_and_sidecars_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let a = Fixture::new("1.0");
    let mut manager = AppManager::open(root.path()).unwrap();
    manager.install_archive(&a.cask, &a.archive, false).unwrap();
    let old = manager.get("fixture").unwrap().unwrap();
    manager
        .db
        .execute("UPDATE receipts SET app_name='Other.app'", [])
        .unwrap();
    assert!(manager.list().is_err());
    assert!(manager.get("fixture").is_err());
    assert!(manager.uninstall("fixture").is_err());
    manager.verify(&old).unwrap();
    drop(manager);
    let external = root.path().join("external");
    fs::write(&external, b"keep").unwrap();
    std::os::unix::fs::symlink(
        &external,
        root.path().join(".zerobrew-apps/state.sqlite3-journal"),
    )
    .unwrap();
    assert!(AppManager::open(root.path()).is_err());
    assert_eq!(fs::read(external).unwrap(), b"keep");
}

#[test]
#[ignore = "subprocess worker; invoked only by abrupt_exit_and_cross_process_lock"]
fn subprocess_worker() {
    let root = PathBuf::from(std::env::var_os("ZB_APP_TEST_DIRECTORY").unwrap());
    let action = std::env::var("ZB_APP_TEST_ACTION").unwrap();
    if action == "lock" {
        assert!(AppManager::open(&root).is_err());
        return;
    }
    let mut manager = AppManager::open(&root).unwrap();
    manager.fault = Some("exit_after_publish");
    if action == "uninstall" {
        let _ = manager.uninstall("fixture");
    } else {
        let cask: AppCask =
            serde_json::from_str(&std::env::var("ZB_APP_TEST_CASK").unwrap()).unwrap();
        let archive = PathBuf::from(std::env::var_os("ZB_APP_TEST_ARCHIVE").unwrap());
        let _ = manager.install_archive(&cask, &archive, action == "upgrade");
    }
    panic!("child did not exit at the publication boundary");
}

#[test]
fn abrupt_exit_and_cross_process_lock() {
    for action in ["install", "upgrade", "uninstall"] {
        let root = tempfile::tempdir().unwrap();
        let a = Fixture::new("1.0");
        let b = Fixture::new("2.0");
        let mut manager = AppManager::open(root.path()).unwrap();
        if action != "install" {
            manager.install_archive(&a.cask, &a.archive, false).unwrap();
        }
        let old = manager.get("fixture").unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--ignored",
                "--exact",
                "app_cask::state::tests::subprocess_worker",
            ])
            .env("ZB_APP_TEST_DIRECTORY", root.path());
        let locked = child.env("ZB_APP_TEST_ACTION", "lock").output().unwrap();
        assert!(
            locked.status.success(),
            "{}",
            String::from_utf8_lossy(&locked.stderr)
        );
        drop(manager);
        let interrupted = child
            .env("ZB_APP_TEST_ACTION", action)
            .env("ZB_APP_TEST_CASK", serde_json::to_string(&b.cask).unwrap())
            .env("ZB_APP_TEST_ARCHIVE", &b.archive)
            .output()
            .unwrap();
        assert_eq!(
            interrupted.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&interrupted.stdout)
        );
        let manager = AppManager::open(root.path()).unwrap();
        assert_eq!(manager.get("fixture").unwrap(), old);
        if let Some(old) = &old {
            manager.verify(old).unwrap();
        } else {
            assert!(!root.path().join("Fixture.app").exists());
        }
    }
}

#[test]
fn numeric_versions_never_guess_a_downgrade() {
    assert!(newer_version("1.10", "1.9").unwrap());
    assert!(!newer_version("1.0.0", "1").unwrap());
    assert!(!newer_version("1.0", "2").unwrap());
    assert!(newer_version("1.2beta", "1.1").is_err());
}
