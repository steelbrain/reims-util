//! OVMF, the UEFI firmware an x86-64 VM boots OpenCore with: its code and its variable store,
//! from Arch Linux's `edk2-ovmf` package, pinned. The package comes from the Arch Linux Archive,
//! or, when that fails, from the archive's mirror in Umeå, which serves the same file.
//!
//! The pinned build is the one the reims-vgpu macOS 26 guest on homelab-ms01 boots: its
//! `OVMF_CODE.4m.fd` and `OVMF_VARS.4m.fd` are those files' bytes. It is built without SMM and
//! without Secure Boot, so it runs on q35 under KVM as reims-ctl starts it. Its licenses
//! (BSD-2-Clause-Patent, with Apache-2.0 and MIT parts) are in the package, and are written
//! beside the firmware.

use crate::cache::{self, Pinned};
use crate::progress::Reporter;
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const VERSION: &str = "edk2-ovmf 202608-1";

const PACKAGE: Pinned = Pinned {
    name: "edk2-ovmf-202608-1-any.pkg.tar.zst",
    // The Arch Linux Archive, then its mirror (Arch's infrastructure docs/servers.md, "Archive
    // Mirrors").
    urls: &[
        "https://archive.archlinux.org/packages/e/edk2-ovmf/edk2-ovmf-202608-1-any.pkg.tar.zst",
        "https://umea.archive.pkgbuild.com/packages/e/edk2-ovmf/edk2-ovmf-202608-1-any.pkg.tar.zst",
    ],
    sha256: "bee34c6036ebd0e96562b7330d2d3ddf75e240cfc44a6d506a5ccb6390eafb91",
    what: "OVMF (Arch Linux's edk2-ovmf 202608-1)",
};

/// The package's files, what each is written as, and its SHA-256.
const FILES: &[(&str, &str, Option<&str>)] = &[
    (
        "usr/share/edk2/x64/OVMF_CODE.4m.fd",
        "OVMF_CODE.fd",
        Some("2febd26c0b4cf95a636a941afa37d64a552723443ed9ac72f763f6840da98cb4"),
    ),
    (
        "usr/share/edk2/x64/OVMF_VARS.4m.fd",
        "OVMF_VARS.fd",
        Some("5d2ac383371b408398accee7ec27c8c09ea5b74a0de0ceea6513388b15be5d1e"),
    ),
    (
        "usr/share/licenses/edk2-ovmf/License.txt",
        "OVMF-License.txt",
        None,
    ),
    (
        "usr/share/licenses/edk2-ovmf/OvmfPkg.License.txt",
        "OVMF-OvmfPkg-License.txt",
        None,
    ),
];

#[derive(Clone, Debug, serde::Serialize)]
pub struct Outputs {
    pub code: PathBuf,
    pub vars: PathBuf,
}

/// Writes `OVMF_CODE.fd` and `OVMF_VARS.fd`, and the firmware's licenses, into `dir`.
pub fn write(dir: &Path, r: &dyn Reporter) -> Result<Outputs> {
    let pkg = cache::fetch(&PACKAGE, r)?;
    r.step("Writing OVMF's code and variables");
    let f = std::fs::File::open(&pkg).with_context(|| format!("open {}", pkg.display()))?;
    let zst = ruzstd::decoding::StreamingDecoder::new(std::io::BufReader::new(f))
        .map_err(|e| anyhow::anyhow!("read {}: {e}", pkg.display()))?;
    let mut tar = tar::Archive::new(zst);
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let mut found = 0;
    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().into_owned();
        let Some((_, out, sha)) = FILES.iter().find(|(p, _, _)| *p == path) else {
            continue;
        };
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        if let Some(sha) = sha {
            use sha2::{Digest, Sha256};
            let got = crate::http::hex(&Sha256::digest(&data));
            if got != *sha {
                bail!("{path} in the OVMF package has SHA-256 {got}, not the pinned {sha}");
            }
        }
        crate::http::write_atomic(&dir.join(out), &data)?;
        found += 1;
    }
    if found != FILES.len() {
        bail!("the OVMF package lacks some of its files");
    }
    Ok(Outputs {
        code: dir.join("OVMF_CODE.fd"),
        vars: dir.join("OVMF_VARS.fd"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_source_is_the_archives_path_to_the_package() {
        let path = format!("/packages/e/edk2-ovmf/{}", PACKAGE.name);
        let hosts: Vec<&str> = PACKAGE
            .urls
            .iter()
            .map(|u| u.strip_suffix(path.as_str()).unwrap())
            .collect();
        assert_eq!(
            hosts,
            [
                "https://archive.archlinux.org",
                "https://umea.archive.pkgbuild.com"
            ]
        );
    }
}
