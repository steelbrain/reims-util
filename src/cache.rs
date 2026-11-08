//! Files fetched once and kept: the OpenCore release, OcBinaryData and the OVMF package, each
//! pinned to its SHA-256, under `$XDG_CACHE_HOME/reims-util` (`~/.cache/reims-util`).

use crate::http::{self, Dest, Download, NoCheck};
use crate::progress::Reporter;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

pub fn dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".cache"),
    };
    Ok(base.join("reims-util"))
}

pub struct Pinned {
    /// Its name in the cache.
    pub name: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    /// What it is, for the progress line: "OpenCore 1.0.8".
    pub what: &'static str,
}

/// The cached file, downloaded (or its download resumed) first if it is not there.
pub fn fetch(p: &Pinned, r: &dyn Reporter) -> Result<PathBuf> {
    let path = dir()?.join(p.name);
    if path.exists() {
        if http::sha256_file(&path)? == p.sha256 {
            return Ok(path);
        }
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    let agent = http::agent();
    http::download(
        &agent,
        Download::new(p.url, format!("Downloading {}", p.what)),
        Dest::File(&path),
        &mut NoCheck,
        r,
    )?;
    let got = http::sha256_file(&path)?;
    if got != p.sha256 {
        let _ = std::fs::remove_file(&path);
        bail!(
            "{} from {} has SHA-256 {got}, not the pinned {}",
            p.what,
            p.url,
            p.sha256
        );
    }
    Ok(path)
}
