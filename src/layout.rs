//! ISO9660 (+Joliet, +El Torito) image layout and rendering.
//!
//! Volume layout (all LBAs are 2048-byte sectors):
//!   [0..16)      system area (zeros)
//!   16           Primary Volume Descriptor
//!   17           Supplementary Volume Descriptor (Joliet)   [optional]
//!   18           Boot Record VD (El Torito)                 [optional]
//!   next         Volume Descriptor Set Terminator
//!   next         El Torito boot catalog (1 sector)          [optional]
//!   next         path tables (base L, base M, joliet L, joliet M; each padded to whole sectors)
//!   next         directory-record blocks, base namespace, DFS order
//!   next         directory-record blocks, Joliet namespace, DFS order
//!   next         file contents (each padded to a whole sector)
//!
//! Field offsets follow ECMA-119 and the El Torito 1.0 spec (cross-checked
//! against widely deployed reader implementations).
use std::io::{self, Write};

use crate::timeutil::{iso_record_date, iso_volume_date_digits};
use crate::tree::Tree;

pub const SECTOR: usize = 2048;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ns {
    Base,
    Jol,
}

#[derive(Clone, Copy)]
pub struct PtInfo {
    pub le_lba: u32,
    pub be_lba: u32,
    pub size: usize, // unpadded byte size of one table
}

pub struct ImagePlan {
    pub svd_lba: u32,
    pub boot_rec_lba: u32,
    pub catalog_lba: u32,
    pub term_lba: u32,
    pub pt_base: PtInfo,
    pub pt_jol: PtInfo,
    pub total_sectors: u32,
}

// ---------------------------------------------------------------------------
// small helpers
// ---------------------------------------------------------------------------

fn both16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
    buf[off + 2..off + 4].copy_from_slice(&v.to_be_bytes());
}

fn both32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    buf[off + 4..off + 8].copy_from_slice(&v.to_be_bytes());
}

fn rec_len(id: &[u8]) -> usize {
    33 + id.len() + (id.len() & 1)
}

fn dir_ns_lba(t: &Tree, d: usize, ns: Ns) -> u32 {
    if ns == Ns::Base {
        t.arena[d].base_lba
    } else {
        t.arena[d].jol_lba
    }
}

fn dir_ns_sectors(t: &Tree, d: usize, ns: Ns) -> u32 {
    if ns == Ns::Base {
        t.arena[d].sectors
    } else {
        t.arena[d].jol_sectors
    }
}

fn ns_id(t: &Tree, n: usize, ns: Ns) -> &[u8] {
    if ns == Ns::Base {
        &t.arena[n].base_id
    } else {
        &t.arena[n].joliet_id
    }
}

/// Children of dir `d` sorted by their identifier in namespace `ns`.
fn children_sorted(t: &Tree, d: usize, ns: Ns) -> Vec<usize> {
    let mut kids = t.arena[d].children.clone();
    kids.sort_unstable_by(|&a, &b| ns_id(t, a, ns).cmp(ns_id(t, b, ns)));
    kids
}

/// DFS pre-order of all dirs in namespace child order.
fn dirs_dfs(t: &Tree, ns: Ns) -> Vec<usize> {
    let mut out = Vec::new();
    let mut stack = vec![t.root];
    while let Some(d) = stack.pop() {
        out.push(d);
        let kids = children_sorted(t, d, ns);
        let dirs: Vec<usize> = kids.into_iter().filter(|&c| t.arena[c].is_dir).collect();
        for &c in dirs.iter().rev() {
            stack.push(c);
        }
    }
    out
}

/// ECMA-119 9.1.4: "." is a single 0x00 byte, ".." a single 0x01 byte —
/// structural identifiers, identical in the base and Joliet trees.
fn dot_ids(ns: Ns) -> (&'static [u8], &'static [u8]) {
    let _ = ns;
    (&[0x00], &[0x01])
}

fn ceil_div(a: usize, b: usize) -> usize {
    a.div_ceil(b)
}

// ---------------------------------------------------------------------------
// size planning
// ---------------------------------------------------------------------------

