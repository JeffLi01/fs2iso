//! Conformance hardening pass for ISOs produced by the isobemak engine.
//!
//! isobemak 0.4.3 writes the PVD numeric fields (volume set size, sequence
//! number, logical block size, path-table size/location) as little-endian
//! only, leaves the big-endian copies zero, and does not write ISO9660 path
//! tables at all (PVD path-table pointers stay 0). Strict readers (pycdlib,
//! and likely some firmware / Windows CDFS paths) reject such images.
//!
//! This pass re-opens the finished image, appends spec-compliant Type-L and
//! Type-M path tables built from the actual directory records, and patches
//! the PVD: both-endian volume fields + path-table pointers + total sectors.
//! Directory blocks and file data are untouched, so existing extents and
//! El Torito entries stay valid.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const SECTOR: usize = 2048;

fn r32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn both16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    buf[off + 2..off + 4].copy_from_slice(&v.to_be_bytes());
}

fn both32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    buf[off + 4..off + 8].copy_from_slice(&v.to_be_bytes());
}

/// Raw records of one directory block: (id, is_dir, extent_lba, data_len).
fn raw_records(img: &[u8], lba: u32, dlen: u32) -> Vec<(Vec<u8>, bool, u32, u32)> {
    let start = lba as usize * SECTOR;
    let end = (start + dlen as usize).min(img.len());
    let mut out = Vec::new();
    let mut pos = start;
    while pos + 33 <= end {
        let ln = img[pos] as usize;
        if ln == 0 {
            break;
        }
        if pos + ln > end {
            break;
        }
        let r = &img[pos..pos + ln];
        let idlen = r[32] as usize;
        out.push((
            r[33..33 + idlen].to_vec(),
            r[25] & 0x02 != 0,
            r32le(r, 2),
            r32le(r, 10),
        ));
        pos += ln;
    }
    out
}

struct Dir {
    id: Vec<u8>, // directory identifier exactly as stored in its record
    lba: u32,
    no: u32, // path-table entry number (1-based), assigned during BFS
    children: Vec<Dir>,
}

fn build(img: &[u8], lba: u32, dlen: u32, depth: usize) -> Result<Dir, String> {
    if depth > 64 {
        return Err("directory nesting too deep for path tables".into());
    }
    let mut kids = Vec::new();
    for (id, is_dir, ext, len) in raw_records(img, lba, dlen) {
        if !is_dir || id == [0x00] || id == [0x01] {
            continue; // ".", "..", files
        }
        kids.push(Dir {
            children: build(img, ext, len, depth + 1)?.children,
            id,
            lba: ext,
            no: 0,
        });
    }
    Ok(Dir {
        id: Vec::new(), // root placeholder
        lba,
        no: 0,
        children: kids,
    })
}

/// Append Type-L/Type-M path tables for the whole tree (root first) and
/// return (L bytes, M bytes).
fn path_table_bytes(root: &mut Dir) -> (Vec<u8>, Vec<u8>) {
    let mut l = Vec::new();
    let mut m = Vec::new();
    let push = |l: &mut Vec<u8>, m: &mut Vec<u8>, id: &[u8], lba: u32, parent: u32| {
        let p16 = parent as u16;
        l.push(id.len() as u8);
        l.push(0);
        l.extend_from_slice(&lba.to_le_bytes());
        l.extend_from_slice(&p16.to_le_bytes());
        l.extend_from_slice(id);
        if id.len() & 1 == 1 {
            l.push(0);
        }
        m.push(id.len() as u8);
        m.push(0);
        m.extend_from_slice(&lba.to_be_bytes());
        m.extend_from_slice(&p16.to_be_bytes());
        m.extend_from_slice(id);
        if id.len() & 1 == 1 {
            m.push(0);
        }
    };
    // entry #1 is the root directory itself (empty identifier)
    push(&mut l, &mut m, &[], root.lba, 1);
    let mut next_no = 2u32;
    let mut level: Vec<&mut Dir> = vec![root];
    while !level.is_empty() {
        let mut nxt: Vec<&mut Dir> = Vec::new();
        for node in level.drain(..) {
            node.children.sort_by(|a, b| a.id.cmp(&b.id));
            for child in node.children.iter_mut() {
                // parent number is the parent's own entry number
                let parent_no = node.no;
                push(&mut l, &mut m, &child.id, child.lba, parent_no);
                child.no = next_no;
                next_no += 1;
                nxt.push(child);
            }
        }
        level = nxt;
    }
    (l, m)
}

