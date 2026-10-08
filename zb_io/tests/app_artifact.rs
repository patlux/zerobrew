//! Explicit, network-free verification of a caller-supplied real artifact.
//! This test never starts the app or touches the system Applications directory.
#[test]
#[ignore = "requires ZB_IMAGEOPTIM_ARCHIVE with the pinned vendor tar.xz"]
#[cfg(target_os = "macos")]
fn imageoptim_archive_preserves_vendor_signature() {
    let archive = std::path::PathBuf::from(
        std::env::var_os("ZB_IMAGEOPTIM_ARCHIVE").expect("ZB_IMAGEOPTIM_ARCHIVE is required"),
    );
    let metadata =
        serde_json::from_str(include_str!("../../zb_core/fixtures/cask_imageoptim.json")).unwrap();
    let cask = zb_core::cask::resolve_app_cask(
        "imageoptim",
        &metadata,
        zb_core::cask::MacPlatform {
            arch: std::env::consts::ARCH,
            version: "27.0",
        },
    )
    .unwrap();
    let parent = tempfile::tempdir().unwrap();
    let stage = zb_io::app_cask::stage_app(&archive, parent.path(), &cask).unwrap();
    let bundle = stage.path().join("ImageOptim.app");
    let info = zb_io::app_cask::inspect_bundle(&bundle).unwrap();
    assert_eq!(info.identifier, "net.pornel.ImageOptim");
    assert_eq!(info.version, "1.9.3");
    let signature = std::process::Command::new("/usr/bin/codesign")
        .args(["-dv", "--verbose=2"])
        .arg(&bundle)
        .output()
        .unwrap();
    assert!(signature.status.success());
    let signature = String::from_utf8_lossy(&signature.stderr);
    assert!(signature.contains("TeamIdentifier=59KZTZA4XR"));
    assert!(signature.contains("Authority=Developer ID Application: Kornel Lesinski"));
    assert!(
        bundle
            .join("Contents/Frameworks/Sparkle.framework/Versions/Current")
            .is_symlink()
    );
    assert_eq!(
        info.fingerprint,
        zb_io::app_cask::fingerprint(&bundle).unwrap()
    );
}
