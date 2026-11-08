//! An HFS Plus volume holding files in its root folder, as Apple's Technical Note TN1150 lays it
//! out, with no journal:
//! - block 0: the boot blocks (zero) and, 1024 bytes in, the volume header;
//! - the allocation file, one bit a block;
//! - the extents overflow and attributes files, B-trees of a header node only;
//! - the catalog file, a B-tree of a header node and one leaf: the root folder and its thread,
//!   then each file and its thread;
//! - each file in consecutive blocks, one extent each;
//! - the alternate volume header, 1024 bytes before the volume's end.
//!
//! HFS Plus holds files of 4 GiB or more, such as a macOS installer package (12 to 18 GB), and
//! macOS Recovery mounts it by itself on every release from 11 to 26, where the Recovery that
//! macOS 14's and 15's installer images boot mounts neither exFAT nor FAT32.

use super::Data;
use anyhow::{Result, ensure};

const BLOCK: u64 = 4096;
const VOLUME_HEADER: u64 = 1024;
/// The catalog's and the extents file's node size; the attributes file's is twice it, as
/// newfs_hfs makes them.
const NODE: usize = 4096;
/// Each B-tree's nodes: room for what macOS adds when it mounts the volume writable (its
/// private folders, .fseventsd), as newfs_hfs gives a small volume.
const CATALOG_NODES: u32 = 128;
const EXTENTS_NODES: u32 = 128;
const ATTRIBUTES_NODES: u32 = 64;

const ROOT_PARENT: u32 = 1;
const ROOT_FOLDER: u32 = 2;
const FIRST_USER_ID: u32 = 16;

const UNMOUNTED: u32 = 1 << 8;
/// The B-trees' unused nodes are zero.
const UNUSED_NODES_ZEROED: u32 = 1 << 31;
const BIG_KEYS: u32 = 2;
const VARIABLE_INDEX_KEYS: u32 = 4;
const CASE_FOLDING: u8 = 0xcf;

const FOLDER_RECORD: u16 = 1;
const FILE_RECORD: u16 = 2;
const FOLDER_THREAD: u16 = 3;
const FILE_THREAD: u16 = 4;
const THREAD_EXISTS: u16 = 2;
/// Seconds from HFS Plus's epoch, 1904-01-01 UTC, to the Unix epoch.
const EPOCH_1904: u64 = 2_082_844_800;
/// The owner and group of files on removable media: unknown.
const UNKNOWN_ID: u32 = 99;

