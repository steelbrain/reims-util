//! Disk images: GPT disks of FAT32 and HFS Plus volumes holding given files, written as a raw
//! image or as qcow2.
//!
//! A disk is made as the byte ranges it holds ([`Disk`]): the volumes' structures and small files
//! in memory, and room for big files that are downloaded into the image where they belong
//! ([`Data::Reserved`], [`open_raw`]), so an installer image of up to 20 GB is never in memory,
//! or on disk twice.

pub mod fat32;
pub mod gpt;
pub mod hfsplus;
pub mod qcow2;

use anyhow::{Context, Result, bail, ensure};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Raw,
    Qcow2,
}

impl Format {
    /// qcow2 for a `.qcow2` path, raw for any other (`.img`, `.raw`).
    pub fn for_path(p: &Path) -> Format {
        match p.extension().and_then(|e| e.to_str()) {
            Some(e) if e.eq_ignore_ascii_case("qcow2") => Format::Qcow2,
            _ => Format::Raw,
        }
    }
}

/// What a file on a volume holds: bytes, or room for `len` bytes written into the image later,
/// such as a download, found by `name` ([`Disk::place`]).
#[derive(Clone, Debug)]
pub enum Data {
    Bytes(Vec<u8>),
    Reserved { name: &'static str, len: u64 },
}

impl Data {
    pub fn size(&self) -> u64 {
        match self {
            Data::Bytes(b) => b.len() as u64,
            Data::Reserved { len, .. } => *len,
        }
    }
}

/// A disk image of `size` bytes: what lies at each offset; the rest is zero.
pub struct Disk {
    pub size: u64,
    pub pieces: Vec<(u64, Data)>,
}

impl Disk {
    fn check(&self) -> Result<()> {
        for (at, d) in &self.pieces {
            ensure!(
                at.checked_add(d.size()).is_some_and(|end| end <= self.size),
                "a piece at {at} runs past the disk's end"
            );
        }
        Ok(())
    }

    /// Where the reserved file `name` starts on the disk.
    pub fn place(&self, name: &str) -> Option<u64> {
        self.pieces.iter().find_map(|(at, d)| match d {
            Data::Reserved { name: n, .. } if *n == name => Some(*at),
            _ => None,
        })
    }

    /// The whole disk in memory, for a small one; reserved files are zero.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        self.check()?;
        let mut disk = vec![0u8; usize::try_from(self.size)?];
        for (at, d) in &self.pieces {
            if let Data::Bytes(b) = d {
                disk[*at as usize..*at as usize + b.len()].copy_from_slice(b);
            }
        }
        Ok(disk)
    }

    /// Writes the disk's bytes into `f`, leaving reserved files as they are.
    fn write_bytes(&self, f: &mut File) -> Result<()> {
        for (at, d) in &self.pieces {
            if let Data::Bytes(b) = d {
                f.seek(SeekFrom::Start(*at))?;
                f.write_all(b)?;
            }
        }
        Ok(())
    }
}

/// A disk of `size` bytes with an EFI system partition from 1 MiB to the end, formatted FAT32
/// (fat32.rs; FAT32 needs 65525 clusters at least, 32 MiB of 512-byte ones), holding `files` at
/// their slash-separated paths; a path ending in `/` is an empty folder.
pub fn esp_disk(size: u64, label: &str, files: &[(String, Data)]) -> Result<Disk> {
    if size < 40 << 20 || !size.is_multiple_of(gpt::SECTOR) {
        bail!("an EFI system partition disk needs 40 MiB at least, in whole sectors");
    }
    let sectors = size / gpt::SECTOR;
    let first = gpt::FIRST_LBA;
    let last = gpt::last_usable(sectors);
    let volume = fat32::make(
        (last + 1 - first) * gpt::SECTOR,
        first as u32,
        label,
        serial()?,
        files,
        fat32::dos_time(std::time::SystemTime::now()),
    )?;
    let mut pieces = gpt::table(
        sectors,
        gpt::random_guid()?,
        &[gpt::Partition {
            type_guid: gpt::ESP_TYPE,
            guid: gpt::random_guid()?,
            first_lba: first,
            last_lba: last,
            name: "EFI System Partition",
        }],
    );
    pieces.extend(
        volume
            .into_iter()
            .map(|(at, d)| (first * gpt::SECTOR + at, d)),
    );
    Ok(Disk { size, pieces })
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fs {
    Fat32,
    HfsPlus,
}

/// A volume for [`data_disk`]: its file system, volume label, partition name, and files.
pub struct Volume<'a> {
    pub fs: Fs,
    pub label: &'a str,
    pub name: &'static str,
    pub files: &'a [(String, Data)],
}

