//! A GUID partition table, as UEFI specifies it (UEFI 2.10, section 5.3): a protective MBR, the
//! primary header and its 128 entries at the start of the disk, and their backups at its end.

use super::Data;

pub const SECTOR: u64 = 512;
const ENTRIES: u64 = 128;
const ENTRY_SIZE: u64 = 128;
/// Sectors the 128 entries take.
const ENTRY_SECTORS: u64 = ENTRIES * ENTRY_SIZE / SECTOR;

/// The EFI system partition's type, C12A7328-F81F-11D2-BA4B-00A0C93EC93B.
pub const ESP_TYPE: [u8; 16] = guid(
    0xC12A7328,
    0xF81F,
    0x11D2,
    [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B],
);

/// Microsoft's basic data partition type, EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, which FAT32
/// volumes that are not EFI system partitions take.
pub const BASIC_DATA_TYPE: [u8; 16] = guid(
    0xEBD0A0A2,
    0xB9E5,
    0x4433,
    [0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7],
);

/// Apple's HFS Plus partition type, 48465300-0000-11AA-AA11-00306543ECAC.
pub const HFS_PLUS_TYPE: [u8; 16] = guid(
    0x48465300,
    0x0000,
    0x11AA,
    [0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC],
);

/// A GUID in its on-disk form: the first three fields little-endian.
pub const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [
        a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6],
        d[7],
    ]
}

/// A random (version 4) GUID in on-disk form.
pub fn random_guid() -> anyhow::Result<[u8; 16]> {
    let mut g = [0u8; 16];
    getrandom::fill(&mut g).map_err(|e| anyhow::anyhow!("random: {e}"))?;
    // The version is in the third field's high nibble, which is little-endian on disk.
    g[7] = (g[7] & 0x0f) | 0x40;
    g[8] = (g[8] & 0x3f) | 0x80;
    Ok(g)
}

pub struct Partition {
    pub type_guid: [u8; 16],
    pub guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub name: &'static str,
}

/// The first sector a 1 MiB-aligned partition starts at.
pub const FIRST_LBA: u64 = 2048;

/// The last sector a partition may use on a disk of `sectors` sectors.
pub fn last_usable(sectors: u64) -> u64 {
    sectors - 1 - ENTRY_SECTORS - 1
}

/// The table of a disk of `sectors` 512-byte sectors holding `parts`, as the sectors it takes:
/// the protective MBR, the primary header and entries, and their backups.
pub fn table(sectors: u64, disk_guid: [u8; 16], parts: &[Partition]) -> Vec<(u64, Data)> {
    let last = sectors - 1;

    // The protective MBR: one partition of type 0xEE over the whole disk.
    let mut mbr = vec![0u8; SECTOR as usize];
    let e = &mut mbr[446..462];
    e[1..4].copy_from_slice(&[0x00, 0x02, 0x00]);
    e[4] = 0xee;
    e[5..8].copy_from_slice(&[0xff, 0xff, 0xff]);
    e[8..12].copy_from_slice(&1u32.to_le_bytes());
    e[12..16].copy_from_slice(&((sectors - 1).min(u32::MAX as u64) as u32).to_le_bytes());
    mbr[510] = 0x55;
    mbr[511] = 0xaa;

    let mut entries = vec![0u8; (ENTRIES * ENTRY_SIZE) as usize];
    for (e, p) in entries.chunks_mut(ENTRY_SIZE as usize).zip(parts) {
        e[0..16].copy_from_slice(&p.type_guid);
        e[16..32].copy_from_slice(&p.guid);
        e[32..40].copy_from_slice(&p.first_lba.to_le_bytes());
        e[40..48].copy_from_slice(&p.last_lba.to_le_bytes());
        for (i, u) in p.name.encode_utf16().take(36).enumerate() {
            e[56 + i * 2..58 + i * 2].copy_from_slice(&u.to_le_bytes());
        }
    }
    let entries_crc = crc32fast::hash(&entries);

    let header = |my_lba: u64, alternate: u64, entries_lba: u64| -> Vec<u8> {
        let mut h = vec![0u8; SECTOR as usize];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alternate.to_le_bytes());
        h[40..48].copy_from_slice(&(2 + ENTRY_SECTORS).to_le_bytes());
        h[48..56].copy_from_slice(&last_usable(sectors).to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid);
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&(ENTRIES as u32).to_le_bytes());
        h[84..88].copy_from_slice(&(ENTRY_SIZE as u32).to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32fast::hash(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    let backup_entries = last - ENTRY_SECTORS;
    vec![
        (0, Data::Bytes(mbr)),
        (SECTOR, Data::Bytes(header(1, last, 2))),
        (2 * SECTOR, Data::Bytes(entries.clone())),
        (backup_entries * SECTOR, Data::Bytes(entries)),
        (last * SECTOR, Data::Bytes(header(last, 1, backup_entries))),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esp_type_is_mixed_endian() {
        assert_eq!(
            ESP_TYPE,
            [
                0x28, 0x73, 0x2a, 0xc1, 0x1f, 0xf8, 0xd2, 0x11, 0xba, 0x4b, 0x00, 0xa0, 0xc9, 0x3e,
                0xc9, 0x3b
            ]
        );
    }

    #[test]
    fn headers_carry_valid_crcs() {
        let size = 8u64 << 20;
        let sectors = size / SECTOR;
        let part = |first_lba, last_lba| Partition {
            type_guid: BASIC_DATA_TYPE,
            guid: [2; 16],
            first_lba,
            last_lba,
            name: "Data",
        };
        let disk = super::super::Disk {
            size,
            pieces: table(
                sectors,
                [1; 16],
                &[part(FIRST_LBA, 4095), part(4096, last_usable(sectors))],
            ),
        }
        .to_bytes()
        .unwrap();
        for (lba, entries) in [(1, 2), (sectors - 1, sectors - 1 - ENTRY_SECTORS)] {
            let h = &disk[(lba * SECTOR) as usize..(lba * SECTOR) as usize + 92];
            assert_eq!(&h[0..8], b"EFI PART");
            let mut z = h.to_vec();
            z[16..20].fill(0);
            assert_eq!(crc32fast::hash(&z).to_le_bytes(), h[16..20]);
            let e =
                &disk[(entries * SECTOR) as usize..((entries + ENTRY_SECTORS) * SECTOR) as usize];
            assert_eq!(crc32fast::hash(e).to_le_bytes(), h[88..92]);
            assert_eq!(&e[128 + 32..128 + 40], &4096u64.to_le_bytes());
        }
        assert_eq!(&disk[510..512], &[0x55, 0xaa]);
    }
}