/// A volume of `size` bytes (a whole partition) holding `files` in its root folder, as byte
/// ranges from the volume's start. `label` is the volume's name; `id` the 64-bit identifier
/// macOS takes its volume UUID from; `now` the time every date is set to.
pub fn make(
    size: u64,
    label: &str,
    id: [u8; 8],
    files: &[(String, Data)],
    now: std::time::SystemTime,
) -> Result<Vec<(u64, Data)>> {
    ensure!(
        size.is_multiple_of(BLOCK) && size >= 4 << 20,
        "an HFS Plus volume is 4 MiB at least, in whole blocks"
    );
    let total =
        u32::try_from(size / BLOCK).map_err(|_| anyhow::anyhow!("the volume is too big"))?;
    let label = name(label, "volume")?;
    let mut names = Vec::with_capacity(files.len());
    for (n, _) in files {
        names.push(name(n, "file")?);
    }
    for (i, a) in names.iter().enumerate() {
        ensure!(
            names[..i].iter().all(|b| fold(b) != fold(a)),
            "two files are named {:?} but for case",
            files[i].0
        );
    }
    let date = u32::try_from(
        now.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            + EPOCH_1904,
    )
    .unwrap_or(u32::MAX);

    // Blocks in order from 1: the allocation file, the three B-trees, then the files.
    let mut next = 1u32;
    let mut alloc = |bytes: u64| -> Result<(u32, u32)> {
        let n = u32::try_from(bytes.div_ceil(BLOCK))?;
        let first = next;
        // The last block holds the alternate volume header.
        ensure!(
            u64::from(first) + u64::from(n) < u64::from(total),
            "the files do not fit the volume"
        );
        next += n;
        Ok((first, n))
    };
    let bitmap_bytes = u64::from(total).div_ceil(8).next_multiple_of(BLOCK);
    let allocation = alloc(bitmap_bytes)?;
    let extents_bytes = u64::from(EXTENTS_NODES) * NODE as u64;
    let extents = alloc(extents_bytes)?;
    let attributes_bytes = u64::from(ATTRIBUTES_NODES) * 2 * NODE as u64;
    let attributes = alloc(attributes_bytes)?;
    let catalog_bytes = u64::from(CATALOG_NODES) * NODE as u64;
    let catalog = alloc(catalog_bytes)?;

    // The files, each in its blocks, given IDs in their order here.
    let mut pieces = Vec::new();
    let mut entries = Vec::new();
    for (i, ((_, data), n)) in files.iter().zip(&names).enumerate() {
        let len = data.size();
        let (first, count) = if len == 0 { (0, 0) } else { alloc(len)? };
        if len != 0 {
            let d = match data {
                Data::Bytes(b) => whole(b.clone(), BLOCK),
                reserved => reserved.clone(),
            };
            pieces.push((u64::from(first) * BLOCK, d));
        }
        let fork = Fork {
            len,
            blocks: count,
            first,
        };
        entries.push((FIRST_USER_ID + i as u32, n, fork));
    }
    let used = next + 1;

    // The catalog's leaf, its records in key order: by parent ID, then by name, case folded.
    let mut records = vec![
        (key(ROOT_PARENT, &label), folder(files.len() as u32, date)),
        (
            key(ROOT_FOLDER, &[]),
            thread(FOLDER_THREAD, ROOT_PARENT, &label),
        ),
    ];
    let mut by_name: Vec<_> = entries.iter().collect();
    by_name.sort_by_key(|(_, n, _)| fold(n));
    for (id, n, fork) in &by_name {
        records.push((key(ROOT_FOLDER, n), file(*id, date, fork)));
    }
    for (id, n, _) in &entries {
        records.push((key(*id, &[]), thread(FILE_THREAD, ROOT_FOLDER, n)));
    }
    let leaf_records = records.len() as u32;
    let leaf = leaf_node(
        &records
            .into_iter()
            .map(|(k, r)| [k, r].concat())
            .collect::<Vec<_>>(),
    )?;

    let mut catalog_file = header_node(&Tree {
        node_size: NODE,
        nodes: CATALOG_NODES,
        max_key: 516,
        attributes: BIG_KEYS | VARIABLE_INDEX_KEYS,
        compare: CASE_FOLDING,
        leaf_records,
    });
    catalog_file.extend(leaf);
    catalog_file.resize(catalog_bytes as usize, 0);
    let mut extents_file = header_node(&Tree {
        node_size: NODE,
        nodes: EXTENTS_NODES,
        max_key: 10,
        attributes: BIG_KEYS,
        compare: 0,
        leaf_records: 0,
    });
    extents_file.resize(extents_bytes as usize, 0);
    let mut attributes_file = header_node(&Tree {
        node_size: 2 * NODE,
        nodes: ATTRIBUTES_NODES,
        max_key: 266,
        attributes: BIG_KEYS | VARIABLE_INDEX_KEYS,
        compare: 0,
        leaf_records: 0,
    });
    attributes_file.resize(attributes_bytes as usize, 0);

    let mut bitmap = vec![0u8; bitmap_bytes as usize];
    for b in (0..next).chain([total - 1]) {
        bitmap[b as usize / 8] |= 0x80 >> (b % 8);
    }

    let header = volume_header(&Header {
        date,
        files: files.len() as u32,
        total,
        free: total - used,
        next_allocation: next,
        next_id: FIRST_USER_ID + files.len() as u32,
        id,
        allocation: fork_of(allocation, bitmap_bytes),
        extents: fork_of(extents, extents_bytes),
        catalog: fork_of(catalog, catalog_bytes),
        attributes: fork_of(attributes, attributes_bytes),
    });
    let at = |(first, _): (u32, u32)| u64::from(first) * BLOCK;
    // Block 0 whole, boot blocks zero, so no earlier image's bytes stay in it.
    let mut block0 = vec![0u8; BLOCK as usize];
    block0[VOLUME_HEADER as usize..VOLUME_HEADER as usize + header.len()].copy_from_slice(&header);
    let mut last = vec![0u8; BLOCK as usize];
    last[(BLOCK - VOLUME_HEADER) as usize..(BLOCK - VOLUME_HEADER) as usize + header.len()]
        .copy_from_slice(&header);
    let mut out = vec![
        (0, Data::Bytes(block0)),
        (at(allocation), Data::Bytes(bitmap)),
        (at(extents), Data::Bytes(extents_file)),
        (at(attributes), Data::Bytes(attributes_file)),
        (at(catalog), Data::Bytes(catalog_file)),
        (size - BLOCK, Data::Bytes(last)),
    ];
    out.extend(pieces);
    Ok(out)
}

