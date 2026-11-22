//! HTTP: small fetches, and downloads that resume.
//!
//! A download writes `<dest>.part` and renames it to `<dest>` once every byte is in and checked,
//! so an interrupted download (a dropped connection, Ctrl+C, a closed terminal) leaves a `.part`
//! file that the next run continues from. Or it writes into its place in a bigger file, a disk
//! image's ([`Dest::Region`]), where the next run continues after what the chunklist passes.
//!
//! It asks for the file in ranges of [`SEGMENT`] bytes, each with a time budget: a stalled
//! connection fails within [`SEGMENT_BUDGET`] and the download goes on from its last byte, on a
//! new connection. Apple's and GitHub's servers, and
//! the Arch Linux archive, all answer ranges.

use crate::progress::{self, Reporter};
use anyhow::{Context, Result, bail};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use ureq::Agent;

/// The most each request asks for.
pub const SEGMENT: u64 = 64 << 20;
/// How long a request may take to send its range: a stall is noticed within this. (On a
/// connection too slow to send a whole range in it, each request sends what it can.)
pub const SEGMENT_BUDGET: Duration = Duration::from_secs(180);
/// Failed requests in a row, with no byte received between them, before a download gives up
/// (unless its [`Download::tries`] says otherwise).
pub const MAX_FAILURES: u32 = 10;

pub type Headers = Vec<(String, String)>;

