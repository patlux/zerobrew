//! Explicit app mode never enters formula initialization, Cellar or reset paths.
use std::path::Path;

use zb_core::{
    Error,
    cask::{AppCask, MacPlatform, resolve_app_cask},
};
use zb_io::{ApiClient, BlobCache, DownloadRequest, ParallelDownloader, app_cask::AppManager};

use crate::cli::Commands;

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgument {
        message: message.into(),
    }
}

fn token(name: &str) -> Result<&str, Error> {
    let token = name
        .strip_prefix("cask:")
        .ok_or_else(|| invalid("--appdir accepts only explicit cask:<token> names"))?;
    if token.is_empty()
        || matches!(token, "." | "..")
        || !token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'@' | b'.'))
    {
        return Err(invalid("invalid standalone app cask token"));
    }
    Ok(token)
}

fn macos_version() -> Result<String, Error> {
    let output = std::process::Command::new("/usr/bin/sw_vers")
        .arg("-productVersion")
        .output()
        .map_err(Error::exec("cannot detect macOS version"))?;
    if !output.status.success() {
        return Err(invalid("cannot detect macOS version"));
    }
    String::from_utf8(output.stdout)
        .map(|s| s.trim().to_owned())
        .map_err(Error::file("invalid macOS version"))
}

/// An explicit raw Homebrew JSON manifest supports a reproducible offline pilot.
/// It uses exactly the same fail-closed resolver as the live API; no bypass flags.
async fn resolve(name: &str, manifest: Option<&Path>) -> Result<AppCask, Error> {
    let name = token(name)?;
    let metadata = match manifest {
        Some(path) => serde_json::from_slice(
            &std::fs::read(path).map_err(Error::file("cannot read app manifest"))?,
        )
        .map_err(Error::file("invalid app manifest"))?,
        None => ApiClient::new().get_cask(name).await?,
    };
    resolve_app_cask(
        name,
        &metadata,
        MacPlatform {
            arch: std::env::consts::ARCH,
            version: &macos_version()?,
        },
    )
}

pub async fn execute(
    appdir: &Path,
    command: Commands,
    manifest: Option<&Path>,
    archive: Option<&Path>,
    quiet: bool,
) -> Result<(), Error> {
    if !cfg!(target_os = "macos") {
        return Err(invalid("--appdir requires macOS"));
    }
    if archive.is_some()
        && (manifest.is_none()
            || !matches!(
                &command,
                Commands::Install { .. } | Commands::Upgrade { .. }
            ))
    {
        return Err(invalid(
            "--app-archive requires --app-manifest and install or upgrade",
        ));
    }
    if manifest.is_some()
        && !matches!(
            &command,
            Commands::Install { .. } | Commands::Upgrade { .. } | Commands::Outdated { .. }
        )
    {
        return Err(invalid(
            "--app-manifest supports install, upgrade and outdated only",
        ));
    }
    match command {
        Commands::Install {
            formulas,
            no_link,
            build_from_source,
        } => {
            reject_install_flags(no_link, build_from_source)?;
            install(appdir, formulas, false, manifest, archive, quiet).await
        }
        Commands::Upgrade {
            formulas,
            no_link,
            build_from_source,
        } => {
            reject_install_flags(no_link, build_from_source)?;
            install(appdir, formulas, true, manifest, archive, quiet).await
        }
        Commands::Uninstall { formulas, all } => {
            if all || formulas.len() != 1 {
                return Err(invalid(
                    "app uninstall requires exactly one explicit cask name; --all is not supported",
                ));
            }
            let token = token(&formulas[0])?;
            AppManager::open(appdir)?.uninstall(token)?;
            if !quiet {
                println!("Removed cask:{token}; preferences and zap paths were not touched.");
            }
            Ok(())
        }
        Commands::List => {
            let manager = AppManager::open(appdir)?;
            for receipt in manager.list()? {
                manager.verify(&receipt)?;
                println!(
                    "cask:{} {} {}",
                    receipt.cask.token,
                    receipt.cask.version,
                    manager.appdir().join(&receipt.cask.app.name).display()
                );
            }
            Ok(())
        }
        Commands::Info { formula } => {
            let name = token(&formula)?;
            let manager = AppManager::open(appdir)?;
            let receipt = manager.get(name)?.ok_or_else(|| Error::NotInstalled {
                name: formula.clone(),
            })?;
            manager.verify(&receipt)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&receipt)
                    .map_err(Error::file("cannot encode app info"))?
            );
            Ok(())
        }
        Commands::Outdated { json } => {
            let manager = AppManager::open(appdir)?;
            let mut result = Vec::new();
            for old in manager.list()? {
                manager.verify(&old)?;
                let name = format!("cask:{}", old.cask.token);
                let current = resolve(&name, manifest).await?;
                if manager.is_outdated(&current)? {
                    result.push((name, old.cask.version, current.version));
                }
            }
            if json {
                let rows: Vec<_> = result.iter().map(|(name, installed, current)| serde_json::json!({
                    "name": name, "installed_version": installed, "current_version": current,
                })).collect();
                println!(
                    "{}",
                    serde_json::to_string(&rows)
                        .map_err(Error::file("cannot encode outdated apps"))?
                );
            } else {
                for (name, installed, current) in result {
                    if quiet {
                        println!("{name}");
                    } else {
                        println!("{name} {installed} -> {current}");
                    }
                }
            }
            Ok(())
        }
        _ => Err(invalid(
            "app pilot supports install, upgrade, uninstall, list, info and outdated only; no reset, migration or zap",
        )),
    }
}

fn reject_install_flags(no_link: bool, build_from_source: bool) -> Result<(), Error> {
    if no_link || build_from_source {
        return Err(invalid(
            "app installs do not support --no-link or source builds",
        ));
    }
    Ok(())
}

async fn install(
    appdir: &Path,
    names: Vec<String>,
    upgrade: bool,
    manifest: Option<&Path>,
    archive: Option<&Path>,
    quiet: bool,
) -> Result<(), Error> {
    // One app per transaction: no partial batch success, hidden dependencies or
    // formula work before a later unsupported artifact is discovered.
    if names.len() != 1 {
        return Err(invalid(
            "app pilot installs/upgrades exactly one explicit cask at a time",
        ));
    }
    let cask = resolve(&names[0], manifest).await?;
    let mut manager = AppManager::open(appdir)?;
    if !manager.needs_install(&cask, upgrade)? {
        if !quiet {
            println!(
                "cask:{} {} is already installed and unchanged",
                cask.token, cask.version
            );
        }
        return Ok(());
    }
    let path = if let Some(path) = archive {
        path.to_path_buf()
    } else {
        let downloader = ParallelDownloader::new(
            BlobCache::new(&manager.cache_dir())
                .map_err(Error::file("cannot create app download cache"))?,
        );
        downloader
            .download_single(
                DownloadRequest::new(cask.url.clone(), cask.sha256.clone(), names[0].clone()),
                None,
            )
            .await?
    };
    manager.install_archive(&cask, &path, upgrade)?;
    if !quiet {
        println!(
            "Installed cask:{} {} at {}. Zap is not performed.",
            cask.token,
            cask.version,
            manager.appdir().join(&cask.app.name).display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_safe_tokens_only() {
        assert_eq!(token("cask:imageoptim").unwrap(), "imageoptim");
        for input in [
            "imageoptim",
            "cask:.",
            "cask:..",
            "cask:../escape",
            "cask:a/b",
            "cask:",
            "cask:a\n",
        ] {
            assert!(token(input).is_err(), "{input:?}");
        }
    }
}
