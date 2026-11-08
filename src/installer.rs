//! Full macOS installers, from Apple's software update catalog: `InstallAssistant.pkg`, which
//! holds "Install macOS <name>.app" whole, for macOS 11 Big Sur and later.
//!
//! The catalog (an XML plist, gzipped, over HTTPS) lists products. A full installer is a product
//! with `ExtendedMetaInfo > InstallAssistantPackageIdentifiers` and an `InstallAssistant.pkg`
//! package, which has its size, its URL and its chunklist's (`IntegrityDataURL`, the SHA-256 of
//! each 10 MiB). The product's distribution file names the macOS (`<title>`, and `VERSION` and
//! `BUILD` under `<auxinfo>`) and, in its installation check, the board IDs of the Macs it
//! installs on: Intel Macs' are `Mac-` and 16 hex digits, Apple silicon Macs' model names
//! (`J413AP`). A product that names no Intel board installs on Apple silicon alone, as macOS 27
//! does, and is left out.
//!
//! A VM cannot boot the package, so the offline installer ([`offline_installer`]) is a disk
//! image holding it and the recovery image of the same macOS, from which Recovery installs it
//! without the network.

use crate::chunklist::{ChunkCheck, Chunklist};
use crate::http::{self, Check, Dest, Download, Headers};
use crate::image::{self, Data, Fs, Volume};
use crate::progress::{self, Reporter};
use crate::recovery;
use anyhow::{Context, Result, bail};
use plist::Value;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The catalog that lists every release from 10.5 to 26, where x86-64 macOS ends.
pub const CATALOG: &str = "https://swscan.apple.com/content/catalogs/others/index-26-15-14-13-12-10.16-10.15-10.14-10.13-10.12-10.11-10.10-10.9-mountainlion-lion-snowleopard-leopard.merged-1.sucatalog.gz";

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Installer {
    /// Apple's product ID: "142-28081".
    pub product: String,
    /// "macOS Tahoe".
    pub title: String,
    pub version: String,
    pub build: String,
    /// When Apple posted it, as the catalog says: "2026-09-28".
    pub posted: String,
    pub size: u64,
    pub url: String,
    pub integrity_url: Option<String>,
    /// The SHA-1 of the package's table of contents, the catalog's `Digest`.
    pub digest: Option<String>,
}

impl Installer {
    pub fn label(&self) -> String {
        format!("{} {} ({})", self.title, self.version, self.build)
    }

    pub fn offline_installer_name(&self) -> String {
        format!(
            "{}-{}-{}-offline-installer.img",
            self.title.replace(' ', "-"),
            self.version,
            self.build
        )
    }
}

/// A full installer in the catalog, before its distribution file is read.
struct Candidate {
    product: String,
    posted: String,
    size: u64,
    url: String,
    integrity_url: Option<String>,
    digest: Option<String>,
    dist_url: String,
}

/// The installers that run on Intel Macs, newest first.
pub fn list(r: &dyn Reporter) -> Result<Vec<Installer>> {
    let agent = http::agent();
    r.step("Reading Apple's software update catalog");
    let gz = http::get(&agent, CATALOG, &Vec::new(), 64 << 20)?;
    let mut xml = Vec::new();
    std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&gz[..]), &mut xml)
        .context("unpack the software update catalog")?;
    let candidates = parse_catalog(&xml)?;

    r.step("Reading each installer's details");
    let next = AtomicUsize::new(0);
    let found: Mutex<Vec<Installer>> = Mutex::new(Vec::new());
    let failed: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    std::thread::scope(|s| {
        for _ in 0..8 {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(c) = candidates.get(i) else {
                        break;
                    };
                    if r.cancelled() {
                        break;
                    }
                    match http::get(&agent, &c.dist_url, &Vec::new(), 4 << 20) {
                        Ok(d) => {
                            if let Some(info) = parse_dist(&String::from_utf8_lossy(&d))
                                && info.intel
                                && let Ok(mut f) = found.lock()
                            {
                                f.push(Installer {
                                    product: c.product.clone(),
                                    title: info.title,
                                    version: info.version,
                                    build: info.build,
                                    posted: c.posted.clone(),
                                    size: c.size,
                                    url: c.url.clone(),
                                    integrity_url: c.integrity_url.clone(),
                                    digest: c.digest.clone(),
                                });
                            }
                        }
                        Err(e) => {
                            if let Ok(mut f) = failed.lock() {
                                f.get_or_insert(e);
                            }
                        }
                    }
                }
            });
        }
    });
    crate::progress::check(r)?;
    if let Ok(mut f) = failed.lock()
        && let Some(e) = f.take()
    {
        return Err(e.context("read an installer's distribution file"));
    }
    let mut found = found.into_inner().unwrap_or_default();
    found.sort_by(|a, b| {
        version_key(&b.version)
            .cmp(&version_key(&a.version))
            .then(b.posted.cmp(&a.posted))
    });
    Ok(found)
}

