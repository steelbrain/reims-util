//! The OpenCore boot image: a 128 MiB GPT disk whose EFI system partition holds a pinned
//! OpenCore release, the drivers and ACPI table a VM needs, the picker's icons and the boot chime
//! from a pinned commit of OcBinaryData, the kexts given with `--add-kext` (added.rs), and a
//! `config.plist` made for a q35 VM (config.rs) with an SMBIOS identity of its own (smbios.rs).
//! OVMF goes beside it (ovmf.rs), where reims-ctl looks for it.

pub mod acpi;
pub mod added;
pub mod config;
pub mod smbios;

use crate::cache::{self, Pinned};
use crate::image::{self, Data, Format};
use crate::progress::{self, Reporter};
use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const VERSION: &str = "1.0.8";

const RELEASE: Pinned = Pinned {
    name: "OpenCore-1.0.8-RELEASE.zip",
    url: "https://github.com/acidanthera/OpenCorePkg/releases/download/1.0.8/OpenCore-1.0.8-RELEASE.zip",
    sha256: "2011e8b7216ecb2645d97ea710df965947edd406846e92050b9bc4190be6d27b",
    what: "OpenCore 1.0.8",
};

/// OcBinaryData, which has no releases: GitHub's archive of a commit.
const RESOURCES: Pinned = Pinned {
    name: "OcBinaryData-32500d5e3313f11cad00351ba1c6f9a4aa6cb0f7.zip",
    url: "https://github.com/acidanthera/OcBinaryData/archive/32500d5e3313f11cad00351ba1c6f9a4aa6cb0f7.zip",
    sha256: "1ced15b42352a3a6c1b76ca95951f337fa79899acfb4ed80f329191716623741",
    what: "OpenCore's picker icons and boot chime",
};

/// The archive's folder that `EFI/OC/Resources` takes from.
const RESOURCES_ROOT: &str = "OcBinaryData-32500d5e3313f11cad00351ba1c6f9a4aa6cb0f7/Resources/";

/// The boot chime, the image's only sound.
const CHIME: &str = "Audio/OCEFIAudio_VoiceOver_Boot.mp3";

/// The image's size: room for OpenCore (about 2 MiB of it here) and for what a person may add.
const DISK_SIZE: u64 = 128 << 20;