/// A name as HFS Plus stores it, UTF-16: printable ASCII but `:` and `/`, which are the same
/// in decomposed form, as names must be. That is every name an installer image holds.
fn name(s: &str, what: &str) -> Result<Vec<u16>> {
    ensure!(
        !s.is_empty()
            && s.len() <= 255
            && s.chars()
                .all(|c| (' '..='~').contains(&c) && c != ':' && c != '/'),
        "bad HFS Plus {what} name {s:?}"
    );
    Ok(s.encode_utf16().collect())
}

/// A name as HFS Plus orders names: case folded (for ASCII, A to Z as a to z).
fn fold(name: &[u16]) -> Vec<u16> {
    name.iter()
        .map(|&c| {
            if (0x41..=0x5a).contains(&c) {
                c + 0x20
            } else {
                c
            }
        })
        .collect()
}

fn whole(mut b: Vec<u8>, unit: u64) -> Data {
    b.resize(b.len().next_multiple_of(unit as usize), 0);
    Data::Bytes(b)
}

#[derive(Clone, Copy)]
struct Fork {
    len: u64,
    blocks: u32,
    first: u32,
}

fn fork_of((first, blocks): (u32, u32), len: u64) -> Fork {
    Fork { len, blocks, first }
}

/// HFSPlusForkData: the size, clump size, blocks, and eight extents, the first holding all.
fn fork_data(f: &Fork, clump: u32) -> [u8; 80] {
    let mut d = [0u8; 80];
    d[0..8].copy_from_slice(&f.len.to_be_bytes());
    d[8..12].copy_from_slice(&clump.to_be_bytes());
    d[12..16].copy_from_slice(&f.blocks.to_be_bytes());
    if f.blocks != 0 {
        d[16..20].copy_from_slice(&f.first.to_be_bytes());
        d[20..24].copy_from_slice(&f.blocks.to_be_bytes());
    }
    d
}

struct Header {
    date: u32,
    files: u32,
    total: u32,
    free: u32,
    next_allocation: u32,
    next_id: u32,
    id: [u8; 8],
    allocation: Fork,
    extents: Fork,
    catalog: Fork,
    attributes: Fork,
}