pub fn agent() -> Agent {
    Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .user_agent(concat!("reims-util/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// A whole small response: a catalog, a chunklist, a release's manifest.
pub fn get(agent: &Agent, url: &str, headers: &Headers, limit: u64) -> Result<Vec<u8>> {
    let mut failures = 0;
    loop {
        let mut req = agent.get(url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let attempt = req
            .call()
            .map_err(anyhow::Error::from)
            .and_then(|mut resp| {
                let status = resp.status().as_u16();
                if status != 200 {
                    bail!("{url} answered HTTP {status}");
                }
                Ok(resp.body_mut().with_config().limit(limit).read_to_vec()?)
            });
        match attempt {
            Ok(v) => return Ok(v),
            Err(e) if failures < 3 && retryable(&e) => {
                failures += 1;
                std::thread::sleep(Duration::from_secs(1 << failures));
            }
            Err(e) => return Err(e.context(format!("fetch {url}"))),
        }
    }
}

fn retryable(e: &anyhow::Error) -> bool {
    match e.downcast_ref::<ureq::Error>() {
        Some(
            ureq::Error::StatusCode(_) | ureq::Error::BadUri(_) | ureq::Error::BodyExceedsLimit(_),
        ) => false,
        Some(_) => true,
        None => e.to_string().contains("HTTP 5"),
    }
}

/// What a download checks as its bytes arrive: a chunklist's chunks, or nothing.
pub trait Check {
    /// Reads the `len` bytes from `offset` in `file` that a previous run wrote, and returns how
    /// many of them are good: the download keeps those and continues after them.
    fn resume(&mut self, file: &mut File, offset: u64, len: u64, r: &dyn Reporter) -> Result<u64>;
    /// Whether `resume` reads the bytes: without that, only a file's own length says how much a
    /// previous run wrote.
    fn verifies(&self) -> bool;
    /// The next bytes of the file. On a chunk that does not match, returns the offset the file is
    /// good up to, where the download starts again.
    fn feed(&mut self, data: &[u8]) -> std::result::Result<(), u64>;
    /// The download starts again at `offset`, a value `resume` or `feed` returned, or 0.
    fn restart_at(&mut self, offset: u64);
    /// Every byte is in: the whole file is good.
    fn finish(&mut self) -> Result<()>;
}

/// Checks nothing as the bytes arrive (a pinned file's hash is checked once it is complete).
pub struct NoCheck;

impl Check for NoCheck {
    fn resume(&mut self, _: &mut File, _: u64, len: u64, _: &dyn Reporter) -> Result<u64> {
        Ok(len)
    }
    fn verifies(&self) -> bool {
        false
    }
    fn feed(&mut self, _: &[u8]) -> std::result::Result<(), u64> {
        Ok(())
    }
    fn restart_at(&mut self, _: u64) {}
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
}

pub struct Download<'a> {
    pub url: String,
    /// What the download is, as the step it reports: "Downloading OpenCore 1.0.8".
    pub step: String,
    pub headers: Headers,
    /// The file's size, when the caller knows it (a catalog's, a chunklist's).
    pub size: Option<u64>,
    /// For requests the server refuses (401, 403): new headers, such as a fresh asset token.
    pub refresh: Option<&'a mut dyn FnMut() -> Result<Headers>>,
    pub user_agent: Option<&'static str>,
    /// Failed requests in a row, with no byte received between them, before it gives up:
    /// [`MAX_FAILURES`], or fewer when there is another place to download the file from.
    pub tries: u32,
}

impl Download<'_> {
    pub fn new(url: &str, step: String) -> Self {
        Download {
            url: url.to_string(),
            step,
            headers: Vec::new(),
            size: None,
            refresh: None,
            user_agent: None,
            tries: MAX_FAILURES,
        }
    }
}

/// Where a download goes.
pub enum Dest<'a> {
    /// A file of its own: `<path>.part`, renamed to `path` once complete.
    File(&'a Path),
    /// `len` bytes of `file` from `offset`, such as a file's place in a disk image. What is there
    /// already counts as far as the check passes it, and none of it without a check that reads.
    Region {
        file: &'a mut File,
        offset: u64,
        len: u64,
    },
}

pub fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Downloads `d` to `dest`, continuing what a previous run left there. Returns once `dest`
/// holds the whole file and `check` passed it.
pub fn download(
    agent: &Agent,
    mut d: Download,
    dest: Dest,
    check: &mut dyn Check,
    r: &dyn Reporter,
) -> Result<()> {
    let mut own;
    let (file, base, part, existing) = match dest {
        Dest::File(dest) => {
            let part = part_path(dest);
            if let Some(dir) = dest.parent()
                && !dir.as_os_str().is_empty()
            {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("create {}", dir.display()))?;
            }
            own = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&part)
                .with_context(|| format!("open {}", part.display()))?;
            let len = own.metadata()?.len();
            (&mut own, 0, Some((part, dest)), len)
        }
        Dest::Region { file, offset, len } => {
            if d.size.is_some_and(|s| s != len) {
                bail!("{} is not the size of its place", d.url);
            }
            d.size = Some(len);
            let existing = if check.verifies() { len } else { 0 };
            (file, offset, None, existing)
        }
    };
    // A file of its own is cut back to what is good; a region's bytes stay, to be written over.
    let cut = |file: &mut File, at: u64| -> Result<()> {
        if part.is_some() {
            file.set_len(at)?;
        }
        Ok(())
    };
    r.step(&d.step);
    let mut have = if existing > 0 {
        let good = check.resume(file, base, existing, r)?;
        if good > 0 && check.verifies() {
            // The check reported a step of its own; the rest is the download's.
            r.step(&d.step);
        }
        if good < existing {
            cut(file, good)?;
        }
        if good > 0 && d.size.is_none_or(|s| good < s) {
            r.note(&format!("Resuming at {}", progress::bytes(good)));
        }
        good
    } else {
        0
    };
    check.restart_at(have);
    let mut total = d.size;
    let mut failures = 0u32;
    let mut bad_chunks = 0u32;
    let mut buf = vec![0u8; 1 << 18];
    r.progress(have, total);

    'requests: loop {
        progress::check(r)?;
        if total.is_some_and(|t| have >= t) {
            break;
        }
        let end = match total {
            Some(t) => (have + SEGMENT).min(t) - 1,
            None => have + SEGMENT - 1,
        };
        let mut req = agent
            .get(&d.url)
            .header("Range", format!("bytes={have}-{end}"));
        for (k, v) in &d.headers {
            req = req.header(k, v);
        }
        let mut cfg = req.config().timeout_recv_body(Some(SEGMENT_BUDGET));
        if let Some(ua) = d.user_agent {
            cfg = cfg.user_agent(ua);
        }
        let resp = match cfg.build().call() {
            Ok(resp) => resp,
            Err(e) => {
                failures += 1;
                retry_or_fail(&mut failures, d.tries, &format!("{e}"), &d.url, r)?;
                continue;
            }
        };
        let status = resp.status().as_u16();
        match status {
            206 => {
                let range = resp
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .and_then(content_range)
                    .with_context(|| format!("{} sent a range without Content-Range", d.url))?;
                if range.0 != have {
                    bail!(
                        "{} sent bytes from {} when asked for {have}",
                        d.url,
                        range.0
                    );
                }
                match (total, range.1) {
                    (Some(t), Some(rt)) if t != rt => bail!(
                        "{} is {} long, not the expected {}: it changed on the server",
                        d.url,
                        rt,
                        t
                    ),
                    (None, Some(rt)) => {
                        total = Some(rt);
                        r.progress(have, total);
                    }
                    _ => {}
                }
            }
            200 => {
                // The whole file: the server ignored the range.
                if have > 0 {
                    cut(file, 0)?;
                    have = 0;
                    check.restart_at(0);
                }
                if total.is_none() {
                    total = resp.body().content_length();
                }
            }
            416 if total.is_some_and(|t| have >= t) => break,
            416 => {
                // Past the end: the file on the server is shorter than what we have.
                if have == 0 {
                    bail!("{} refused every range", d.url);
                }
                r.note("The file changed on the server: starting over");
                cut(file, 0)?;
                have = 0;
                check.restart_at(0);
                continue;
            }
            401 | 403 if d.refresh.is_some() && failures < 3 => {
                failures += 1;
                if let Some(f) = d.refresh.as_mut() {
                    d.headers = f()?;
                }
                continue;
            }
            408 | 429 | 500..=599 => {
                failures += 1;
                retry_or_fail(&mut failures, d.tries, &format!("HTTP {status}"), &d.url, r)?;
                continue;
            }
            _ => bail!("{} answered HTTP {status}", d.url),
        }

        file.seek(SeekFrom::Start(base + have))?;
        let mut body = resp.into_body().into_reader();
        let start = have;
        loop {
            if r.cancelled() {
                file.flush()?;
                return Err(progress::Cancelled.into());
            }
            let n = match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    file.flush()?;
                    if have > start {
                        // Cut off partway, which is how a request on a slow connection ends
                        // when its time is up: the next one goes on from here.
                        failures = 0;
                        continue 'requests;
                    }
                    failures += 1;
                    retry_or_fail(&mut failures, d.tries, &format!("{e}"), &d.url, r)?;
                    continue 'requests;
                }
            };
            let data = match total {
                Some(t) => &buf[..n.min((t - have) as usize)],
                None => &buf[..n],
            };
            file.write_all(data).context("write the download")?;
            if let Err(good) = check.feed(data) {
                bad_chunks += 1;
                if bad_chunks > 3 {
                    bail!("{} keeps arriving damaged", d.url);
                }
                r.note(&format!(
                    "A chunk at {} did not match its checksum: downloading it again",
                    progress::bytes(good)
                ));
                file.flush()?;
                cut(file, good)?;
                have = good;
                check.restart_at(good);
                continue 'requests;
            }
            have += data.len() as u64;
            failures = 0;
            r.progress(have, total);
        }
        if total.is_none() && have < end + 1 {
            // A server that ignored the range sent the whole file.
            total = Some(have);
        }
    }
    file.flush()?;
    check.finish()?;
    if let Some((part, dest)) = &part {
        file.sync_all()?;
        std::fs::rename(part, dest)
            .with_context(|| format!("rename {} to {}", part.display(), dest.display()))?;
    }
    Ok(())
}

