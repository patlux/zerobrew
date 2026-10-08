use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use tar::Archive;
use tempfile::TempDir;
use xz2::read::XzDecoder;
use zb_core::{Error, cask::AppCask};

use super::invalid;

const MAX_ENTRIES: usize = 100_000;
const MAX_UNPACKED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Check the entire archive before extraction, then extract only into a fresh
/// private directory on the app volume. The caller owns the returned staging
/// directory until it atomically publishes the app. Hard links, special files,
/// duplicate/case-colliding paths and members below symlinks are not supported.
pub fn stage_app(
    archive_path: &Path,
    staging_parent: &Path,
    cask: &AppCask,
) -> Result<TempDir, Error> {
    let mut file = File::open(archive_path).map_err(Error::file("cannot open app archive"))?;
    let mut hash = Sha256::new();
    let mut buf = [0; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(Error::file("cannot read app archive"))?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    let actual = crate::checksum::sha256_hex(hash);
    if actual != cask.sha256 {
        return Err(invalid("app archive SHA-256 mismatch"));
    }
    file.rewind()
        .map_err(Error::file("cannot rewind app archive"))?;
    validate_archive(&file, &cask.app.name)?;
    file.seek(SeekFrom::Start(0))
        .map_err(Error::file("cannot rewind app archive"))?;

    let stage = tempfile::Builder::new()
        .prefix(".zb-app-stage-")
        .tempdir_in(staging_parent)
        .map_err(Error::file("cannot create private app staging directory"))?;
    let mut archive = Archive::new(XzDecoder::new(&file));
    archive.set_preserve_permissions(true);
    archive.set_unpack_xattrs(true);
    for entry in archive
        .entries()
        .map_err(Error::file("cannot read app archive entries"))?
    {
        let mut entry = entry.map_err(Error::file("cannot read app archive entry"))?;
        if !entry
            .unpack_in(stage.path())
            .map_err(Error::file("cannot unpack app archive entry"))?
        {
            return Err(invalid("app archive entry escaped staging directory"));
        }
    }
    // Resolve complete symlink chains after extraction, not just their lexical
    // targets. A chain must stay within this bundle and cannot be dangling.
    let bundle = stage.path().join(&cask.app.name);
    super::bundle::validate_tree(&bundle)?;
    Ok(stage)
}

fn normalized(path: &Path) -> Result<PathBuf, Error> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => (),
            Component::ParentDir => {
                if !result.pop() {
                    return Err(invalid("app symlink escapes archive"));
                }
            }
            _ => return Err(invalid("absolute path in app archive")),
        }
    }
    Ok(result)
}

