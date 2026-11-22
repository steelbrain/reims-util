//! Files fetched once and kept: the OpenCore release, OcBinaryData and the OVMF package, each
//! pinned to its SHA-256, under `$XDG_CACHE_HOME/reims-util` (`~/.cache/reims-util`).
//!
//! A pinned file can have more than one source, each serving the same bytes. They are tried in
//! order, and the pinned SHA-256 decides: a source that fails, or whose file has another hash,
//! hands the download to the next one, which goes on from what the earlier one left.

use crate::http::{self, Dest, Download, NoCheck};
use crate::progress::{self, Reporter};
use anyhow::{Context, Result, anyhow, bail};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use ureq::Agent;

/// Failed requests in a row before a source hands the download to the next: a host that answers
/// nothing then costs about a minute and a half (three 30-second connects), not the nine or so
/// minutes of [`http::MAX_FAILURES`] tries. The last source gets all of those.
const FAILOVER_TRIES: u32 = 2;

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
    /// Where it is downloaded from, tried in this order.
    pub urls: &'static [&'static str],
    pub sha256: &'static str,
    /// What it is, for the progress line: "OpenCore 1.0.8".
    pub what: &'static str,
}

/// The cached file, downloaded (or its download resumed) first if it is not there.
pub fn fetch(p: &Pinned, r: &dyn Reporter) -> Result<PathBuf> {
    let path = dir()?.join(p.name);
    fetch_to(p, &path, &http::agent(), r)?;
    Ok(path)
}

fn fetch_to(p: &Pinned, path: &Path, agent: &Agent, r: &dyn Reporter) -> Result<()> {
    if path.exists() {
        if http::sha256_file(path)? == p.sha256 {
            return Ok(());
        }
        std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    }
    // A file with one source (OpenCore's) fails with that source's own error.
    let alone = p.urls.len() == 1;
    let mut failed = Vec::new();
    for (i, url) in p.urls.iter().enumerate() {
        let next = p.urls.get(i + 1);
        let mut d = Download::new(url, format!("Downloading {}", p.what));
        if next.is_some() {
            d.tries = FAILOVER_TRIES;
        }
        let answer = match http::download(agent, d, Dest::File(path), &mut NoCheck, r) {
            Ok(()) => {
                let got = http::sha256_file(path)?;
                if got == p.sha256 {
                    return Ok(());
                }
                let _ = std::fs::remove_file(path);
                if alone {
                    bail!(
                        "{} from {url} has SHA-256 {got}, not the pinned {}",
                        p.what,
                        p.sha256
                    );
                }
                format!("served a file with SHA-256 {got}")
            }
            Err(e) if alone || e.is::<progress::Cancelled>() => return Err(e),
            Err(e) => answer(url, &e),
        };
        if let Some(next) = next {
            r.note(&format!(
                "{} did not serve it: trying {}",
                host(url),
                host(next)
            ));
        }
        failed.push((*url, answer));
    }
    Err(anyhow!(all_failed(p, path, &failed)))
}

/// What a source answered: its error, without the URL the error starts with.
fn answer(url: &str, e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    match s.strip_prefix(url) {
        Some(rest) => rest.trim_start_matches(':').trim_start().to_string(),
        None => s,
    }
}

fn host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split('/').next().unwrap_or(rest)
}

