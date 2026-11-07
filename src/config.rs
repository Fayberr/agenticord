//! Where Agenticord keeps its small amount of state: the Discord token, the
//! token MCP clients present, the audit log and the last known build number.

use anyhow::{Context, Result};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub fn dir() -> PathBuf {
    if let Ok(dir) = std::env::var("AGENTICORD_HOME") {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        return Path::new(&dir).join("agenticord");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".config").join("agenticord")
}

pub fn ensure_dir() -> Result<PathBuf> {
    let dir = dir();
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

pub fn read(name: &str) -> Option<String> {
    let text = fs::read_to_string(dir().join(name)).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Writes a file only the owner can read.
pub fn write_secret(name: &str, value: &str) -> Result<()> {
    let path = ensure_dir()?.join(name);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("could not write {}", path.display()))?;
    file.write_all(value.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

pub fn remove(name: &str) {
    let _ = fs::remove_file(dir().join(name));
}

/// The Discord token: the environment first, then the saved file.
pub fn discord_token() -> Option<String> {
    std::env::var("AGENTICORD_TOKEN")
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .or_else(|| read("token"))
}

/// The token MCP clients must present, made on first use.
pub fn mcp_token() -> Result<String> {
    if let Some(token) = read("mcp_token") {
        return Ok(token);
    }
    let token = crate::util::random_hex(32);
    write_secret("mcp_token", &token)?;
    Ok(token)
}