/// Number of sectors needed to pack the given record lengths. Records never
/// straddle a sector boundary; a directory always occupies >= 1 sector.
fn pack_sectors(lens: &[usize]) -> u32 {
    let mut sectors: u32 = 1;
    let mut used = 0usize;
    for &l in lens {
        if l > SECTOR {
            return sectors.saturating_add(1);
        }
        if used + l > SECTOR {
            sectors += 1;
            used = 0;
        }
        used += l;
    }
    sectors
}

/// Per-dir record lengths (dot, dotdot, then each child) in namespace `ns`.
fn dir_record_lens(t: &Tree, d: usize, ns: Ns) -> Vec<usize> {
    let kids = children_sorted(t, d, ns);
    let (dot, dotdot) = dot_ids(ns);
    let mut lens = vec![rec_len(dot), rec_len(dotdot)];
    for c in kids {
        lens.push(rec_len(ns_id(t, c, ns)));
    }
    lens
}

/// Byte size of one path table (identical for the L and M tables of a namespace).
fn path_table_size(t: &Tree, ns: Ns) -> usize {
    let mut size = 0usize;
    let mut level = vec![t.root];
    while !level.is_empty() {
        let mut nxt = Vec::new();
        for &p in &level {
            let id = ns_id(t, p, ns);
            if p == t.root {
                size += 8; // empty identifier
            } else {
                size += 8 + id.len() + (id.len() & 1);
            }
            for c in children_sorted(t, p, ns) {
                if t.arena[c].is_dir {
                    nxt.push(c);
                }
            }
        }
        level = nxt;
    }
    size
}

// ---------------------------------------------------------------------------
// layout: assign LBAs
// ---------------------------------------------------------------------------

pub fn assign_lbas(
    t: &mut Tree,
    boot: Option<usize>,
    use_joliet: bool,
) -> Result<ImagePlan, String> {
    let pt_base_size = path_table_size(t, Ns::Base);
    let pt_jol_size = if use_joliet {
        path_table_size(t, Ns::Jol)
    } else {
        0
    };

    let mut next: u32 = 16; // PVD at LBA 16
    next += 1; // PVD
    let boot_rec_lba = if boot.is_some() {
        let l = next;
        next += 1;
        l
    } else {
        0
    };
    let svd_lba = if use_joliet {
        let l = next;
        next += 1;
        l
    } else {
        0
    };
    let term_lba = next;
    next += 1;
    let catalog_lba = if boot.is_some() {
        let l = next;
        next += 1;
        l
    } else {
        0
    };

    let table_sectors = |size: usize| -> u32 { ceil_div(size, SECTOR).max(1) as u32 };

    let pt_base_le = next;
    next += table_sectors(pt_base_size);
    let pt_base_be = next;
    next += table_sectors(pt_base_size);

    let (pt_jol_le, pt_jol_be) = if use_joliet {
        let a = next;
        next += table_sectors(pt_jol_size);
        let b = next;
        next += table_sectors(pt_jol_size);
        (a, b)
    } else {
        (0, 0)
    };

    // base-namespace directory blocks, DFS order
    for d in dirs_dfs(t, Ns::Base) {
        let lens = dir_record_lens(t, d, Ns::Base);
        let secs = pack_sectors(&lens);
        t.arena[d].base_lba = next;
        t.arena[d].sectors = secs;
        next += secs;
    }
    // joliet-namespace directory blocks, DFS order
    if use_joliet {
        for d in dirs_dfs(t, Ns::Jol) {
            let lens = dir_record_lens(t, d, Ns::Jol);
            let secs = pack_sectors(&lens);
            t.arena[d].jol_lba = next;
            t.arena[d].jol_sectors = secs;
            next += secs;
        }
    }
    // file contents, DFS (base) order. Empty files get an extent LBA but consume
    // no sector (their data length is 0, so nothing is ever read from it).
    for n in dirs_dfs(t, Ns::Base) {
        for c in children_sorted(t, n, Ns::Base) {
            if !t.arena[c].is_dir {
                let size = t.arena[c].size;
                t.arena[c].base_lba = next;
                next += ceil_div(size as usize, SECTOR) as u32;
            }
        }
    }

    let total = next;
    if total == 0 {
        return Err("internal: empty volume".to_string());
    }

    Ok(ImagePlan {
        svd_lba,
        boot_rec_lba,
        catalog_lba,
        term_lba,
        pt_base: PtInfo {
            le_lba: pt_base_le,
            be_lba: pt_base_be,
            size: pt_base_size,
        },
        pt_jol: PtInfo {
            le_lba: pt_jol_le,
            be_lba: pt_jol_be,
            size: pt_jol_size,
        },
        total_sectors: total,
    })
}