/// The error once every source failed: each source and what it answered, and what to do.
fn all_failed(p: &Pinned, path: &Path, failed: &[(&str, String)]) -> String {
    let mut s = format!(
        "could not download {} from any of its {} sources:",
        p.what,
        failed.len()
    );
    for (url, answer) in failed {
        let _ = write!(s, "\n  {url}\n    {answer}");
    }
    let _ = write!(
        s,
        "\nTry again later. Or download {} yourself, from any source that has it, and put it \
         at\n  {}\nwhere reims-util uses it if its SHA-256 is {}.",
        p.name,
        path.display(),
        p.sha256
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Event;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    const GOOD: &[u8] = b"the pinned file's bytes";
    const OTHER: &[u8] = b"some other bytes";

    /// What the fake network answers for a path.
    #[derive(Clone, Copy)]
    enum Answer {
        Status(u16),
        /// The file, or the part of it from where a `Range` header starts.
        File(&'static [u8]),
    }

    /// The network, faked: an HTTP server on 127.0.0.1 that answers each path as its routes say
    /// (404 for any other) and records the paths it was asked for, in order.
    struct Server {
        base: String,
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start(routes: &[(&'static str, Answer)]) -> Server {
            let routes = routes.to_vec();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let asked = Arc::new(Mutex::new(Vec::new()));
            let log = asked.clone();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(mut conn) = conn else { continue };
                    let mut req = BufReader::new(conn.try_clone().unwrap());
                    let mut line = String::new();
                    let _ = req.read_line(&mut line);
                    let path = line.split(' ').nth(1).unwrap_or("").to_string();
                    let mut from = 0;
                    loop {
                        let mut h = String::new();
                        if req.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("range: bytes=") {
                            from = v.split('-').next().unwrap().trim().parse().unwrap();
                        }
                    }
                    log.lock().unwrap().push(path.clone());
                    let answer = routes
                        .iter()
                        .find(|(p, _)| *p == path)
                        .map_or(Answer::Status(404), |(_, a)| *a);
                    let resp = match answer {
                        Answer::Status(s) => format!(
                            "HTTP/1.1 {s} Fake\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .into_bytes(),
                        Answer::File(data) => {
                            let from = from.min(data.len());
                            let mut v = format!(
                                "HTTP/1.1 206 Partial Content\r\ncontent-range: bytes {from}-{}/{}\r\n\
                                 content-length: {}\r\nconnection: close\r\n\r\n",
                                data.len() - 1,
                                data.len(),
                                data.len() - from
                            )
                            .into_bytes();
                            v.extend_from_slice(&data[from..]);
                            v
                        }
                    };
                    let _ = conn.write_all(&resp);
                }
            });
            Server { base, asked }
        }

        fn url(&self, path: &str) -> &'static str {
            Box::leak(format!("{}{path}", self.base).into_boxed_str())
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    #[derive(Default)]
    struct Log {
        notes: Mutex<Vec<String>>,
        cancel: AtomicBool,
    }

    impl Reporter for Log {
        fn event(&self, e: Event) {
            if let Event::Note { message } = e {
                self.notes.lock().unwrap().push(message);
            }
        }
        fn cancelled(&self) -> bool {
            self.cancel.load(Ordering::Relaxed)
        }
    }

    fn sha(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        http::hex(&Sha256::digest(data))
    }

    fn pinned(urls: Vec<&'static str>) -> Pinned {
        Pinned {
            name: "file.pkg",
            urls: Box::leak(urls.into_boxed_slice()),
            sha256: Box::leak(sha(GOOD).into_boxed_str()),
            what: "the test file",
        }
    }

    /// Where the test's file is cached: in an empty folder of its own.
    fn cache_path(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("reims-util-cache-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("file.pkg")
    }

    #[test]
    fn tries_the_sources_in_order_and_keeps_the_first_with_the_pinned_hash() {
        let s = Server::start(&[
            ("/missing", Answer::Status(404)),
            ("/other", Answer::File(OTHER)),
            ("/good", Answer::File(GOOD)),
            ("/also-good", Answer::File(GOOD)),
        ]);
        let p = pinned(vec![
            s.url("/missing"),
            s.url("/other"),
            s.url("/good"),
            s.url("/also-good"),
        ]);
        let path = cache_path("order");
        let r = Log::default();
        fetch_to(&p, &path, &http::agent(), &r).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), GOOD);
        assert_eq!(s.asked(), ["/missing", "/other", "/good"]);
        let host = host(&s.base).to_string();
        assert_eq!(
            *r.notes.lock().unwrap(),
            [
                format!("{host} did not serve it: trying {host}"),
                format!("{host} did not serve it: trying {host}"),
            ]
        );
    }

    #[test]
    fn a_failing_source_hands_over_sooner_when_another_follows() {
        let s = Server::start(&[
            ("/busy", Answer::Status(503)),
            ("/good", Answer::File(GOOD)),
        ]);
        let p = pinned(vec![s.url("/busy"), s.url("/good")]);
        let path = cache_path("failover");
        fetch_to(&p, &path, &http::agent(), &Log::default()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), GOOD);
        // The first request and FAILOVER_TRIES more.
        assert_eq!(s.asked(), ["/busy", "/busy", "/busy", "/good"]);
    }

    #[test]
    fn names_each_source_and_its_answer_when_every_source_fails() {
        let s = Server::start(&[
            ("/missing", Answer::Status(404)),
            ("/other", Answer::File(OTHER)),
        ]);
        let (missing, other) = (s.url("/missing"), s.url("/other"));
        let p = pinned(vec![missing, other]);
        let path = cache_path("all-fail");
        let e = fetch_to(&p, &path, &http::agent(), &Log::default()).unwrap_err();
        assert_eq!(
            format!("{e:#}"),
            format!(
                "could not download the test file from any of its 2 sources:\n  \
                 {missing}\n    answered HTTP 404\n  \
                 {other}\n    served a file with SHA-256 {}\n\
                 Try again later. Or download file.pkg yourself, from any source that has it, \
                 and put it at\n  {}\nwhere reims-util uses it if its SHA-256 is {}.",
                sha(OTHER),
                path.display(),
                sha(GOOD)
            )
        );
        assert!(!path.exists());
        assert_eq!(s.asked(), ["/missing", "/other"]);
    }

    #[test]
    fn uses_a_file_put_where_the_cache_looks() {
        let s = Server::start(&[]);
        let p = pinned(vec![s.url("/missing"), s.url("/missing-too")]);
        let path = cache_path("placed");
        std::fs::write(&path, GOOD).unwrap();
        fetch_to(&p, &path, &http::agent(), &Log::default()).unwrap();
        assert!(s.asked().is_empty());
    }

    #[test]
    fn stopping_does_not_hand_over_to_the_next_source() {
        let s = Server::start(&[("/good", Answer::File(GOOD))]);
        let p = pinned(vec![s.url("/good"), s.url("/good")]);
        let path = cache_path("stop");
        let r = Log::default();
        r.cancel.store(true, Ordering::Relaxed);
        let e = fetch_to(&p, &path, &http::agent(), &r).unwrap_err();
        assert!(e.is::<progress::Cancelled>());
        assert!(s.asked().is_empty());
    }

    #[test]
    fn a_file_with_one_source_fails_with_its_own_error() {
        let s = Server::start(&[]);
        let url = s.url("/missing");
        let e = fetch_to(
            &pinned(vec![url]),
            &cache_path("alone"),
            &http::agent(),
            &Log::default(),
        )
        .unwrap_err();
        assert_eq!(format!("{e:#}"), format!("{url} answered HTTP 404"));
    }
}