/// Rewrite the image with path tables appended and a conformant PVD.
///
/// When `want_efi` is set and an El Torito boot record exists, the validation
/// entry platform is forced to 0xEF (isobemak hardcodes 0x00) with the checksum
/// recomputed, so UEFI firmware accepts the catalog.
pub fn conform(path: &Path, want_efi: bool) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open {}: {}", path.display(), e))?;
    let mut img = Vec::new();
    f.read_to_end(&mut img)
        .map_err(|e| format!("read {}: {}", path.display(), e))?;

    let pvd_off = 16 * SECTOR;
    if img.len() < pvd_off + SECTOR || &img[pvd_off + 1..pvd_off + 6] != b"CD001" {
        return Err(format!("{}: PVD not found at sector 16", path.display()));
    }
    let root_lba = r32le(&img[pvd_off..], 156 + 2);
    let root_dlen = r32le(&img[pvd_off..], 156 + 10);

    let mut root = build(&img, root_lba, root_dlen, 0)?;
    root.no = 1;
    let (l, m) = path_table_bytes(&mut root);
    debug_assert_eq!(l.len(), m.len());

    let old_total = r32le(&img[pvd_off..], 80) as usize;
    let secs_l = l.len().div_ceil(SECTOR);
    let secs_m = m.len().div_ceil(SECTOR);
    let new_total = old_total + secs_l + secs_m;

    // relocate the tables just past the current end of the volume
    let l_lba = old_total as u32;
    let m_lba = (old_total + secs_l) as u32;

    // build the final image bytes
    let mut out = img.clone();
    out.resize(new_total * SECTOR, 0);
    out[l_lba as usize * SECTOR..l_lba as usize * SECTOR + l.len()].copy_from_slice(&l);
    out[m_lba as usize * SECTOR..m_lba as usize * SECTOR + m.len()].copy_from_slice(&m);

    // patch the PVD
    let pvd = &mut out[pvd_off..pvd_off + SECTOR];
    both32(pvd, 80, new_total as u32); // volume space size
    both16(pvd, 120, 1); // volume set size
    both16(pvd, 124, 1); // volume sequence number
    both16(pvd, 128, SECTOR as u16); // logical block size
    both32(pvd, 132, l.len() as u32); // path table size (bytes)
    pvd[140..144].copy_from_slice(&l_lba.to_le_bytes()); // type-L location
    pvd[148..152].copy_from_slice(&m_lba.to_be_bytes()); // type-M location
                                                         // optional duplicate table pointers stay 0

    // force the El Torito validation platform to EFI when booting is requested
    if want_efi {
        if let Some(boot_vd) = (16..old_total).find(|&lba| {
            let sec = &out[lba * SECTOR..(lba + 1) * SECTOR];
            sec[0] == 0 && &sec[1..6] == b"CD001"
        }) {
            let sec = &mut out[boot_vd * SECTOR..(boot_vd + 1) * SECTOR];
            let catalog = r32le(sec, 71) as usize;
            let cat = &mut out[catalog * SECTOR..catalog * SECTOR + 32];
            if cat[0] == 1 && cat[1] == 0x00 {
                cat[1] = 0xEF;
                let sum: u16 = (0..32).step_by(2).filter(|&i| i != 28).fold(0u16, |s, i| {
                    s.wrapping_add(u16::from_le_bytes([cat[i], cat[i + 1]]))
                });
                let ck = 0u16.wrapping_sub(sum);
                cat[28..30].copy_from_slice(&ck.to_le_bytes());
            }
        }
    }

    f.seek(SeekFrom::Start(0))
        .and_then(|_| f.write_all(&out))
        .map_err(|e| format!("write {}: {}", path.display(), e))?;
    f.flush()
        .map_err(|e| format!("flush {}: {}", path.display(), e))?;
    Ok(())
}
