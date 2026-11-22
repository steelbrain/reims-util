//! x86-64 macOS recovery images, from osrecovery.apple.com, as a Mac's Internet Recovery fetches
//! them.
//!
//! The exchange, as macrecovery does it (OpenCorePkg's Utilities/macrecovery, BSD-3-Clause):
//! 1. `GET http://osrecovery.apple.com/` with the `InternetRecovery/1.0` user agent sets a
//!    `session` cookie.
//! 2. `POST /InstallationPayload/RecoveryImage` with that cookie and a body of `key=value` lines
//!    (a random client ID `cid`, the logic board serial `sn`, the board ID `bid`, random `k` and
//!    `fg`, and `os`) answers `key: value` lines: the product (`AP`), the image's URL (`AU`) and
//!    asset token (`AT`), and its chunklist's (`CU`, `CT`).
//! 3. Each file is fetched with the cookie `AssetToken=<token>`; a token lasts about 45 minutes,
//!    and a refused request asks for a new one.
//!
//! A board ID with an all-zero serial gets the newest recovery that board runs, so each version
//! below names the board of a Mac whose last macOS it is. Every image here is x86-64: these are
//! Intel Macs' boards.
//!
//! The image is `BaseSystem.dmg`, checked chunk by chunk against its chunklist, whose RSA
//! signature is Apple's (chunklist.rs): both come over plain HTTP.
//!
//! It is the online installer as it is ([`online_installer`]): reims-ctl takes a DMG and
//! converts it, and a VM boots it into Recovery, whose Reinstall macOS downloads the newest
//! release of its macOS. The offline installer boots it too (installer.rs).

use crate::chunklist::{ChunkCheck, Chunklist, Signature};
use crate::http::{self, Dest, Download, Headers};
use crate::progress::Reporter;
use anyhow::{Context, Result, bail, ensure};
use std::path::{Path, PathBuf};
use ureq::Agent;

const SERVER: &str = "http://osrecovery.apple.com";
const USER_AGENT: &str = "InternetRecovery/1.0";
const MLB_ZERO: &str = "00000000000000000";

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Version {
    pub name: &'static str,
    pub number: &'static str,
    /// Its name in file names and on the command line: "tahoe".
    pub slug: &'static str,
    pub board_id: &'static str,
    pub mlb: &'static str,
    pub os: &'static str,
}

impl Version {
    pub fn title(&self) -> String {
        format!("macOS {} {}", self.name, self.number)
    }

    pub fn online_installer_name(&self) -> String {
        format!("macOS-{}-online-installer.dmg", self.name.replace(' ', "-"))
    }
}

/// Newest first. The board IDs and serials are macrecovery's (its recovery_urls.txt), whose
/// authors found which board gets which recovery.
pub const VERSIONS: &[Version] = &[
    Version {
        name: "Tahoe",
        number: "26",
        slug: "tahoe",
        board_id: "Mac-CFF7D910A743CAAF",
        mlb: MLB_ZERO,
        os: "latest",
    },
    Version {
        name: "Sequoia",
        number: "15",
        slug: "sequoia",
        board_id: "Mac-7BA5B2D9E42DDD94",
        mlb: MLB_ZERO,
        os: "default",
    },
    Version {
        name: "Sonoma",
        number: "14",
        slug: "sonoma",
        board_id: "Mac-827FAC58A8FDFA22",
        mlb: MLB_ZERO,
        os: "default",
    },
    Version {
        name: "Ventura",
        number: "13",
        slug: "ventura",
        board_id: "Mac-B4831CEBD52A0C4C",
        mlb: MLB_ZERO,
        os: "default",
    },
    Version {
        name: "Monterey",
        number: "12",
        slug: "monterey",
        board_id: "Mac-E43C1C25D4880AD6",
        mlb: MLB_ZERO,
        os: "default",
    },
    Version {
        name: "Big Sur",
        number: "11",
        slug: "big-sur",
        board_id: "Mac-2BD1B31983FE1663",
        mlb: MLB_ZERO,
        os: "default",
    },
    Version {
        name: "Catalina",
        number: "10.15",
        slug: "catalina",
        board_id: "Mac-00BE6ED71E35EB86",
        mlb: MLB_ZERO,
        os: "default",
    },
    Version {
        name: "Mojave",
        number: "10.14",
        slug: "mojave",
        board_id: "Mac-7BA5B2DFE22DDD8C",
        mlb: "00000000000KXPG00",
        os: "default",
    },
    Version {
        name: "High Sierra",
        number: "10.13",
        slug: "high-sierra",
        board_id: "Mac-7BA5B2D9E42DDD94",
        mlb: "00000000000J80300",
        os: "default",
    },
];

