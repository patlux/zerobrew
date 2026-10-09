//! The deliberately small, fail-closed contract for standalone macOS app casks.
//!
//! Binary casks keep their existing installer. App casks must not reach that
//! installer: copying a CLI executable out of a signed bundle breaks the app.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppCask {
    pub token: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub app: AppArtifact,
    /// Recorded for the operator; never interpreted as permission to delete data.
    pub has_zap: bool,
    pub auto_updates: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppArtifact {
    /// A single top-level bundle. Nested sources and renamed targets are not yet supported.
    pub name: String,
}

/// Supplied by the caller, so metadata validation is deterministic and testable.
#[derive(Debug, Clone, Copy)]
pub struct MacPlatform<'a> {
    pub arch: &'a str,
    pub version: &'a str,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgument {
        message: message.into(),
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, Error> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(format!("app cask is missing string field '{key}'")))
}

fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | '$' | '~'))
}

fn version_parts(version: &str) -> Result<[u32; 3], Error> {
    let mut result = [0; 3];
    for (i, part) in version.split('.').enumerate() {
        if i >= result.len() || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid(format!("unsupported macOS version '{version}'")));
        }
        result[i] = part
            .parse()
            .map_err(|_| invalid("macOS version component overflow"))?;
    }
    Ok(result)
}

fn codename(version: [u32; 3]) -> Option<&'static str> {
    match version[0] {
        11 => Some("big_sur"),
        12 => Some("monterey"),
        13 => Some("ventura"),
        14 => Some("sonoma"),
        15 => Some("sequoia"),
        26 => Some("tahoe"),
        _ => None,
    }
}