/// HFSPlusVolumeHeader, 512 bytes.
fn volume_header(h: &Header) -> Vec<u8> {
    let mut v = vec![0u8; 512];
    let mut put = |at: usize, b: &[u8]| v[at..at + b.len()].copy_from_slice(b);
    put(0, b"H+");
    put(2, &4u16.to_be_bytes());
    put(4, &(UNMOUNTED | UNUSED_NODES_ZEROED).to_be_bytes());
    put(8, b"10.0");
    // createDate (local time; here UTC), modifyDate, backupDate (never), checkedDate.
    put(16, &h.date.to_be_bytes());
    put(20, &h.date.to_be_bytes());
    put(28, &h.date.to_be_bytes());
    put(32, &h.files.to_be_bytes());
    // folderCount, the root not counted: 0.
    put(40, &(BLOCK as u32).to_be_bytes());
    put(44, &h.total.to_be_bytes());
    put(48, &h.free.to_be_bytes());
    put(52, &h.next_allocation.to_be_bytes());
    put(56, &65536u32.to_be_bytes());
    put(60, &65536u32.to_be_bytes());
    put(64, &h.next_id.to_be_bytes());
    put(68, &1u32.to_be_bytes());
    // encodingsBitmap: MacRoman.
    put(72, &1u64.to_be_bytes());
    // finderInfo[6..8]: the volume's identifier.
    put(104, &h.id);
    put(112, &fork_data(&h.allocation, BLOCK as u32));
    put(192, &fork_data(&h.extents, h.extents.len as u32));
    put(272, &fork_data(&h.catalog, h.catalog.len as u32));
    put(352, &fork_data(&h.attributes, h.attributes.len as u32));
    v
}

struct Tree {
    node_size: usize,
    nodes: u32,
    max_key: u16,
    attributes: u32,
    compare: u8,
    /// Records in its one leaf, node 1; none for a tree of its header node only.
    leaf_records: u32,
}

/// A B-tree's header node: its node descriptor, header record, user data record, and map
/// record, which marks the nodes in use.
fn header_node(t: &Tree) -> Vec<u8> {
    let size = t.node_size;
    let mut n = vec![0u8; size];
    n[8] = 1; // kBTHeaderNode
    n[10..12].copy_from_slice(&3u16.to_be_bytes());
    let has_leaf = t.leaf_records != 0;
    let used = if has_leaf { 2 } else { 1 };
    let mut put = |at: usize, b: &[u8]| n[at..at + b.len()].copy_from_slice(b);
    if has_leaf {
        put(14, &1u16.to_be_bytes()); // treeDepth
        put(16, &1u32.to_be_bytes()); // rootNode
        put(20, &t.leaf_records.to_be_bytes());
        put(24, &1u32.to_be_bytes()); // firstLeafNode
        put(28, &1u32.to_be_bytes()); // lastLeafNode
    }
    put(32, &(size as u16).to_be_bytes());
    put(34, &t.max_key.to_be_bytes());
    put(36, &t.nodes.to_be_bytes());
    put(40, &(t.nodes - used).to_be_bytes());
    put(46, &(t.nodes * size as u32).to_be_bytes());
    // btreeType 0 (an HFS B-tree), then keyCompareType.
    n[51] = t.compare;
    n[52..56].copy_from_slice(&t.attributes.to_be_bytes());
    // The map record, from 248: node 0, and the leaf.
    n[248] = if has_leaf { 0xc0 } else { 0x80 };
    for (i, off) in [14u16, 120, 248, size as u16 - 8].into_iter().enumerate() {
        n[size - 2 - 2 * i..size - 2 * i].copy_from_slice(&off.to_be_bytes());
    }
    n
}

/// A leaf node of `records`, each a key and its data, with the offsets of each and of the free
/// space after them at the node's end.
fn leaf_node(records: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut n = vec![0u8; NODE];
    n[8] = 0xff; // kBTLeafNode, -1
    n[9] = 1; // height
    n[10..12].copy_from_slice(&(records.len() as u16).to_be_bytes());
    let mut offsets = Vec::with_capacity(records.len() + 1);
    let mut at = 14;
    for r in records {
        ensure!(
            at + r.len() + 2 * (records.len() + 1) <= NODE,
            "the catalog's records do not fit one node"
        );
        offsets.push(at as u16);
        n[at..at + r.len()].copy_from_slice(r);
        at += r.len();
    }
    offsets.push(at as u16);
    for (i, off) in offsets.into_iter().enumerate() {
        n[NODE - 2 - 2 * i..NODE - 2 * i].copy_from_slice(&off.to_be_bytes());
    }
    Ok(n)
}