/// A version by its slug, name or number: "tahoe", "Big Sur", "26", "10.15", or "latest".
pub fn find(s: &str) -> Option<&'static Version> {
    let s = s.trim().to_ascii_lowercase();
    if s == "latest" {
        return VERSIONS.first();
    }
    let s = s.strip_prefix("macos").unwrap_or(&s).trim().to_string();
    VERSIONS.iter().find(|v| {
        v.slug == s
            || v.name.to_ascii_lowercase() == s
            || v.number == s
            || v.slug.replace('-', "") == s.replace([' ', '-'], "")
    })
}

#[derive(Debug, PartialEq)]
pub struct ImageInfo {
    pub product: String,
    pub image_url: String,
    pub image_token: String,
    pub chunklist_url: String,
    pub chunklist_token: String,
}

fn session(agent: &Agent) -> Result<String> {
    let resp = agent
        .get(&format!("{SERVER}/"))
        .config()
        .user_agent(USER_AGENT)
        .build()
        .call()
        .context("connect to osrecovery.apple.com")?;
    for v in resp.headers().get_all("set-cookie") {
        if let Ok(v) = v.to_str()
            && let Some(c) = v
                .split(';')
                .map(str::trim)
                .find(|c| c.starts_with("session="))
        {
            return Ok(c.to_string());
        }
    }
    bail!("osrecovery.apple.com set no session cookie")
}

fn random_hex(n: usize) -> Result<String> {
    let mut b = vec![0u8; n.div_ceil(2)];
    getrandom::fill(&mut b).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    let mut s: String = b.iter().map(|x| format!("{x:02X}")).collect();
    s.truncate(n);
    Ok(s)
}

pub fn image_info(agent: &Agent, v: &Version) -> Result<ImageInfo> {
    let session = session(agent)?;
    let body = format!(
        "cid={}\nsn={}\nbid={}\nk={}\nfg={}\nos={}",
        random_hex(16)?,
        v.mlb,
        v.board_id,
        random_hex(64)?,
        random_hex(64)?,
        v.os
    );
    let mut resp = agent
        .post(&format!("{SERVER}/InstallationPayload/RecoveryImage"))
        .header("Cookie", &session)
        .header("Content-Type", "text/plain")
        .config()
        .user_agent(USER_AGENT)
        .build()
        .send(body)
        .context("ask osrecovery.apple.com for the image")?;
    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .with_config()
        .limit(1 << 16)
        .read_to_string()?;
    if status != 200 {
        bail!(
            "osrecovery.apple.com answered HTTP {status} for {}",
            v.title()
        );
    }
    parse_info(&text)
}

fn parse_info(text: &str) -> Result<ImageInfo> {
    let get = |k: &str| -> Result<String> {
        text.lines()
            .filter_map(|l| l.split_once(": "))
            .find(|(key, _)| key.trim() == k)
            .map(|(_, v)| v.trim().to_string())
            .with_context(|| format!("osrecovery.apple.com's answer has no {k}"))
    };
    Ok(ImageInfo {
        product: get("AP")?,
        image_url: get("AU")?,
        image_token: get("AT")?,
        chunklist_url: get("CU")?,
        chunklist_token: get("CT")?,
    })
}

fn asset_headers(token: &str) -> Headers {
    vec![("Cookie".to_string(), format!("AssetToken={token}"))]
}

/// A version's recovery image as Apple serves it now: its product, and its chunklist, whose
/// signature is checked, and which gives the image's size.
pub struct Image {
    agent: Agent,
    version: Version,
    pub product: String,
    info: ImageInfo,
    pub chunklist: Vec<u8>,
    list: Chunklist,
}

/// Asks Apple for `v`'s recovery image.
pub fn ask(v: &Version, r: &dyn Reporter) -> Result<Image> {
    let agent = http::agent();
    r.step(&format!(
        "Asking Apple for the {} recovery image",
        v.title()
    ));
    let info = image_info(&agent, v)?;
    r.note(&format!("Apple's product {}", info.product));
    let chunklist = get_with_ua(
        &agent,
        &info.chunklist_url,
        &asset_headers(&info.chunklist_token),
    )?;
    let list = Chunklist::parse(&chunklist).context("the recovery image's chunklist")?;
    ensure!(
        list.signature == Signature::Rsa,
        "the recovery image's chunklist is not signed"
    );
    Ok(Image {
        agent,
        version: *v,
        product: info.product.clone(),
        info,
        chunklist,
        list,
    })
}

impl Image {
    pub fn size(&self) -> u64 {
        self.list.total()
    }

    pub fn check(&self) -> ChunkCheck {
        ChunkCheck::new(self.list.clone())
    }