fn parse_catalog(xml: &[u8]) -> Result<Vec<Candidate>> {
    let root: Value = plist::from_bytes(xml).context("read the software update catalog")?;
    let products = root
        .as_dictionary()
        .and_then(|d| d.get("Products"))
        .and_then(Value::as_dictionary)
        .context("the catalog lists no products")?;
    let mut out = Vec::new();
    for (id, p) in products {
        let Some(p) = p.as_dictionary() else {
            continue;
        };
        let is_full = p
            .get("ExtendedMetaInfo")
            .and_then(Value::as_dictionary)
            .is_some_and(|e| e.contains_key("InstallAssistantPackageIdentifiers"));
        if !is_full {
            continue;
        }
        let Some(pkg) = p
            .get("Packages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_dictionary)
            .find(|k| {
                k.get("URL")
                    .and_then(Value::as_string)
                    .is_some_and(|u| u.ends_with("/InstallAssistant.pkg"))
            })
        else {
            continue;
        };
        let dists = p.get("Distributions").and_then(Value::as_dictionary);
        let Some(dist_url) = dists
            .and_then(|d| d.get("English").or_else(|| d.get("en")))
            .and_then(Value::as_string)
        else {
            continue;
        };
        let s = |k: &str| pkg.get(k).and_then(Value::as_string).map(str::to_string);
        let posted = match p.get("PostDate") {
            Some(Value::Date(d)) => {
                let st: std::time::SystemTime = (*d).into();
                date_string(st)
            }
            _ => String::new(),
        };
        out.push(Candidate {
            product: id.clone(),
            posted,
            size: pkg
                .get("Size")
                .and_then(Value::as_unsigned_integer)
                .unwrap_or(0),
            url: s("URL").unwrap_or_default(),
            integrity_url: s("IntegrityDataURL"),
            digest: s("Digest"),
            dist_url: dist_url.to_string(),
        });
    }
    Ok(out)
}

#[derive(Debug, PartialEq)]
struct DistInfo {
    title: String,
    version: String,
    build: String,
    intel: bool,
}

fn parse_dist(s: &str) -> Option<DistInfo> {
    let between = |from: &str, open: &str, close: &str| -> Option<String> {
        let i = s.find(from)? + from.len();
        let rest = &s[i..];
        let a = rest.find(open)? + open.len();
        let b = rest[a..].find(close)?;
        Some(rest[a..a + b].trim().to_string())
    };
    let title = between("<title", ">", "</title>").unwrap_or_default();
    let version = between("<key>VERSION</key>", "<string>", "</string>")?;
    let build = between("<key>BUILD</key>", "<string>", "</string>")?;
    Some(DistInfo {
        title,
        version,
        build,
        intel: has_intel_board(s),
    })
}

/// Whether the text names an Intel Mac's board ID: `Mac-` and 16 hex digits.
fn has_intel_board(s: &str) -> bool {
    s.match_indices("Mac-").any(|(i, _)| {
        let id = &s.as_bytes()[i + 4..];
        id.len() >= 16 && id[..16].iter().all(u8::is_ascii_hexdigit)
    })
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// A UTC date as YYYY-MM-DD.
fn date_string(t: std::time::SystemTime) -> String {
    let days = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86400)
        .unwrap_or(0);
    let (y, m, d) = crate::image::fat32::civil(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// An installer by version ("26.7.1", or "26" for the newest 26), build ("25G241"), product ID,
/// name ("tahoe", the newest Tahoe) or "latest".
pub fn find<'a>(list: &'a [Installer], q: &str) -> Option<&'a Installer> {
    let q = q.trim().to_ascii_lowercase();
    if q == "latest" {
        return list.first();
    }
    let q = q.strip_prefix("macos").unwrap_or(&q).trim().to_string();
    // A name matches without its spaces and hyphens: "big sur", "big-sur" and "bigsur".
    let squash = |s: &str| s.replace([' ', '-'], "");
    let name = squash(&q);
    list.iter()
        .find(|i| i.version == q || i.build.to_ascii_lowercase() == q || i.product == q)
        .or_else(|| {
            list.iter().find(|i| {
                i.version.split('.').next() == Some(q.as_str())
                    || i.title
                        .to_ascii_lowercase()
                        .strip_prefix("macos ")
                        .is_some_and(|t| squash(t) == name)
            })
        })
}

/// Checks that the package at `offset` in `f` is the one the catalog lists: a xar archive whose
/// table of contents has the SHA-1 the catalog gives as the package's `Digest` (the xar header's
/// 28 bytes, then the zlib-compressed table of contents, whose SHA-1 also opens the heap).
fn check_toc(f: &mut File, offset: u64, digest: &str) -> Result<()> {
    use sha1::Digest;
    use std::io::{Read, Seek, SeekFrom};
    f.seek(SeekFrom::Start(offset))?;
    let mut h = [0u8; 28];
    f.read_exact(&mut h)?;
    if &h[0..4] != b"xar!" {
        bail!("the installer package is not a package");
    }
    let header_size = u16::from_be_bytes([h[4], h[5]]) as u64;
    let toc_size = u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
    if toc_size > 64 << 20 {
        bail!("the installer package's table of contents is {toc_size} bytes");
    }
    f.seek(SeekFrom::Start(offset + header_size))?;
    let mut toc = vec![0u8; toc_size as usize];
    f.read_exact(&mut toc)?;
    let got = http::hex(&sha1::Sha1::digest(&toc));
    if !got.eq_ignore_ascii_case(digest) {
        bail!(
            "the installer package's table of contents has SHA-1 {got}, the catalog says {digest}"
        );
    }
    Ok(())
}

/// The command that installs macOS from an installer image, in Recovery's Terminal: the image's
/// HFS Plus volume, labelled `Installer`, mounts at `/Volumes/Installer`.
pub const IMAGE_COMMAND: &str = "sh /Volumes/Installer/install-macos.sh";

#[derive(Clone, Debug, serde::Serialize)]
pub struct Outputs {
    pub installer: Installer,
    /// The product of the recovery image it boots, or None for an image made before Apple's
    /// current recovery image came out, which boots the earlier one.
    pub recovery: Option<String>,
    pub image: PathBuf,
}

const PACKAGE: &str = "InstallAssistant.pkg";
const BASE_SYSTEM: &str = "BaseSystem.dmg";

/// Makes `inst`'s offline installer in `dir`: a disk image, which reims-ctl attaches as the VM's
/// installer, named `macOS-<name>-<version>-<build>-offline-installer.img`, of two volumes:
/// - HFS Plus `Installer` (FAT32 holds no file of 4 GiB, and the Recovery of macOS 14 and 15
///   mounts no exFAT volume by itself): `InstallAssistant.pkg`, and
///   `install-macos.sh`, which Recovery's Terminal runs to put the package's Install macOS app on
///   the VM's disk and start the app's `startosinstall`;
/// - FAT32 `RECOVERY`: the recovery image of the same macOS (major version), as
///   `com.apple.recovery.boot/BaseSystem.dmg` and its chunklist, which OpenCore boots, checking
///   Apple's signature, as it would from a USB stick.
///
/// Both files are downloaded straight into their places in the image, checked against their
/// chunklists, so the package is never on disk twice; an interrupted run is continued, and an
/// image already there is checked, and repaired if it needs it.
pub fn offline_installer(inst: &Installer, dir: &Path, r: &dyn Reporter) -> Result<Outputs> {
    let major = inst.version.split('.').next().unwrap_or_default();
    let v = recovery::find(major)
        .with_context(|| format!("no recovery image for macOS {major} to boot the installer"))?;
    let rec = recovery::ask(v, r)?;
    let agent = http::agent();
    let mut pkg_check: Box<dyn Check> = match &inst.integrity_url {
        Some(u) => {
            r.step(&format!("Reading {}'s chunklist", inst.label()));
            let raw = http::get(&agent, u, &Headers::new(), 16 << 20)?;
            let list = Chunklist::parse(&raw).context("the installer's chunklist")?;
            if list.total() != inst.size {
                bail!(
                    "the installer's chunklist covers {} bytes, the catalog says {}",
                    list.total(),
                    inst.size
                );
            }
            Box::new(ChunkCheck::new(list))
        }
        // The size, and the table of contents below, are all there is to check.
        None => Box::new(http::NoCheck),
    };

    let install_files = [
        (
            PACKAGE.to_string(),
            Data::Reserved {
                name: PACKAGE,
                len: inst.size,
            },
        ),
        (
            "install-macos.sh".to_string(),
            Data::Bytes(install_script(inst).into_bytes()),
        ),
    ];
    let recovery_files = [
        (
            format!("com.apple.recovery.boot/{BASE_SYSTEM}"),
            Data::Reserved {
                name: BASE_SYSTEM,
                len: rec.size(),
            },
        ),
        (
            "com.apple.recovery.boot/BaseSystem.chunklist".to_string(),
            Data::Bytes(rec.chunklist.clone()),
        ),
    ];
    // The package's volume first, so its place stays where it is when Apple's recovery image
    // changes size between runs.
    let disk = image::data_disk(&[
        Volume {
            fs: Fs::HfsPlus,
            label: "Installer",
            name: "macOS Installer",
            files: &install_files,
        },
        Volume {
            fs: Fs::Fat32,
            label: "RECOVERY",
            name: "macOS Recovery",
            files: &recovery_files,
        },
    ])?;
    let (Some(pkg_at), Some(dmg_at)) = (disk.place(PACKAGE), disk.place(BASE_SYSTEM)) else {
        bail!("the image has no place for its files");
    };

    let path = dir.join(inst.offline_installer_name());
    let mut out = Outputs {
        installer: inst.clone(),
        recovery: Some(rec.product.clone()),
        image: path.clone(),
    };
    let mut rec_check = rec.check();
    if path.exists() {
        // Read only: a VM may be running from it.
        let mut f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        // The package's place doesn't depend on the recovery image, so it's checked first.
        let package = holds(&mut f, pkg_at, inst.size, pkg_check.as_mut(), r)?
            && inst
                .digest
                .as_deref()
                .is_none_or(|d| check_toc(&mut f, pkg_at, d).is_ok());
        if package {
            if f.metadata()?.len() == disk.size
                && holds(&mut f, dmg_at, rec.size(), &mut rec_check, r)?
            {
                r.note(&format!(
                    "{} is already here and checks out",
                    path.display()
                ));
            } else {
                // Apple has a newer recovery image than the one the image was made with. The
                // image is whole as it is (it was checked when it was made, and OpenCore checks
                // its recovery image against its chunklist as it boots), and a VM may be running
                // from it, so it's left alone.
                r.note(&format!(
                    "{} is already here; its installer checks out, and its recovery image is \
                     an earlier one of Apple's",
                    path.display()
                ));
                out.recovery = None;
            }
            return Ok(out);
        }
        std::fs::rename(&path, http::part_path(&path))
            .with_context(|| format!("rename {}", path.display()))?;
    }

    let part = http::part_path(&path);
    let mut f = image::open_raw(&disk, &part)?;
    let dmg = Dest::Region {
        file: &mut f,
        offset: dmg_at,
        len: rec.size(),
    };
    rec.download(dmg, &mut rec_check, r)?;
    progress::check(r)?;
    let mut d = Download::new(&inst.url, format!("Downloading {}", inst.label()));
    d.size = Some(inst.size);
    let pkg = Dest::Region {
        file: &mut f,
        offset: pkg_at,
        len: inst.size,
    };
    http::download(&agent, d, pkg, pkg_check.as_mut(), r)?;
    if let Some(digest) = &inst.digest {
        check_toc(&mut f, pkg_at, digest)?;
    }
    f.sync_all()?;
    drop(f);
    std::fs::rename(&part, &path).with_context(|| format!("write {}", path.display()))?;
    r.note(&format!("Wrote {}", path.display()));
    Ok(out)
}

/// Whether `len` bytes at `offset` in `f` pass `check` whole (a check that reads nothing passes
/// them); leaves `check` ready for a download from the start.
fn holds(
    f: &mut File,
    offset: u64,
    len: u64,
    check: &mut dyn Check,
    r: &dyn Reporter,
) -> Result<bool> {
    if f.metadata()?.len() < offset + len {
        return Ok(false);
    }
    let good = check.resume(f, offset, len, r)?;
    check.restart_at(0);
    Ok(good == len)
}

/// The script on an installer image that installs its macOS from Recovery's Terminal.
fn install_script(inst: &Installer) -> String {
    format!(
        r#"#!/bin/sh
# Installs {label} from the InstallAssistant.pkg beside this script, with no
# network needed. In macOS Recovery, erase the VM's disk in Disk Utility (APFS), then open
# Utilities > Terminal and run:
#
#   {command}
#
# It installs onto the volume named Macintosh HD, or else Untitled (Disk Utility's name for
# it); for another, give its path after the script's.
#
# It installs the package's Install macOS app onto that volume, then runs the app's
# startosinstall, which restarts the VM into the installer.

set -e
# This script's directory, without dirname, which the Recovery of macOS 11 and 12 lacks.
case $0 in
    */*) here=${{0%/*}} ;;
    *) here=. ;;
esac
here=$(cd "$here" && pwd)
target=${{1:-}}
if [ -z "$target" ]; then
    # Disk Utility names an erased disk Untitled unless told otherwise.
    for v in "/Volumes/Macintosh HD" "/Volumes/Untitled"; do
        if [ -d "$v" ]; then target=$v; break; fi
    done
fi
target=${{target%/}}
if [ -z "$target" ] || [ ! -d "$target" ]; then
    echo "There is no ${{target:-/Volumes/Macintosh HD}}: erase the VM's disk in Disk Utility" >&2
    echo "first, or give the volume to install onto. These are mounted:" >&2
    ls /Volumes >&2
    exit 1
fi

# The package's postinstall script copies the package into the app as $3Applications/...,
# which is the app's folder only when $3, the target, is /. For a volume it is a path beside
# the volume, here on Recovery's small RAM disk, so a link puts it on the volume. (mist-cli,
# installing to a disk image, moves <volume>Applications into <volume>/Applications after.)
link="${{target}}Applications"
mkdir -p "$target/Applications"
ln -sfn "$target/Applications" "$link"
trap 'rm -f "$link"' EXIT

echo "Installing the Install macOS app onto $target, which takes a few minutes"
# CM_BUILD as mist-cli sets it, installing the package anywhere but /.
status=0
CM_BUILD=CM_BUILD installer -pkg "$here/InstallAssistant.pkg" -target "$target" || status=$?
app=
for a in "$target"/Applications/Install\ macOS*.app; do
    if [ -d "$a" ]; then app=$a; fi
done
if [ -z "$app" ] || [ ! -f "$app/Contents/Resources/startosinstall" ]; then
    echo "The package left no Install macOS app in $target/Applications" >&2
    exit 1
fi
shared="$app/Contents/SharedSupport"
if [ ! -f "$shared/SharedSupport.dmg" ]; then
    # The package placed the app, and its postinstall did not copy the package into it, as
    # SharedSupport.dmg: Big Sur's runs with zsh, which Big Sur's Recovery lacks. Do the copy
    # as the postinstall does, from another device.
    echo "The package's postinstall did not finish (installer's status $status): copying the package into the app"
    mkdir -p "$shared"
    chmod 0755 "$shared"
    cp "$here/InstallAssistant.pkg" "$shared/SharedSupport.dmg"
    chflags -h norestricted "$shared/SharedSupport.dmg" 2>/dev/null || true
elif [ "$status" != 0 ]; then
    echo "installer failed (status $status)" >&2
    exit 1
fi
rm -f "$link"
# As mist-cli does: the app's files can come out without the modes they need.
chmod -R 755 "$app"
echo "Starting $app; the VM restarts when it is ready"
exec "$app/Contents/Resources/startosinstall" --agreetolicense --volume "$target"
"#,
        label = inst.label(),
        command = IMAGE_COMMAND,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIST: &str = r#"<installer-gui-script minSpecVersion="2">
    <title>macOS Tahoe</title>
    <script>var boardIds = ['Mac-CFF7D910A743CAAF','Mac-AF89B6D9451A490B']; var models = ['J413AP'];</script>
    <auxinfo>
        <dict>
            <key>BUILD</key>
            <string>25G241</string>
            <key>VERSION</key>
            <string>26.7.1</string>
        </dict>
    </auxinfo></installer-gui-script>"#;

    #[test]
    fn install_script_is_valid_sh() {
        let i = Installer {
            product: "142-28081".into(),
            title: "macOS Tahoe".into(),
            version: "26.7.1".into(),
            build: "25G241".into(),
            posted: "2026-09-28".into(),
            size: 0,
            url: String::new(),
            integrity_url: None,
            digest: None,
        };
        let script = install_script(&i);
        assert!(script.contains("# Installs macOS Tahoe 26.7.1 (25G241) from"));
        assert!(script.contains(IMAGE_COMMAND));
        let mut sh = std::process::Command::new("sh")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut sh.stdin.take().unwrap(), script.as_bytes()).unwrap();
        assert!(sh.wait().unwrap().success());
    }

    /// The script runs in a Recovery with no dirname and no zsh, by its path or by its name from
    /// its own directory: where the package's postinstall copies the package into the app (as
    /// macOS 12's and 26's do, through `<target>Applications`), and where it fails after placing
    /// the app (as Big Sur's does, needing zsh), the app gets the package as SharedSupport.dmg
    /// and the script starts its startosinstall on the volume.
    #[test]
    fn install_script_runs_without_dirname_or_zsh() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let i = Installer {
            product: "001-00001".into(),
            title: "macOS Big Sur".into(),
            version: "11.7.10".into(),
            build: "20G1427".into(),
            posted: "2023-09-21".into(),
            size: 0,
            url: String::new(),
            integrity_url: None,
            digest: None,
        };
        // The tools the script and the fake installer run, and no others.
        let tools = ["mkdir", "ln", "rm", "chmod", "cp", "ls", "cat"];
        let tool = |t: &str| {
            ["/bin", "/usr/bin"]
                .iter()
                .map(|d| Path::new(d).join(t))
                .find(|p| p.is_file())
                .unwrap_or_else(|| panic!("no {t}"))
        };
        // The fake installer places the app; with POSTINSTALL=ok it then copies the package as
        // the postinstall does, through "$3Applications", and with POSTINSTALL=zsh it fails as
        // Big Sur's does in a Recovery without zsh.
        let installer = r#"#!/bin/sh
pkg=$2 target=$4
app="$target/Applications/Install macOS Big Sur.app"
mkdir -p "$app/Contents/Resources"
printf '#!/bin/sh\necho "$@" > "%s/started"\n' "$target" > "$app/Contents/Resources/startosinstall"
chmod 755 "$app/Contents/Resources/startosinstall"
if [ "$POSTINSTALL" = ok ]; then
    ss="${target}Applications/Install macOS Big Sur.app/Contents/SharedSupport"
    mkdir -p "$ss" && cp "$pkg" "$ss/SharedSupport.dmg"
    exit 0
fi
echo "Couldn't posix_spawn: error 2" >&2
exit 1
"#;
        for (n, (postinstall, by_name)) in [("ok", false), ("zsh", false), ("zsh", true)]
            .into_iter()
            .enumerate()
        {
            let root = std::env::temp_dir().join(format!(
                "reims-util-test-install-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            let (bin, here, vol) = (
                root.join("bin"),
                root.join("Installer"),
                root.join("Untitled"),
            );
            for d in [&bin, &here, &vol] {
                std::fs::create_dir_all(d).unwrap();
            }
            for t in tools {
                symlink(tool(t), bin.join(t)).unwrap();
            }
            std::fs::write(bin.join("installer"), installer).unwrap();
            std::fs::set_permissions(
                bin.join("installer"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            std::fs::write(here.join("install-macos.sh"), install_script(&i)).unwrap();
            std::fs::write(here.join(PACKAGE), "the package").unwrap();
            let mut sh = std::process::Command::new("/bin/sh");
            if by_name {
                sh.arg("install-macos.sh").current_dir(&here);
            } else {
                sh.arg(here.join("install-macos.sh")).current_dir(&root);
            }
            let out = sh
                .arg(&vol)
                .env_clear()
                .env("PATH", &bin)
                .env("POSTINSTALL", postinstall)
                .output()
                .unwrap();
            let app = vol.join("Applications/Install macOS Big Sur.app");
            let dmg = std::fs::read_to_string(app.join("Contents/SharedSupport/SharedSupport.dmg"));
            let started = std::fs::read_to_string(vol.join("started"));
            let link_left = root.join("UntitledApplications").symlink_metadata().is_ok();
            let _ = std::fs::remove_dir_all(&root);
            let case = format!("{postinstall}, by name {by_name}: {out:?}");
            assert!(out.status.success(), "{case}");
            assert_eq!(dmg.ok().as_deref(), Some("the package"), "{case}");
            assert_eq!(
                started.ok().as_deref().map(str::trim),
                Some(format!("--agreetolicense --volume {}", vol.display()).as_str()),
                "{case}"
            );
            assert!(!link_left, "{case}");
        }
    }

    #[test]
    fn reads_a_distribution_file() {
        assert_eq!(
            parse_dist(DIST),
            Some(DistInfo {
                title: "macOS Tahoe".into(),
                version: "26.7.1".into(),
                build: "25G241".into(),
                intel: true,
            })
        );
        let arm_only = DIST
            .replace("Mac-CFF7D910A743CAAF", "J414AP")
            .replace("Mac-AF89B6D9451A490B", "J415AP");
        assert!(!parse_dist(&arm_only).unwrap().intel);
        assert!(!has_intel_board("Mac-XYZ"));
    }

    #[test]
    fn finds_installers() {
        let mk = |v: &str, b: &str, t: &str| Installer {
            product: format!("p-{b}"),
            title: t.into(),
            version: v.into(),
            build: b.into(),
            posted: String::new(),
            size: 0,
            url: String::new(),
            integrity_url: None,
            digest: None,
        };
        let list = vec![
            mk("26.7.1", "25G241", "macOS Tahoe"),
            mk("26.7", "25G229", "macOS Tahoe"),
            mk("15.8.1", "24H32", "macOS Sequoia"),
            mk("11.7.11", "20G1443", "macOS Big Sur"),
        ];
        assert_eq!(find(&list, "latest").unwrap().build, "25G241");
        assert_eq!(find(&list, "26.7").unwrap().build, "25G229");
        assert_eq!(find(&list, "26").unwrap().build, "25G241");
        assert_eq!(find(&list, "24h32").unwrap().version, "15.8.1");
        assert_eq!(find(&list, "sequoia").unwrap().version, "15.8.1");
        assert_eq!(find(&list, "macOS Tahoe").unwrap().version, "26.7.1");
        assert!(find(&list, "27").is_none());
        for q in ["big sur", "Big Sur", "big-sur", "bigsur"] {
            assert_eq!(find(&list, q).unwrap().version, "11.7.11", "{q}");
        }
    }

    #[test]
    fn sorts_versions_numerically() {
        assert!(version_key("26.10") > version_key("26.9"));
        assert!(version_key("11.7.10") > version_key("11.7.9"));
    }

    #[test]
    fn formats_dates() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        assert_eq!(date_string(t), "2026-09-21");
    }
}