/// HFSPlusCatalogKey: its length, the parent's ID, and the name.
fn key(parent: u32, name: &[u16]) -> Vec<u8> {
    let mut k = Vec::with_capacity(8 + 2 * name.len());
    k.extend((6 + 2 * name.len() as u16).to_be_bytes());
    k.extend(parent.to_be_bytes());
    k.extend((name.len() as u16).to_be_bytes());
    k.extend(name.iter().flat_map(|c| c.to_be_bytes()));
    k
}

/// HFSPlusBSDInfo: owner, group, flags, mode, and the link count.
fn bsd(mode: u16) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&UNKNOWN_ID.to_be_bytes());
    b[4..8].copy_from_slice(&UNKNOWN_ID.to_be_bytes());
    b[10..12].copy_from_slice(&mode.to_be_bytes());
    b[12..16].copy_from_slice(&1u32.to_be_bytes());
    b
}

/// The four dates a catalog record gives first (created, content and attributes modified,
/// accessed); the backup date after them stays 0.
fn dates(date: u32) -> [u8; 20] {
    let mut d = [0u8; 20];
    for i in 0..4 {
        d[i * 4..i * 4 + 4].copy_from_slice(&date.to_be_bytes());
    }
    d
}

/// HFSPlusCatalogFolder for the root folder, 88 bytes.
fn folder(valence: u32, date: u32) -> Vec<u8> {
    let mut r = vec![0u8; 88];
    r[0..2].copy_from_slice(&FOLDER_RECORD.to_be_bytes());
    r[4..8].copy_from_slice(&valence.to_be_bytes());
    r[8..12].copy_from_slice(&ROOT_FOLDER.to_be_bytes());
    r[12..32].copy_from_slice(&dates(date));
    r[32..48].copy_from_slice(&bsd(0o40755));
    r
}

/// HFSPlusCatalogFile, 248 bytes: its data fork in one extent, no resource fork.
fn file(id: u32, date: u32, fork: &Fork) -> Vec<u8> {
    let mut r = vec![0u8; 248];
    r[0..2].copy_from_slice(&FILE_RECORD.to_be_bytes());
    r[2..4].copy_from_slice(&THREAD_EXISTS.to_be_bytes());
    r[8..12].copy_from_slice(&id.to_be_bytes());
    r[12..32].copy_from_slice(&dates(date));
    r[32..48].copy_from_slice(&bsd(0o100644));
    r[88..168].copy_from_slice(&fork_data(fork, 0));
    r
}