/// A GPT disk of `volumes`, in order, each a partition just big enough for its files and
/// starting on a MiB boundary: a basic data partition for FAT32, an Apple HFS one for HFS Plus.
pub fn data_disk(volumes: &[Volume]) -> Result<Disk> {
    const MIB: u64 = 1 << 20;
    let sizes: Vec<u64> = volumes.iter().map(|v| fit(v.files)).collect();
    // The first MiB for the primary table, and one at the end for the backup.
    let size = MIB + sizes.iter().sum::<u64>() + MIB;
    let clock = std::time::SystemTime::now();
    let now = fat32::dos_time(clock);
    let mut pieces = Vec::new();
    let mut parts = Vec::new();
    let mut at = MIB;
    for (v, &len) in volumes.iter().zip(&sizes) {
        let first = at / gpt::SECTOR;
        let volume = match v.fs {
            Fs::Fat32 => fat32::make(len, u32::try_from(first)?, v.label, serial()?, v.files, now)?,
            Fs::HfsPlus => hfsplus::make(len, v.label, volume_id()?, v.files, clock)?,
        };
        pieces.extend(volume.into_iter().map(|(o, d)| (at + o, d)));
        parts.push(gpt::Partition {
            type_guid: match v.fs {
                Fs::Fat32 => gpt::BASIC_DATA_TYPE,
                Fs::HfsPlus => gpt::HFS_PLUS_TYPE,
            },
            guid: gpt::random_guid()?,
            first_lba: first,
            last_lba: first + len / gpt::SECTOR - 1,
            name: v.name,
        });
        at += len;
    }
    pieces.extend(gpt::table(size / gpt::SECTOR, gpt::random_guid()?, &parts));
    Ok(Disk { size, pieces })
}

/// A volume size that holds `files`: their bytes, each rounded up to 128 KiB (more than either
/// file system here rounds a file to), plus 1/64 for the FATs (FAT32's two, of 512-byte
/// clusters, take 1/64 of a small volume) and 16 MiB for the rest (HFS Plus's B-trees take
/// 1.5 MiB), in whole MiB, 64 MiB at least.
fn fit(files: &[(String, Data)]) -> u64 {
    let data: u64 = files
        .iter()
        .map(|(_, d)| d.size().next_multiple_of(128 << 10))
        .sum();
    (data + data / 64 + (16 << 20))
        .max(64 << 20)
        .next_multiple_of(1 << 20)
}

/// A random identifier for an HFS Plus volume, which macOS makes its volume UUID from.
fn volume_id() -> Result<[u8; 8]> {
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(id)
}

fn serial() -> Result<u32> {
    let mut s = [0u8; 4];
    getrandom::fill(&mut s).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    Ok(u32::from_le_bytes(s))
}