// ---------------------------------------------------------------------------
// record / descriptor / table renderers
// ---------------------------------------------------------------------------

/// One directory record (33-byte base + identifier + even padding).
fn make_record(extent: u32, data_len: u32, is_dir: bool, mtime: u64, id: &[u8]) -> Vec<u8> {
    let total = rec_len(id);
    let mut r = vec![0u8; total];
    r[0] = total as u8;
    r[1] = 0;
    both32(&mut r, 2, extent);
    both32(&mut r, 10, data_len);
    r[18..25].copy_from_slice(&iso_record_date(mtime));
    r[25] = if is_dir { 0x02 } else { 0x00 };
    both16(&mut r, 28, 1);
    r[32] = id.len() as u8;
    r[33..33 + id.len()].copy_from_slice(id);
    r
}

/// Full padded directory-record block for dir `d` in namespace `ns`.
fn render_dir_block(t: &Tree, d: usize, ns: Ns) -> Vec<u8> {
    let kids = children_sorted(t, d, ns);
    let (dot, dotdot) = dot_ids(ns);

    struct Item<'a> {
        id: &'a [u8],
        extent: u32,
        len: u32,
        is_dir: bool,
        mtime: u64,
    }

    let self_lba = dir_ns_lba(t, d, ns);
    let self_len = dir_ns_sectors(t, d, ns) * SECTOR as u32;
    let parent = if d == t.root { d } else { t.arena[d].parent };
    let par_lba = dir_ns_lba(t, parent, ns);
    let par_len = dir_ns_sectors(t, parent, ns) * SECTOR as u32;

    let mut items: Vec<Item> = Vec::with_capacity(kids.len() + 2);
    items.push(Item {
        id: dot,
        extent: self_lba,
        len: self_len,
        is_dir: true,
        mtime: t.arena[d].mtime,
    });
    items.push(Item {
        id: dotdot,
        extent: par_lba,
        len: par_len,
        is_dir: true,
        mtime: t.arena[parent].mtime,
    });
    for c in kids {
        let it = if t.arena[c].is_dir {
            Item {
                id: ns_id(t, c, ns),
                extent: dir_ns_lba(t, c, ns),
                len: dir_ns_sectors(t, c, ns) * SECTOR as u32,
                is_dir: true,
                mtime: t.arena[c].mtime,
            }
        } else {
            Item {
                id: ns_id(t, c, ns),
                extent: t.arena[c].base_lba,
                len: t.arena[c].size as u32,
                is_dir: false,
                mtime: t.arena[c].mtime,
            }
        };
        items.push(it);
    }

    let lens: Vec<usize> = items.iter().map(|it| rec_len(it.id)).collect();
    let sectors = pack_sectors(&lens);
    let mut buf = vec![0u8; sectors as usize * SECTOR];

    let mut pos = 0usize;
    for it in &items {
        let rec = make_record(it.extent, it.len, it.is_dir, it.mtime, it.id);
        if pos + rec.len() > buf.len() {
            pos = (pos / SECTOR + 1) * SECTOR; // start next record in a fresh sector
        }
        buf[pos..pos + rec.len()].copy_from_slice(&rec);
        pos += rec.len();
    }
    buf
}

