//! Runtime configuration. Nothing machine- or account-specific is compiled in.
//! File: $UU_CONFIG, else $XDG_CONFIG_HOME/uuremote/config.json, else ~/.config/uuremote/config.json.
//! Environment variables override file values. Keep the file private (chmod 600).

use std::path::PathBuf;
use std::sync::OnceLock;

/// Tests that point UU_CONFIG at a temp file must take this, or they race.
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

pub fn path() -> PathBuf {
    if let Some(p) = std::env::var_os("UU_CONFIG") { return PathBuf::from(p); }
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("uuremote/config.json")
}

/// Re-reads the config file (used after `login` rewrites it while the server runs).
pub fn read_fresh() -> Result<Option<serde_json::Value>, String> {
    let p = path();
    match std::fs::read(&p) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| format!("invalid config {}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read config {}: {e}", p.display())),
    }
}

/// Parsed config file, or None when absent. A malformed file is an error, not silently ignored.
pub fn file() -> Result<Option<&'static serde_json::Value>, String> {
    static FILE: OnceLock<Result<Option<serde_json::Value>, String>> = OnceLock::new();
    FILE.get_or_init(|| {
        let p = path();
        match std::fs::read(&p) {
            Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|e| format!("invalid config {}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("cannot read config {}: {e}", p.display())),
        }
    }).as_ref().map(|v| v.as_ref()).map_err(Clone::clone)
}

/// Optional bundled FFmpeg prefix: UU_FFMPEG_ROOT or config "ffmpeg_root".
pub fn ffmpeg_root() -> Option<PathBuf> {
    std::env::var_os("UU_FFMPEG_ROOT").filter(|v| !v.is_empty()).map(PathBuf::from)
        .or_else(|| file().ok().flatten().and_then(|f| f.get("ffmpeg_root")).and_then(|v| v.as_str()).map(PathBuf::from))
}

/// Where the reported client version comes from, for `doctor` (a name, never a secret).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VersionSource { Default, Config, Environment }

impl VersionSource {
    pub fn as_str(self) -> &'static str {
        match self { Self::Default => "default", Self::Config => "config", Self::Environment => "environment" }
    }
}

/// Trimmed non-empty environment variable, so `UU_CLIENT_VERSION=""` means unset.
fn env_override(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}
/// Trimmed non-empty string from the config file.
fn config_override(key: &str) -> Option<String> {
    file().ok().flatten().and_then(|f| f.get(key)).and_then(|v| v.as_str())
        .map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned)
}

/// Reported client version (x-param-vn and User-Agent).
/// Defaults to matching modern official Windows client (4.45.0.4318).
/// `UU_CLIENT_VERSION` (also set by `--client-version`) wins over config "client_version".
pub fn client_version() -> String {
    client_version_with_source().0
}

/// `client_version()` plus where the value came from.
pub fn client_version_with_source() -> (String, VersionSource) {
    if let Some(v) = env_override("UU_CLIENT_VERSION") { return (v, VersionSource::Environment); }
    if let Some(v) = config_override("client_version") { return (v, VersionSource::Config); }
    ("4.45.0.4318".into(), VersionSource::Default)
}

/// Reported client version code (x-param-vc). An explicit `UU_VERSION_CODE` or config
/// "version_code" wins; otherwise it is the trailing component of the reported version, so a
/// version override cannot leave the pair mismatched (4.45.0.4318 -> 4318).
pub fn version_code() -> String {
    version_code_with_source().0
}

/// `version_code()` plus where the value came from. A derived code reports the version's source.
pub fn version_code_with_source() -> (String, VersionSource) {
    if let Some(v) = env_override("UU_VERSION_CODE") { return (v, VersionSource::Environment); }
    if let Some(v) = config_override("version_code") { return (v, VersionSource::Config); }
    let (version, source) = client_version_with_source();
    (derive_version_code(&version).unwrap_or_else(|| "4318".into()), source)
}

/// Trailing dot-separated component when it is all digits ("4.45.0.4318" -> "4318").
fn derive_version_code(version: &str) -> Option<String> {
    let last = version.rsplit('.').next()?.trim();
    (!last.is_empty() && last.bytes().all(|b| b.is_ascii_digit())).then(|| last.to_owned())
}

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn version_code_is_the_trailing_build_number() {
        assert_eq!(derive_version_code("4.45.0.4318").as_deref(), Some("4318"));
        assert_eq!(derive_version_code("4.42.0.2770").as_deref(), Some("2770"));
        assert_eq!(derive_version_code("4.45.0").as_deref(), Some("0"));
        assert_eq!(derive_version_code("4.45").as_deref(), Some("45"));
        assert_eq!(derive_version_code("4.45.0-beta"), None);
        assert_eq!(derive_version_code(""), None);
    }

    #[test]
    fn source_names_are_stable_for_doctor() {
        assert_eq!(VersionSource::Default.as_str(), "default");
        assert_eq!(VersionSource::Config.as_str(), "config");
        assert_eq!(VersionSource::Environment.as_str(), "environment");
    }
}