#[derive(Clone, Debug)]
pub struct Options {
    pub output: PathBuf,
    pub config: config::Options,
    /// Kexts to load, in this order.
    pub add_kexts: Vec<added::Spec>,
    /// Where OVMF's files go, or None to write none.
    pub ovmf_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Outputs {
    pub image: PathBuf,
    pub format: Format,
    pub opencore: &'static str,
    pub smbios: smbios::Smbios,
    /// The kexts the image carries, by their bundle names, in the order they load.
    pub kexts: Vec<String>,
    pub ovmf: Option<crate::ovmf::Outputs>,
}

pub fn build(o: &Options, r: &dyn Reporter) -> Result<Outputs> {
    // Read first, so a kext that can't go in fails before anything is downloaded.
    let added = o
        .add_kexts
        .iter()
        .map(added::read)
        .collect::<Result<Vec<_>>>()?;
    added::check(&added)?;
    let zip_path = cache::fetch(&RELEASE, r)?;
    progress::check(r)?;
    let resources_path = cache::fetch(&RESOURCES, r)?;
    progress::check(r)?;
    r.step(&format!("Building the OpenCore {VERSION} image"));
    let mut files = files(&zip_path, &o.config, &added)?;
    files.extend(resources(&resources_path, o.config.graphical_picker)?);
    let mut kext_names = Vec::new();
    for k in added {
        for (p, d) in k.files {
            files.push((format!("EFI/OC/Kexts/{}/{p}", k.bundle), d));
        }
        kext_names.push(k.bundle);
    }
    let files: Vec<(String, Data)> = files
        .into_iter()
        .map(|(p, d)| (p, Data::Bytes(d)))
        .collect();
    let disk = image::esp_disk(DISK_SIZE, "OPENCORE", &files)?;
    let format = Format::for_path(&o.output);
    image::write(&disk, &o.output, format)?;
    r.note(&format!("Wrote {}", o.output.display()));
    let ovmf = match &o.ovmf_dir {
        Some(dir) => {
            progress::check(r)?;
            let out = crate::ovmf::write(dir, r)?;
            r.note(&format!(
                "Wrote {} and {}",
                out.code.display(),
                out.vars.display()
            ));
            Some(out)
        }
        None => None,
    };
    Ok(Outputs {
        image: o.output.clone(),
        format,
        opencore: VERSION,
        smbios: o.config.smbios.clone(),
        kexts: kext_names,
        ovmf,
    })
}

/// The partition's files, from the release's X64 tree, and the made config.plist and ACPI table.
fn files(
    zip_path: &Path,
    c: &config::Options,
    added: &[added::Kext],
) -> Result<Vec<(String, Vec<u8>)>> {
    let f =
        std::fs::File::open(zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut zip = zip::ZipArchive::new(f).context("read the OpenCore release")?;
    let mut read = |name: &str| -> Result<Vec<u8>> {
        let mut e = zip
            .by_name(name)
            .with_context(|| format!("the OpenCore release has no {name}"))?;
        let mut v = Vec::new();
        e.read_to_end(&mut v)?;
        Ok(v)
    };
    let mut files = Vec::new();
    for p in [
        "EFI/BOOT/BOOTx64.efi",
        "EFI/BOOT/.contentFlavour",
        "EFI/BOOT/.contentVisibility",
        "EFI/OC/OpenCore.efi",
        "EFI/OC/.contentFlavour",
        "EFI/OC/.contentVisibility",
    ] {
        files.push((p.to_string(), read(&format!("X64/{p}"))?));
    }
    for d in config::drivers(c) {
        files.push((
            format!("EFI/OC/Drivers/{d}"),
            read(&format!("X64/EFI/OC/Drivers/{d}"))?,
        ));
    }
    let sample = read("Docs/Sample.plist")?;
    files.push((
        "EFI/OC/config.plist".into(),
        config::make(&sample, c, added)?,
    ));
    files.push((
        format!("EFI/OC/ACPI/{}", acpi::FILE_NAME),
        acpi::ssdt_ec_usbx(),
    ));
    // The release's other folders, for what a person adds.
    for d in ["EFI/OC/Kexts/", "EFI/OC/Resources/", "EFI/OC/Tools/"] {
        files.push((d.to_string(), Vec::new()));
    }
    Ok(files)
}

/// The resources the image carries, from OcBinaryData's archive.
fn resources(zip_path: &Path, graphical: bool) -> Result<Vec<(String, Vec<u8>)>> {
    let f =
        std::fs::File::open(zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut zip = zip::ZipArchive::new(f).context("read OcBinaryData's archive")?;
    let mut files = Vec::new();
    for i in 0..zip.len() {
        let mut e = zip.by_index(i)?;
        let name = e
            .name()
            .context("read OcBinaryData's archive")?
            .into_owned();
        let Some(path) = resource_path(&name, graphical) else {
            continue;
        };
        let mut v = Vec::new();
        e.read_to_end(&mut v)
            .with_context(|| format!("read {name} from OcBinaryData's archive"))?;
        files.push((path, v));
    }
    let chime = format!("EFI/OC/Resources/{CHIME}");
    if !files.iter().any(|(p, _)| *p == chime) {
        bail!("OcBinaryData's archive has no {CHIME}");
    }
    Ok(files)
}

/// Where the archive's entry `name` goes in the image, if it goes there: the chime, and for the
/// graphical picker its fonts, labels and icon set.
fn resource_path(name: &str, graphical: bool) -> Option<String> {
    let rel = name.strip_prefix(RESOURCES_ROOT)?;
    if rel.is_empty() || rel.ends_with('/') {
        return None;
    }
    let icons = format!("Image/{}/", config::PICKER_VARIANT.replace('\\', "/"));
    let take = rel == CHIME
        || (graphical
            && (rel.starts_with("Font/") || rel.starts_with("Label/") || rel.starts_with(&icons)));
    take.then(|| format!("EFI/OC/Resources/{rel}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_taken() {
        let at = |rel: &str| format!("{RESOURCES_ROOT}{rel}");
        let image = |rel: &str| Some(format!("EFI/OC/Resources/{rel}"));
        for rel in [
            CHIME,
            "Font/Font_1x.png",
            "Label/EFIBoot.lbl",
            "Image/Acidanthera/GoldenGate/HardDrive.icns",
        ] {
            assert_eq!(resource_path(&at(rel), true), image(rel), "{rel}");
        }
        assert_eq!(resource_path(&at(CHIME), false), image(CHIME));
        for rel in [
            "Font/Font_1x.png",
            "Label/EFIBoot.lbl",
            "Image/Acidanthera/GoldenGate/HardDrive.icns",
        ] {
            assert_eq!(resource_path(&at(rel), false), None, "{rel}");
        }
        for rel in [
            "",
            "Image/Acidanthera/GoldenGate/",
            "Image/Acidanthera/Syrah/HardDrive.icns",
            "Audio/OCEFIAudio_en_Picker.mp3",
        ] {
            assert_eq!(resource_path(&at(rel), true), None, "{rel}");
        }
        assert_eq!(
            resource_path("OcBinaryData-0/Resources/Font/Font_1x.png", true),
            None
        );
        assert_eq!(
            resource_path(
                &RESOURCES_ROOT.replace("Resources/", "Drivers/HfsPlus.efi"),
                true
            ),
            None
        );
    }
}