/// Ordered path-table entries: (dir index, identifier, parent number). Order is
/// by hierarchy level; within a level, grouped by parent order and then sorted by
/// identifier (both match what readers expect). Root comes first.
fn path_table_entries(t: &Tree, ns: Ns) -> Vec<(usize, Vec<u8>, u32)> {
    let mut numbers = vec![0u32; t.arena.len()];
    numbers[t.root] = 1;
    let mut entries: Vec<(usize, Vec<u8>, u32)> = Vec::new();
    let mut level = vec![t.root];
    let mut next_no: u32 = 2;
    while !level.is_empty() {
        let mut nxt = Vec::new();
        for &p in &level {
            let id: Vec<u8> = if p == t.root {
                Vec::new()
            } else {
                ns_id(t, p, ns).to_vec()
            };
            let parent_no = if p == t.root {
                1
            } else {
                numbers[t.arena[p].parent]
            };
            entries.push((p, id, parent_no));
            for c in children_sorted(t, p, ns) {
                if t.arena[c].is_dir {
                    numbers[c] = next_no;
                    next_no += 1;
                    nxt.push(c);
                }
            }
        }
        level = nxt;
    }
    entries
}

/// Render the Type L and Type M path tables for one namespace: (L bytes, M bytes).
fn path_table_bytes(t: &Tree, ns: Ns) -> (Vec<u8>, Vec<u8>) {
    let mut l = Vec::new();
    let mut m = Vec::new();
    for (dir, id, parent) in path_table_entries(t, ns) {
        let extent = dir_ns_lba(t, dir, ns);
        let parent16 = parent as u16;
        l.push(id.len() as u8);
        l.push(0);
        l.extend_from_slice(&extent.to_le_bytes());
        l.extend_from_slice(&parent16.to_le_bytes());
        l.extend_from_slice(&id);
        if id.len() & 1 == 1 {
            l.push(0);
        }
        m.push(id.len() as u8);
        m.push(0);
        m.extend_from_slice(&extent.to_be_bytes());
        m.extend_from_slice(&parent16.to_be_bytes());
        m.extend_from_slice(&id);
        if id.len() & 1 == 1 {
            m.push(0);
        }
    }
    debug_assert_eq!(l.len(), m.len());
    (l, m)
}

fn write_pt(out: &mut dyn Write, t: &Tree, ns: Ns, pt: PtInfo) -> io::Result<()> {
    let (l, m) = path_table_bytes(t, ns);
    debug_assert_eq!(l.len(), pt.size);
    let pad = |w: &mut dyn Write, size: usize| -> io::Result<()> {
        let secs = ceil_div(size, SECTOR).max(1);
        write_zeros(w, secs * SECTOR - size)
    };
    out.write_all(&l)?;
    pad(out, l.len())?;
    out.write_all(&m)?;
    pad(out, m.len())?;
    Ok(())
}

/// 34-byte root directory record embedded in a volume descriptor.
fn root_record_bytes(lba: u32, dir_bytes: u32, mtime: u64) -> [u8; 34] {
    let mut r = [0u8; 34];
    r[0] = 34;
    both32(&mut r, 2, lba);
    both32(&mut r, 10, dir_bytes);
    r[18..25].copy_from_slice(&iso_record_date(mtime));
    r[25] = 0x02;
    both16(&mut r, 28, 1);
    r[32] = 1; // "." identifier: one zero byte
    r
}

fn vd_header(buf: &mut [u8; SECTOR], vd_type: u8) {
    buf[0] = vd_type;
    buf[1..6].copy_from_slice(b"CD001");
    buf[6] = 1;
}