    /// Downloads the image to `dest`, checked by `check` (from [`Image::check`]). An asset token
    /// that runs out on the way is renewed, as long as Apple still serves the same image.
    pub fn download(&self, dest: Dest, check: &mut ChunkCheck, r: &dyn Reporter) -> Result<()> {
        let v = self.version;
        let image_path = url_path(&self.info.image_url).to_string();
        let mut refresh = || -> Result<Headers> {
            let fresh = image_info(&self.agent, &v)?;
            ensure!(
                url_path(&fresh.image_url) == image_path,
                "Apple now serves a different {} recovery ({}): run again to start over",
                v.title(),
                fresh.product
            );
            Ok(asset_headers(&fresh.image_token))
        };
        let d = Download {
            url: self.info.image_url.clone(),
            step: format!("Downloading the {} recovery image", v.title()),
            headers: asset_headers(&self.info.image_token),
            size: Some(self.size()),
            refresh: Some(&mut refresh),
            user_agent: Some(USER_AGENT),
            tries: http::MAX_FAILURES,
        };
        http::download(&self.agent, d, dest, check, r)
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Outputs {
    pub title: String,
    pub product: String,
    pub image: PathBuf,
}

/// Makes `v`'s online installer in `dir`: its recovery image, as
/// `macOS-<name>-online-installer.dmg`. A previous run's download is continued; an image already
/// there is checked, kept if it is Apple's current one, and repaired or replaced if not.
pub fn online_installer(v: &Version, dir: &Path, r: &dyn Reporter) -> Result<Outputs> {
    let image = ask(v, r)?;
    let path = dir.join(v.online_installer_name());
    let mut check = image.check();
    let out = Outputs {
        title: v.title(),
        product: image.product.clone(),
        image: path.clone(),
    };
    if path.exists() {
        if verify_whole(&path, &mut check, r)? {
            r.note(&format!(
                "{} is already here and checks out",
                path.display()
            ));
            return Ok(out);
        }
        // Continued from its last good chunk, which for another product is none.
        std::fs::rename(&path, http::part_path(&path))
            .with_context(|| format!("rename {}", path.display()))?;
    }
    image.download(Dest::File(&path), &mut check, r)?;
    Ok(out)
}

fn get_with_ua(agent: &Agent, url: &str, headers: &Headers) -> Result<Vec<u8>> {
    let mut req = agent.get(url);
    for (k, v) in headers {
        req = req.header(k, v);
    }
    let mut resp = req.config().user_agent(USER_AGENT).build().call()?;
    let status = resp.status().as_u16();
    if status != 200 {
        bail!("{url} answered HTTP {status}");
    }
    Ok(resp
        .body_mut()
        .with_config()
        .limit(16 << 20)
        .read_to_vec()?)
}

/// Checks a whole file against `check`; leaves `check` ready for a fresh download either way.
fn verify_whole(path: &Path, check: &mut ChunkCheck, r: &dyn Reporter) -> Result<bool> {
    use crate::http::Check;
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let good = check.resume(&mut f, 0, len, r)?;
    check.restart_at(0);
    Ok(good == len && len == check.total())
}

fn url_path(url: &str) -> &str {
    url.split(['?', '#']).next().unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_versions_by_any_name() {
        assert_eq!(find("tahoe").map(|v| v.number), Some("26"));
        assert_eq!(find("Big Sur").map(|v| v.slug), Some("big-sur"));
        assert_eq!(find("bigsur").map(|v| v.slug), Some("big-sur"));
        assert_eq!(find("macOS Sonoma").map(|v| v.number), Some("14"));
        assert_eq!(find("10.15").map(|v| v.name), Some("Catalina"));
        assert_eq!(find("latest").map(|v| v.name), Some("Tahoe"));
        assert_eq!(find("27"), None);
    }

    #[test]
    fn parses_the_servers_answer() {
        let text = "AP: 140-93589\nAU: http://oscdn.apple.com/x/BaseSystem.dmg\nAH: 17F6\n\
                    AT: expires=1~access=/x~md5=3\nCU: http://oscdn.apple.com/x/BaseSystem.chunklist\n\
                    CH: 6286\nCT: expires=1~access=/y~md5=e\n";
        let info = parse_info(text).unwrap();
        assert_eq!(info.product, "140-93589");
        assert_eq!(info.image_url, "http://oscdn.apple.com/x/BaseSystem.dmg");
        assert_eq!(info.image_token, "expires=1~access=/x~md5=3");
        assert_eq!(info.chunklist_token, "expires=1~access=/y~md5=e");
        assert!(parse_info("AP: 1\n").is_err());
    }
}
