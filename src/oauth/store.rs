//! The signed-in session on disk: `<data dir>/flight_sim/session.json`
//! (`~/Library/Application Support` on macOS, `%APPDATA%` on Windows,
//! `$XDG_DATA_HOME` or `~/.local/share` on Linux), readable only by the user
//! (0600 on Unix; `%APPDATA%` is already per-user on Windows).
//!
//! Not the OS keychain: macOS asks for the login password whenever a
//! differently signed binary (every rebuild, every ad-hoc-signed release)
//! reads a keychain item, Windows' Credential Manager caps a secret at 2.5 KB
//! (a session is about that), and Linux needs a running Secret Service.
//!
//! Screenshot runs (headless) never read or write it.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub did: String,
    pub handle: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    pub pds: String,
    pub issuer: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub revocation_endpoint: Option<String>,
    /// The DPoP private key (the JWK `d`, base64url).
    pub dpop_key: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
    pub scope: String,
    #[serde(default)]
    pub as_nonce: Option<String>,
    #[serde(default)]
    pub pds_nonce: Option<String>,
}

fn path() -> Option<PathBuf> {
    if crate::dev::headless() {
        return None;
    }
    let home = |rest: &str| std::env::var_os("HOME").map(|h| std::path::Path::new(&h).join(rest));
    let dir = if cfg!(target_os = "macos") {
        home("Library/Application Support")
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(Into::into)
    } else {
        std::env::var_os("XDG_DATA_HOME").map(Into::into).or_else(|| home(".local/share"))
    }?;
    Some(dir.join("flight_sim").join("session.json"))
}

pub fn load() -> Option<Session> {
    let text = std::fs::read_to_string(path()?).ok()?;
    match serde_json::from_str(&text) {
        Ok(s) => Some(s),
        Err(e) => {
            bevy::log::warn!("sign-in: unreadable stored session ({e}); signed out");
            None
        }
    }
}

pub fn save(session: &Session) {
    let Some(path) = path() else { return };
    if let Err(e) = write(&path, &serde_json::to_vec(session).unwrap_or_default()) {
        bevy::log::warn!("sign-in: couldn't save the session to {}: {e}", path.display());
    }
}

pub fn delete() {
    if let Some(path) = path() {
        let _ = std::fs::remove_file(path);
    }
}

/// Writes via a temporary file (owner-only from the start), then renames.
fn write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut f = options.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_owner_only() {
        let dir = std::env::temp_dir().join(format!("flight_sim_store_{}", std::process::id()));
        let path = dir.join("session.json");
        write(&path, b"{}").unwrap();
        write(&path, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