/// Shared volume-descriptor core (PVD and SVD).
#[allow(clippy::too_many_arguments)]
fn fill_vd_core(
    buf: &mut [u8; SECTOR],
    vd_type: u8,
    vol_id: &[u8], // 32 bytes: ASCII for PVD, UTF-16BE for SVD
    escape: Option<&[u8]>,
    root_lba: u32,
    root_len: u32,
    root_mtime: u64,
    pt: PtInfo,
    total_sectors: u32,
    now: u64,
) {
    vd_header(buf, vd_type);
    buf[7] = 0; // flags
    buf[8..14].copy_from_slice(b"FS2ISO"); // system id (spaces pad to 32)
    buf[14..40].fill(b' ');
    let vid = &mut buf[40..72];
    vid.fill(0x20);
    let n = vol_id.len().min(32);
    vid[..n].copy_from_slice(&vol_id[..n]);
    both32(buf, 80, total_sectors); // volume space size
    buf[88..120].fill(0);
    if let Some(esc) = escape {
        buf[88..88 + esc.len()].copy_from_slice(esc);
    }
    both16(buf, 120, 1); // volume set size
    both16(buf, 124, 1); // volume sequence number
    both16(buf, 128, SECTOR as u16); // logical block size
    both32(buf, 132, pt.size as u32); // path table size (bytes, unpadded)
    buf[140..144].copy_from_slice(&pt.le_lba.to_le_bytes());
    // optional LE path table location stays 0
    buf[148..152].copy_from_slice(&pt.be_lba.to_be_bytes());
    // optional BE path table location stays 0
    buf[156..190].copy_from_slice(&root_record_bytes(root_lba, root_len, root_mtime));
    // volume set / publisher / preparer / application / copyright / abstract / biblio
    for (off, len) in [
        (190usize, 128usize),
        (318, 128),
        (446, 128),
        (574, 128),
        (702, 37),
        (739, 37),
        (776, 37),
    ] {
        buf[off..off + len].fill(b' ');
    }
    // dates: creation + modification = now; expiration + effective = unspecified
    let d = iso_volume_date_digits(now);
    buf[813..829].fill(b'0');
    buf[813..829].copy_from_slice(&d);
    buf[829] = 0; // gmt offset (0 = UTC)
    buf[830..846].copy_from_slice(&d);
    buf[846] = 0;
    buf[881] = 1; // file structure version
}

fn build_vd(
    vd_type: u8,
    label_ascii: &[u8],
    t: &Tree,
    ns: Ns,
    pt: PtInfo,
    total: u32,
    now: u64,
) -> [u8; SECTOR] {
    let mut buf = [0u8; SECTOR];
    let root_lba = dir_ns_lba(t, t.root, ns);
    let root_len = dir_ns_sectors(t, t.root, ns) * SECTOR as u32;
    match ns {
        Ns::Jol => {
            // Joliet: the volume identifier is UCS-2 (UTF-16BE), max 16 units
            let mut vid_be = [0x00u8; 32];
            let mut i = 0;
            for b in label_ascii.iter().take(16) {
                vid_be[2 * i] = 0;
                vid_be[2 * i + 1] = *b;
                i += 1;
            }
            fill_vd_core(
                &mut buf,
                vd_type,
                &vid_be,
                Some(b"%/@%/C%/E"),
                root_lba,
                root_len,
                t.arena[t.root].mtime,
                pt,
                total,
                now,
            );
        }
        Ns::Base => {
            let mut vid = [0x20u8; 32];
            vid[..label_ascii.len().min(32)]
                .copy_from_slice(&label_ascii[..label_ascii.len().min(32)]);
            fill_vd_core(
                &mut buf,
                vd_type,
                &vid,
                None,
                root_lba,
                root_len,
                t.arena[t.root].mtime,
                pt,
                total,
                now,
            );
        }
    }
    buf
}

fn boot_record_vd(buf: &mut [u8; SECTOR], catalog_lba: u32) {
    vd_header(buf, 0);
    let mut sys = [0u8; 32];
    sys[..23].copy_from_slice(b"EL TORITO SPECIFICATION");
    buf[7..39].copy_from_slice(&sys);
    buf[71..75].copy_from_slice(&catalog_lba.to_le_bytes());
}

fn terminator(buf: &mut [u8; SECTOR]) {
    vd_header(buf, 0xff);
}

/// El Torito boot catalog: validation entry (platform 0xEF = EFI) + initial entry.
fn boot_catalog(buf: &mut [u8; SECTOR], boot_lba: u32, boot_size: u64) {
    buf.fill(0);
    // --- validation entry ---
    buf[0] = 0x01; // header id
    buf[1] = 0xEF; // platform: EFI
    buf[4..10].copy_from_slice(b"FS2ISO");
    buf[0x1e] = 0x55;
    buf[0x1f] = 0xaa;
    // checksum: the sum of all 32 bytes as 16-bit little-endian words must wrap to 0
    let mut sum: u32 = 0;
    for w in buf[..32].chunks_exact(2) {
        sum += u32::from(w[0]) | (u32::from(w[1]) << 8);
    }
    let ck = (0u32.wrapping_sub(sum) & 0xffff) as u16;
    buf[0x1c..0x1e].copy_from_slice(&ck.to_le_bytes());
    // --- initial/default entry ---
    buf[32] = 0x88; // bootable
    buf[33] = 0x00; // media type: no emulation
    let count = ((boot_size + 511) / 512).min(0xFFFF) as u16;
    buf[38..40].copy_from_slice(&count.to_le_bytes()); // sector count
    buf[40..44].copy_from_slice(&boot_lba.to_le_bytes()); // load RBA
}