fn retry_or_fail(
    failures: &mut u32,
    tries: u32,
    why: &str,
    url: &str,
    r: &dyn Reporter,
) -> Result<()> {
    if *failures > tries {
        bail!("{url}: {why} (gave up after {tries} tries; run again to resume)");
    }
    let wait = Duration::from_secs((1u64 << (*failures).min(5)).min(30));
    r.note(&format!(
        "{why}: trying again in {}",
        progress::duration(wait.as_secs())
    ));
    let until = std::time::Instant::now() + wait;
    while std::time::Instant::now() < until {
        progress::check(r)?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// "bytes 100-199/1000" as (100, Some(1000)); "bytes 100-199/*" as (100, None).
fn content_range(v: &str) -> Option<(u64, Option<u64>)> {
    let v = v.trim().strip_prefix("bytes")?.trim();
    let (range, total) = v.split_once('/')?;
    let (start, _) = range.split_once('-')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((start.trim().parse().ok()?, total))
}

/// Writes a small file whole: a `.part` file renamed over it.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = part_path(path);
    std::fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// The SHA-256 of a file, in lowercase hex.
pub fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(
            content_range("bytes 100-115/960530321"),
            Some((100, Some(960530321)))
        );
        assert_eq!(content_range("bytes 0-0/*"), Some((0, None)));
        assert_eq!(content_range("items 0-1/2"), None);
    }

    #[test]
    fn part_path_appends() {
        assert_eq!(
            part_path(Path::new("/a/b.dmg")),
            PathBuf::from("/a/b.dmg.part")
        );
    }
}