/// HFSPlusCatalogThread: the record type, the parent's ID, and the name.
fn thread(kind: u16, parent: u32, name: &[u16]) -> Vec<u8> {
    let mut r = Vec::with_capacity(10 + 2 * name.len());
    r.extend(kind.to_be_bytes());
    r.extend([0, 0]);
    r.extend(parent.to_be_bytes());
    r.extend((name.len() as u16).to_be_bytes());
    r.extend(name.iter().flat_map(|c| c.to_be_bytes()));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn be16(b: &[u8], at: usize) -> u16 {
        u16::from_be_bytes(b[at..at + 2].try_into().unwrap())
    }
    fn be32(b: &[u8], at: usize) -> u32 {
        u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
    }
    fn be64(b: &[u8], at: usize) -> u64 {
        u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
    }

    fn volume(size: u64, files: &[(&str, Data)]) -> Vec<u8> {
        let files: Vec<_> = files
            .iter()
            .map(|(n, d)| (n.to_string(), d.clone()))
            .collect();
        let pieces = make(
            size,
            "Installer",
            [1, 2, 3, 4, 5, 6, 7, 8],
            &files,
            std::time::UNIX_EPOCH,
        )
        .unwrap();
        let mut v = vec![0u8; size as usize];
        for (at, d) in pieces {
            assert!(
                at.is_multiple_of(BLOCK),
                "a piece at {at} is not on a block"
            );
            if let Data::Bytes(b) = d {
                assert!(
                    (b.len() as u64).is_multiple_of(BLOCK),
                    "a piece at {at} is not whole blocks"
                );
                v[at as usize..at as usize + b.len()].copy_from_slice(&b);
            }
        }
        v
    }

    /// The files of a volume `make` wrote, read as macOS would: from the catalog's leaf
    /// through each file record's extent; and checks of the structures on the way.
    fn list(v: &[u8]) -> Vec<(String, Vec<u8>)> {
        let h = &v[1024..1536];
        assert_eq!(&h[0..2], b"H+");
        assert_eq!(
            h,
            &v[v.len() - 1024..v.len() - 512],
            "the alternate header differs"
        );
        let total = be32(h, 44);
        assert_eq!(u64::from(total) * BLOCK, v.len() as u64);
        // The bitmap marks exactly the blocks in use: the header says how many are free.
        let alloc = be32(h, 112 + 16) as usize * BLOCK as usize;
        let bitmap = &v[alloc..alloc + total.div_ceil(8) as usize];
        let used: u32 = (0..total)
            .filter(|b| bitmap[*b as usize / 8] & (0x80 >> (b % 8)) != 0)
            .count() as u32;
        assert_eq!(total - be32(h, 48), used);
        assert!(
            bitmap[0] & 0x80 != 0
                && bitmap[(total as usize - 1) / 8] & (0x80 >> ((total - 1) % 8)) != 0
        );
        let cat = be32(h, 272 + 16) as usize * BLOCK as usize;
        let header = &v[cat..cat + NODE];
        assert_eq!(header[8], 1);
        assert_eq!(be16(header, 32) as usize, NODE);
        assert_eq!(be32(header, 16), 1);
        let leaf = &v[cat + NODE..cat + 2 * NODE];
        assert_eq!(leaf[8], 0xff);
        let n = be16(leaf, 10) as usize;
        assert_eq!(be32(header, 20) as usize, n);
        let offset = |i: usize| be16(leaf, NODE - 2 - 2 * i) as usize;
        let mut keys = Vec::new();
        let mut out = Vec::new();
        for i in 0..n {
            let at = offset(i);
            let klen = be16(leaf, at) as usize;
            let parent = be32(leaf, at + 2);
            let len = be16(leaf, at + 6) as usize;
            let name: Vec<u16> = (0..len).map(|j| be16(leaf, at + 8 + 2 * j)).collect();
            assert_eq!(klen, 6 + 2 * len);
            keys.push((parent, fold(&name)));
            let r = at + 2 + klen;
            if be16(leaf, r) == FILE_RECORD {
                let size = be64(leaf, r + 88) as usize;
                let start = be32(leaf, r + 88 + 16) as usize * BLOCK as usize;
                assert_eq!(
                    u64::from(be32(leaf, r + 88 + 20)),
                    (size as u64).div_ceil(BLOCK)
                );
                out.push((
                    String::from_utf16(&name).unwrap(),
                    v[start..start + size].to_vec(),
                ));
            }
        }
        assert_eq!(offset(n), {
            let last = offset(n - 1);
            let r = last + 2 + be16(leaf, last) as usize;
            r + 10 + 2 * be16(leaf, r + 8) as usize
        });
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "the catalog's keys are out of order");
        out
    }

    #[test]
    fn writes_files_that_read_back() {
        let script = b"#!/bin/sh\necho hi\n".to_vec();
        let pkg: Vec<u8> = (0..70_000u32).map(|i| i as u8).collect();
        let v = volume(
            8 << 20,
            &[
                ("InstallAssistant.pkg", Data::Bytes(pkg.clone())),
                ("install-macos.sh", Data::Bytes(script.clone())),
            ],
        );
        let files = list(&v);
        // In catalog order: "install-macos.sh" sorts before "InstallAssistant.pkg", case
        // folded ('-' before 'a').
        assert_eq!(
            files,
            vec![
                ("install-macos.sh".to_string(), script),
                ("InstallAssistant.pkg".to_string(), pkg)
            ]
        );
        assert_eq!(be32(&v[1024..], 32), 2);
        assert_eq!(be32(&v[1024..], 64), FIRST_USER_ID + 2);
    }

    #[test]
    fn places_a_big_reserved_file_in_one_extent() {
        let len = 17u64 << 30;
        let files = vec![(
            "InstallAssistant.pkg".to_string(),
            Data::Reserved {
                name: "InstallAssistant.pkg",
                len,
            },
        )];
        let size = (len + (64 << 20)).next_multiple_of(1 << 20);
        let pieces = make(size, "Installer", [0; 8], &files, std::time::UNIX_EPOCH).unwrap();
        let (at, _) = pieces
            .iter()
            .find(|(_, d)| matches!(d, Data::Reserved { .. }))
            .unwrap();
        assert!(at.is_multiple_of(BLOCK) && at + len <= size - BLOCK);
        // The catalog: the B-tree whose keys are up to 516 bytes.
        let catalog = pieces
            .iter()
            .find_map(|(a, d)| match d {
                Data::Bytes(b) if *a > 0 && b.len() >= NODE && b[8] == 1 && be16(b, 34) == 516 => {
                    Some(b)
                }
                _ => None,
            })
            .unwrap();
        let leaf = &catalog[NODE..2 * NODE];
        let r = be16(leaf, NODE - 2 - 2 * 2) as usize;
        let r = r + 2 + be16(leaf, r) as usize;
        assert_eq!(be16(leaf, r), FILE_RECORD);
        assert_eq!(be64(leaf, r + 88), len);
        assert_eq!(u64::from(be32(leaf, r + 88 + 16)) * BLOCK, *at);
        assert_eq!(u64::from(be32(leaf, r + 88 + 20)), len / BLOCK);
    }

    #[test]
    fn refuses_names_hfs_plus_would_store_otherwise() {
        let f = |n: &str| vec![(n.to_string(), Data::Bytes(vec![1]))];
        let now = std::time::UNIX_EPOCH;
        assert!(make(8 << 20, "Installer", [0; 8], &f("a:b"), now).is_err());
        assert!(make(8 << 20, "Installer", [0; 8], &f("é"), now).is_err());
        let twins = vec![
            ("A.pkg".to_string(), Data::Bytes(vec![1])),
            ("a.pkg".to_string(), Data::Bytes(vec![1])),
        ];
        assert!(make(8 << 20, "Installer", [0; 8], &twins, now).is_err());
        assert!(
            make(
                8 << 20,
                "Installer",
                [0; 8],
                &f("x".repeat(9000).as_str()),
                now
            )
            .is_err()
        );
    }

    #[test]
    fn refuses_files_that_do_not_fit() {
        let files = vec![("big".to_string(), Data::Bytes(vec![0; 8 << 20]))];
        assert!(make(8 << 20, "Installer", [0; 8], &files, std::time::UNIX_EPOCH).is_err());
    }

    /// Writes a sample volume where REIMS_UTIL_HFS_SAMPLE names, for macOS's fsck_hfs.
    #[test]
    fn sample_for_fsck() {
        let Some(path) = std::env::var_os("REIMS_UTIL_HFS_SAMPLE") else {
            return;
        };
        let v = volume(
            64 << 20,
            &[
                ("InstallAssistant.pkg", Data::Bytes(vec![0x5a; 3 << 20])),
                (
                    "install-macos.sh",
                    Data::Bytes(b"#!/bin/sh\necho installed\n".to_vec()),
                ),
            ],
        );
        std::fs::write(path, v).unwrap();
    }
}