fn validate_archive(file: &File, app_name: &str) -> Result<(), Error> {
    let mut archive = Archive::new(XzDecoder::new(file));
    let mut paths = HashSet::new();
    let mut symlinks = HashSet::new();
    let mut total = 0u64;
    for entry in archive
        .entries()
        .map_err(Error::file("cannot read app archive"))?
    {
        let entry = entry.map_err(Error::file("cannot read app member"))?;
        let path = entry
            .path()
            .map_err(Error::file("invalid app member path"))?
            .into_owned();
        if path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            return Err(invalid("unsafe app archive member path"));
        }
        let path = normalized(&path)?;
        if !path.starts_with(app_name) {
            return Err(invalid(
                "app archive contains files outside the declared bundle",
            ));
        }
        let text = path
            .to_str()
            .ok_or_else(|| invalid("non-UTF-8 app archive path"))?;
        if text.chars().any(char::is_control) {
            return Err(invalid("control character in app archive path"));
        }
        if !paths.insert(text.to_lowercase()) {
            return Err(invalid("duplicate or case-colliding app archive path"));
        }
        if paths.len() > MAX_ENTRIES {
            return Err(invalid("too many app archive entries"));
        }
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or_else(|| invalid("app archive size overflow"))?;
        if total > MAX_UNPACKED_BYTES {
            return Err(invalid("app archive exceeds unpacked size limit"));
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() && !kind.is_symlink() {
            return Err(invalid(
                "unsupported special file or hard link in app archive",
            ));
        }
        if entry
            .header()
            .mode()
            .map_err(Error::file("invalid app member mode"))?
            & 0o6000
            != 0
        {
            return Err(invalid("setuid/setgid app archive member"));
        }
        if kind.is_symlink() {
            let target = entry
                .link_name()
                .map_err(Error::file("invalid app symlink"))?
                .ok_or_else(|| invalid("app symlink has no target"))?;
            if target.is_absolute() {
                return Err(invalid("absolute app symlink target"));
            }
            let resolved = normalized(&path.parent().unwrap_or(Path::new("")).join(target))?;
            if !resolved.starts_with(app_name) || path == Path::new(app_name) {
                return Err(invalid("app symlink escapes bundle"));
            }
            symlinks.insert(text.to_lowercase());
        }
    }
    if paths.is_empty() {
        return Err(invalid("empty app archive"));
    }
    for path in &paths {
        for ancestor in Path::new(path).ancestors().skip(1) {
            if symlinks.contains(&ancestor.to_string_lossy().to_string()) {
                return Err(invalid("archive member is nested under a symlink"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tar::{Builder, EntryType, Header};
    use xz2::write::XzEncoder;

    type Member<'a> = (&'a str, Option<&'a str>, &'a [u8]);

    fn fixture(entries: &[Member<'_>]) -> (TempDir, PathBuf, AppCask) {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("test.tar.xz");
        let mut tar = Builder::new(XzEncoder::new(File::create(&archive).unwrap(), 1));
        for (path, link, data) in entries {
            let mut header = Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_mode(0o755);
            if let Some(target) = link {
                header.set_entry_type(EntryType::Symlink);
                header.set_link_name(target).unwrap();
                header.set_size(0);
            } else {
                header.set_size(data.len() as u64);
            }
            header.set_cksum();
            tar.append(&header, *data).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap();
        let mut cask: AppCask = zb_core::cask::resolve_app_cask(
            "imageoptim",
            &serde_json::from_str(include_str!(
                "../../../zb_core/fixtures/cask_imageoptim.json"
            ))
            .unwrap(),
            zb_core::cask::MacPlatform {
                arch: "aarch64",
                version: "26",
            },
        )
        .unwrap();
        cask.app.name = "Test.app".into();
        cask.sha256 =
            crate::checksum::sha256_hex(Sha256::new_with_prefix(std::fs::read(&archive).unwrap()));
        (tmp, archive, cask)
    }

    #[test]
    fn preserves_internal_framework_symlinks() {
        let (tmp, path, cask) = fixture(&[
            ("Test.app/Contents/Versions/A/tool", None, b"payload"),
            ("Test.app/Contents/Versions/Current", Some("A"), b""),
        ]);
        let stage = stage_app(&path, tmp.path(), &cask).unwrap();
        assert_eq!(
            std::fs::read_link(stage.path().join("Test.app/Contents/Versions/Current")).unwrap(),
            Path::new("A")
        );
        assert_eq!(
            std::fs::read(stage.path().join("Test.app/Contents/Versions/Current/tool")).unwrap(),
            b"payload"
        );
    }

    #[test]
    fn rejects_checksum_before_creating_staging() {
        let (tmp, path, mut cask) = fixture(&[("Test.app/tool", None, b"hello")]);
        cask.sha256 = "0".repeat(64);
        assert!(stage_app(&path, tmp.path(), &cask).is_err());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejects_malformed_special_and_traversing_members_without_staging() {
        for (name, kind) in [
            ("../escape", EntryType::Regular),
            ("/tmp/escape", EntryType::Regular),
            ("Test.app/hard", EntryType::Link),
            ("Test.app/pipe", EntryType::Fifo),
        ] {
            let (tmp, path, mut cask) = fixture(&[("Test.app/file", None, b"x")]);
            let mut header = Header::new_gnu();
            header.set_mode(0o755);
            header.set_size(0);
            header.set_entry_type(kind);
            // Deliberately bypass tar's safe path setter to build hostile input.
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            if kind.is_hard_link() {
                header.set_link_name("Test.app/file").unwrap();
            }
            header.set_cksum();
            let mut builder = Builder::new(XzEncoder::new(File::create(&path).unwrap(), 1));
            builder.append(&header, std::io::empty()).unwrap();
            builder.into_inner().unwrap().finish().unwrap();
            cask.sha256 =
                crate::checksum::sha256_hex(Sha256::new_with_prefix(std::fs::read(&path).unwrap()));
            assert!(stage_app(&path, tmp.path(), &cask).is_err());
            assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
        }
        let (tmp, path, mut cask) = fixture(&[("Test.app/file", None, b"x")]);
        std::fs::write(&path, b"not an xz archive").unwrap();
        cask.sha256 =
            crate::checksum::sha256_hex(Sha256::new_with_prefix(std::fs::read(&path).unwrap()));
        assert!(stage_app(&path, tmp.path(), &cask).is_err());
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejects_symlink_escapes_nested_writes_duplicates_and_dangling_links() {
        let cases: Vec<Vec<Member<'_>>> = vec![
            vec![("Test.app/link", Some("../../outside"), b"")],
            vec![("Test.app/link", Some("/tmp"), b"")],
            vec![
                ("Test.app/link", Some("target"), b""),
                ("Test.app/link/file", None, b"bad"),
            ],
            vec![("Test.app/file", None, b"a"), ("Test.app/FILE", None, b"b")],
            vec![("Other.app/file", None, b"bad")],
            vec![("Test.app/link", Some("absent"), b"")],
        ];
        for entries in cases {
            let (tmp, path, cask) = fixture(&entries);
            assert!(
                stage_app(&path, tmp.path(), &cask).is_err(),
                "accepted {entries:?}"
            );
            assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 1);
        }
    }
}