fn write_zeros(out: &mut dyn Write, mut n: usize) -> io::Result<()> {
    const Z: [u8; 4096] = [0u8; 4096];
    while n > 0 {
        let take = n.min(Z.len());
        out.write_all(&Z[..take])?;
        n -= take;
    }
    Ok(())
}

fn read_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// full render
// ---------------------------------------------------------------------------

pub fn render_image(
    t: &Tree,
    plan: &ImagePlan,
    boot: Option<usize>,
    use_joliet: bool,
    label_ascii: &[u8],
    out: &mut dyn Write,
) -> Result<(), String> {
    let now = read_now();
    let werr = |e: io::Error| -> String { format!("write error: {}", e) };

    write_zeros(out, 16 * SECTOR).map_err(&werr)?; // system area

    let pvd = build_vd(
        1,
        label_ascii,
        t,
        Ns::Base,
        plan.pt_base,
        plan.total_sectors,
        now,
    );
    out.write_all(&pvd).map_err(&werr)?;

    if boot.is_some() {
        let mut bsec = [0u8; SECTOR];
        boot_record_vd(&mut bsec, plan.catalog_lba);
        out.write_all(&bsec).map_err(&werr)?;
    }

    if use_joliet {
        let svd = build_vd(
            2,
            label_ascii,
            t,
            Ns::Jol,
            plan.pt_jol,
            plan.total_sectors,
            now,
        );
        out.write_all(&svd).map_err(&werr)?;
    }

    let mut tsec = [0u8; SECTOR];
    terminator(&mut tsec);
    out.write_all(&tsec).map_err(&werr)?;

    if let Some(boot_node) = boot {
        let mut csec = [0u8; SECTOR];
        boot_catalog(
            &mut csec,
            t.arena[boot_node].base_lba,
            t.arena[boot_node].size,
        );
        out.write_all(&csec).map_err(&werr)?;
    }

    write_pt(out, t, Ns::Base, plan.pt_base).map_err(&werr)?;
    if use_joliet {
        write_pt(out, t, Ns::Jol, plan.pt_jol).map_err(&werr)?;
    }

    for d in dirs_dfs(t, Ns::Base) {
        let blk = render_dir_block(t, d, Ns::Base);
        debug_assert_eq!(blk.len(), t.arena[d].sectors as usize * SECTOR);
        out.write_all(&blk).map_err(&werr)?;
    }
    if use_joliet {
        for d in dirs_dfs(t, Ns::Jol) {
            let blk = render_dir_block(t, d, Ns::Jol);
            debug_assert_eq!(blk.len(), t.arena[d].jol_sectors as usize * SECTOR);
            out.write_all(&blk).map_err(&werr)?;
        }
    }

    for n in dirs_dfs(t, Ns::Base) {
        for c in children_sorted(t, n, Ns::Base) {
            let node = &t.arena[c];
            if node.is_dir {
                continue;
            }
            let src = node.src.as_ref().expect("file node must have a source");
            let mut f =
                std::fs::File::open(src).map_err(|e| format!("cannot open {:?}: {}", src, e))?;
            let copied = io::copy(&mut f, out).map_err(&werr)?;
            if copied != node.size {
                return Err(format!(
                    "file {:?} changed size while packing (was {}, now {})",
                    src, node.size, copied
                ));
            }
            let rem = node.size as usize % SECTOR;
            if rem != 0 {
                write_zeros(out, SECTOR - rem).map_err(&werr)?;
            }
        }
    }

    Ok(())
}