/// Writes `disk` to `path` in `format`, through a `.part` file renamed over it. A raw image is
/// sparse where the disk is zero.
pub fn write(disk: &Disk, path: &Path, format: Format) -> Result<()> {
    disk.check()?;
    create_parent(path)?;
    let part = crate::http::part_path(path);
    let mut f = File::create(&part).with_context(|| format!("create {}", part.display()))?;
    match format {
        Format::Qcow2 => {
            let bytes = disk.to_bytes()?;
            let mut w = std::io::BufWriter::new(&mut f);
            qcow2::write(&bytes, &mut w)?;
            w.flush()?;
        }
        Format::Raw => {
            f.set_len(disk.size)?;
            disk.write_bytes(&mut f)?;
        }
    }
    f.sync_all()?;
    drop(f);
    std::fs::rename(&part, path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Opens `path` as a raw image of `disk` whose reserved files are written next, and writes the
/// rest. What a previous run left in the file stays, sized to the disk, so downloads into the
/// reserved places resume as far as their checks pass it; the structures written over it fill
/// their sectors and clusters, so none keeps an earlier layout's bytes.
pub fn open_raw(disk: &Disk, path: &Path) -> Result<File> {
    disk.check()?;
    create_parent(path)?;
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    f.set_len(disk.size)?;
    disk.write_bytes(&mut f)?;
    Ok(f)
}

fn create_parent(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esp_disk_is_gpt_around_fat32() {
        let files = vec![(
            "EFI/OC/config.plist".to_string(),
            Data::Bytes(vec![7u8; 5000]),
        )];
        let disk = esp_disk(64 << 20, "OPENCORE", &files)
            .unwrap()
            .to_bytes()
            .unwrap();
        assert_eq!(&disk[512..520], b"EFI PART");
        let vol = (gpt::FIRST_LBA * gpt::SECTOR) as usize;
        assert_eq!(&disk[vol + 82..vol + 90], b"FAT32   ");
        // The volume's hidden sectors are where the partition starts.
        assert_eq!(&disk[vol + 28..vol + 32], &2048u32.to_le_bytes());
    }

    #[test]
    fn data_disk_lays_volumes_end_to_end() {
        let fat_files = vec![("a/b.txt".to_string(), Data::Bytes(vec![1; 100]))];
        let hfs_files = vec![("c.bin".to_string(), Data::Bytes(vec![2; 70_000]))];
        let disk = data_disk(&[
            Volume {
                fs: Fs::Fat32,
                label: "RECOVERY",
                name: "one",
                files: &fat_files,
            },
            Volume {
                fs: Fs::HfsPlus,
                label: "Installer",
                name: "two",
                files: &hfs_files,
            },
        ])
        .unwrap();
        // 1 MiB, two 64 MiB volumes, 1 MiB.
        assert_eq!(disk.size, 130 << 20);
        let bytes = disk.to_bytes().unwrap();
        assert_eq!(&bytes[(1 << 20) + 82..(1 << 20) + 90], b"FAT32   ");
        assert_eq!(&bytes[(65 << 20) + 1024..(65 << 20) + 1026], b"H+");
        let entry = |i: usize| &bytes[1024 + i * 128..1024 + (i + 1) * 128];
        let u64_at = |e: &[u8], o: usize| u64::from_le_bytes(e[o..o + 8].try_into().unwrap());
        assert_eq!(u64_at(entry(0), 32), 2048);
        assert_eq!(u64_at(entry(1), 32), 65 << 11);
        assert_eq!(u64_at(entry(1), 40), (129 << 11) - 1);
        assert_eq!(&entry(0)[..16], &gpt::BASIC_DATA_TYPE);
        assert_eq!(&entry(1)[..16], &gpt::HFS_PLUS_TYPE);
    }

    #[test]
    fn raw_images_keep_what_reserved_places_hold() {
        let dir = std::env::temp_dir().join(format!("reims-util-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let disk = Disk {
            size: 32 << 20,
            pieces: vec![
                (512, Data::Bytes(b"head".to_vec())),
                (
                    1 << 20,
                    Data::Reserved {
                        name: "pkg",
                        len: 5 << 20,
                    },
                ),
            ],
        };
        assert_eq!(disk.place("pkg"), Some(1 << 20));
        let path = dir.join("disk.img");
        let mut f = open_raw(&disk, &path).unwrap();
        f.seek(SeekFrom::Start(1 << 20)).unwrap();
        f.write_all(b"downloaded").unwrap();
        drop(f);
        // Opened again, as a resumed run does: the structures are written again, the
        // download's bytes stay.
        drop(open_raw(&disk, &path).unwrap());
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got.len(), 32 << 20);
        assert_eq!(&got[512..516], b"head");
        assert_eq!(&got[1 << 20..(1 << 20) + 10], b"downloaded");
        // A file of another size is sized to the disk.
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(40 << 20)
            .unwrap();
        drop(open_raw(&disk, &path).unwrap());
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got.len(), 32 << 20);
        assert_eq!(&got[1 << 20..(1 << 20) + 10], b"downloaded");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn format_follows_the_extension() {
        assert_eq!(
            Format::for_path(Path::new("a/OpenCore.qcow2")),
            Format::Qcow2
        );
        assert_eq!(Format::for_path(Path::new("OpenCore.img")), Format::Raw);
    }
}