/// Resolve one app and all its prerequisites before any filesystem mutation.
/// Unknown lifecycle artifacts are errors, not optional best-effort actions.
pub fn resolve_app_cask(
    token: &str,
    metadata: &Value,
    platform: MacPlatform<'_>,
) -> Result<AppCask, Error> {
    if !component(token)
        || !token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'@' | b'.'))
    {
        return Err(invalid(
            "app cask token must be an unqualified safe component",
        ));
    }
    if !matches!(platform.arch, "aarch64" | "x86_64") {
        return Err(invalid(
            "app casks support only Apple Silicon and Intel macOS",
        ));
    }
    let running = version_parts(platform.version)?;
    let mut resolved = metadata.clone();
    if let Some(variations) = metadata.get("variations") {
        let variations = variations
            .as_object()
            .ok_or_else(|| invalid("invalid cask variations"))?;
        if !variations.is_empty() {
            let os = codename(running)
                .ok_or_else(|| invalid("cannot select app cask variation for this macOS"))?;
            let key = if platform.arch == "aarch64" {
                format!("arm64_{os}")
            } else {
                os.to_string()
            };
            // The API's base object is a platform-specific default, not a safe
            // fallback for another architecture or release.
            let variation = variations
                .get(&key)
                .and_then(Value::as_object)
                .ok_or_else(|| invalid(format!("no explicit app cask variation for '{key}'")))?;
            let object = resolved
                .as_object_mut()
                .ok_or_else(|| invalid("invalid cask object"))?;
            for (key, value) in variation {
                object.insert(key.clone(), value.clone());
            }
        }
    }
    let value = &resolved;
    if value.get("disabled").and_then(Value::as_bool) == Some(true) {
        return Err(invalid(format!("app cask '{token}' is disabled upstream")));
    }
    if let Some(actual) = value.get("token").and_then(Value::as_str)
        && actual != token
    {
        return Err(invalid("app cask token does not match the request"));
    }
    let version = string(value, "version")?;
    if !component(version) || version == "latest" {
        return Err(invalid("app casks require a fixed, safe version"));
    }
    let sha256 = string(value, "sha256")?;
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(
            "app casks require a fixed SHA-256; no_check is not supported",
        ));
    }
    let url = string(value, "url")?;
    if !url.starts_with("https://")
        || !url.ends_with(".tar.xz")
        || url.chars().any(char::is_control)
    {
        return Err(invalid("app pilot supports HTTPS .tar.xz downloads only"));
    }
    validate_dependencies(value.get("depends_on"), platform, running)?;
    for key in ["conflicts_with", "container", "url_specs"] {
        if let Some(extra) = value.get(key)
            && !extra.is_null()
            && extra != &serde_json::json!({})
        {
            return Err(invalid(format!("unsupported app cask metadata '{key}'")));
        }
    }
    let artifacts = value
        .get("artifacts")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("app cask is missing artifacts"))?;
    let mut app = None;
    let mut has_zap = false;
    for artifact in artifacts {
        let object = artifact
            .as_object()
            .ok_or_else(|| invalid("invalid cask artifact"))?;
        if object.len() == 1 && object.contains_key("zap") {
            if !object["zap"].is_array() {
                return Err(invalid("invalid zap metadata"));
            }
            has_zap = true;
            continue;
        }
        if !object.contains_key("app") || object.keys().any(|k| k != "app" && k != "target") {
            return Err(invalid(format!(
                "unsupported app cask artifact: {}",
                object.keys().cloned().collect::<Vec<_>>().join(", ")
            )));
        }
        let entries = object["app"]
            .as_array()
            .ok_or_else(|| invalid("invalid app artifact"))?;
        let name = entries
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("missing app bundle name"))?;
        if entries.len() != 1 || !component(name) || !name.ends_with(".app") || name == ".app" {
            return Err(invalid("app pilot requires one top-level .app bundle"));
        }
        if let Some(target) = object.get("target")
            && target.as_str() != Some(format!("/Applications/{name}").as_str())
        {
            return Err(invalid("custom app artifact targets are not supported"));
        }
        if app
            .replace(AppArtifact {
                name: name.to_owned(),
            })
            .is_some()
        {
            return Err(invalid("app pilot supports exactly one app artifact"));
        }
    }
    Ok(AppCask {
        token: token.to_owned(),
        version: version.to_owned(),
        url: url.to_owned(),
        sha256: sha256.to_ascii_lowercase(),
        app: app.ok_or_else(|| invalid("app cask has no app artifact"))?,
        has_zap,
        auto_updates: value
            .get("auto_updates")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn validate_dependencies(
    value: Option<&Value>,
    platform: MacPlatform<'_>,
    running: [u32; 3],
) -> Result<(), Error> {
    let Some(value) = value else {
        return Ok(());
    };
    let object = value
        .as_object()
        .ok_or_else(|| invalid("invalid cask depends_on"))?;
    for (key, constraint) in object {
        match key.as_str() {
            "macos" => {
                let constraints = constraint
                    .as_object()
                    .ok_or_else(|| invalid("invalid macOS requirement"))?;
                for (op, versions) in constraints {
                    let versions = versions
                        .as_array()
                        .ok_or_else(|| invalid("invalid macOS versions"))?;
                    if versions.len() != 1 {
                        return Err(invalid("unsupported multi-version macOS requirement"));
                    }
                    let required = version_parts(
                        versions[0]
                            .as_str()
                            .ok_or_else(|| invalid("invalid macOS version"))?,
                    )?;
                    let matches = match op.as_str() {
                        ">=" => running >= required,
                        ">" => running > required,
                        "<=" => running <= required,
                        "<" => running < required,
                        "==" => running == required,
                        _ => return Err(invalid(format!("unsupported macOS constraint '{op}'"))),
                    };
                    if !matches {
                        return Err(invalid("app cask does not support this macOS version"));
                    }
                }
            }
            "arch" => {
                let options = constraint
                    .as_array()
                    .ok_or_else(|| invalid("invalid architecture requirement"))?;
                let required = if platform.arch == "aarch64" {
                    "arm"
                } else {
                    "intel"
                };
                let mut matches = false;
                for option in options {
                    let option = option
                        .as_object()
                        .ok_or_else(|| invalid("invalid architecture option"))?;
                    if option.len() != 2
                        || !option.contains_key("type")
                        || option.get("bits").and_then(Value::as_u64) != Some(64)
                    {
                        return Err(invalid("unsupported architecture option"));
                    }
                    matches |= option.get("type").and_then(Value::as_str) == Some(required);
                }
                if !matches {
                    return Err(invalid("app cask does not support this architecture"));
                }
            }
            _ => return Err(invalid(format!("unsupported app cask dependency '{key}'"))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../fixtures/cask_imageoptim.json")).unwrap()
    }
    fn platform() -> MacPlatform<'static> {
        MacPlatform {
            arch: "aarch64",
            version: "26.1",
        }
    }
    fn resolve(v: &Value) -> Result<AppCask, Error> {
        resolve_app_cask("imageoptim", v, platform())
    }

    #[test]
    fn resolves_imageoptim_without_interpreting_zap() {
        let cask = resolve(&fixture()).unwrap();
        assert_eq!(cask.app.name, "ImageOptim.app");
        assert_eq!(cask.version, "1.9.3");
        assert!(cask.has_zap && cask.auto_updates);
    }

    #[test]
    fn rejects_every_unsupported_lifecycle_artifact() {
        for kind in [
            "pkg",
            "binary",
            "preflight",
            "postflight_steps",
            "uninstall",
            "uninstall_postflight_steps",
            "command_wrapper",
            "unknown",
        ] {
            let mut v = fixture();
            v["artifacts"]
                .as_array_mut()
                .unwrap()
                .push(json!({kind: ["action"]}));
            assert!(resolve(&v).is_err(), "accepted {kind}");
        }
    }

    #[test]
    fn rejects_unverified_or_unsupported_downloads() {
        for checksum in ["no_check", "", "../oops", "1234", &"z".repeat(64)] {
            let mut v = fixture();
            v["sha256"] = json!(checksum);
            assert!(resolve(&v).is_err());
        }
        for url in [
            "http://example.org/app.tar.xz",
            "https://example.org/app.dmg",
            "https://example.org/app.zip",
        ] {
            let mut v = fixture();
            v["url"] = json!(url);
            assert!(resolve(&v).is_err());
        }
    }

    #[test]
    fn rejects_path_injection_and_multi_app_casks() {
        for name in [
            "../Bad.app",
            "/Bad.app",
            "A/Bad.app",
            ".app",
            "Bad.app\n",
            "$APPDIR/Bad.app",
            "Bad\\.app",
        ] {
            let mut v = fixture();
            v["artifacts"] = json!([{ "app": [name] }]);
            assert!(resolve(&v).is_err(), "accepted {name}");
        }
        let mut v = fixture();
        v["artifacts"] = json!([{ "app": ["A.app"] }, { "app": ["B.app"] }]);
        assert!(resolve(&v).is_err());
        for token in ["../imageoptim", "tap/repo/imageoptim", "", ".."] {
            assert!(resolve_app_cask(token, &fixture(), platform()).is_err());
        }
        for version in ["../1.0", "latest", "1/2"] {
            let mut v = fixture();
            v["version"] = json!(version);
            assert!(resolve(&v).is_err());
        }
    }

    #[test]
    fn rejects_custom_targets_and_required_metadata() {
        let mut v = fixture();
        v["artifacts"][0]["target"] = json!("/Library/ImageOptim.app");
        assert!(resolve(&v).is_err());
        for key in ["conflicts_with", "container", "url_specs"] {
            let mut v = fixture();
            v[key] = json!({"unsupported": true});
            assert!(resolve(&v).is_err());
        }
        let mut v = fixture();
        v["disabled"] = json!(true);
        assert!(resolve(&v).is_err());
    }

    #[test]
    fn checks_os_and_arch_and_rejects_unknown_dependencies() {
        let mut v = fixture();
        v["depends_on"] = json!({"macos": {">=": ["26.2"]}});
        assert!(resolve(&v).is_err());
        v["depends_on"] = json!({"macos": {">=": ["26.0"]}, "arch": [{"type": "arm", "bits": 64}]});
        assert!(resolve(&v).is_ok());
        assert!(
            resolve_app_cask(
                "imageoptim",
                &v,
                MacPlatform {
                    arch: "x86_64",
                    version: "26.1"
                }
            )
            .is_err()
        );
        v["depends_on"] = json!({"formula": ["something"]});
        assert!(resolve(&v).is_err());
        v["depends_on"] = json!({"macos": {"!=": ["15"]}});
        assert!(resolve(&v).is_err());
    }

    #[test]
    fn requires_exact_variation_and_validates_its_artifacts() {
        let mut v = fixture();
        v["variations"] = json!({"arm64_tahoe": {"version":"2.0"}});
        assert_eq!(resolve(&v).unwrap().version, "2.0");
        assert!(
            resolve_app_cask(
                "imageoptim",
                &v,
                MacPlatform {
                    arch: "x86_64",
                    version: "26.1"
                }
            )
            .is_err()
        );
        assert!(
            resolve_app_cask(
                "imageoptim",
                &v,
                MacPlatform {
                    arch: "aarch64",
                    version: "27.0"
                }
            )
            .is_err()
        );
        v["variations"]["arm64_tahoe"]["artifacts"] = json!([{"pkg": ["app.pkg"]}]);
        assert!(resolve(&v).is_err());
    }
}
