//! End-to-end tests: build real payloads, run the packer, then parse the ISO
//! back with an *independent* minimal ISO9660 reader and check structure,
//! naming, extents, content and the El Torito catalog.

use fs2iso::{build_iso, Options};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const SECTOR: usize = 2048;
const SECTOR_U32: u32 = 2048;
static COUNTER: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// tiny ISO9660 parser (independent of the writer)
// ---------------------------------------------------------------------------

fn r16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn r32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn r32be(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[derive(Debug, Clone, PartialEq)]
struct Entry {
    /// '/' separated ISO path from the root, using the namespace's decoded name
    path: String,
    is_dir: bool,
    size: u64,
    extent: u32,
    data_len: u32,
}

struct Vd {
    space_size: u32,
    pt_le: u32,
    pt_size: usize,
    root_extent: u32,
    root_len: u32,
    joliet: bool,
    boot_catalog: Option<u32>,
}

/// Read the volume descriptors; returns (PVD-ish struct, SVD root info if joliet).
fn parse_vds(img: &[u8]) -> (Vd, Option<Vd>) {
    let mut pvd = None;
    let mut svd = None;
    let mut boot_catalog = None;
    let mut lba = 16usize;
    loop {
        let sec = &img[lba * SECTOR..(lba + 1) * SECTOR];
        if &sec[1..6] != b"CD001" {
            panic!("bad CD001 at sector {}", lba);
        }
        match sec[0] {
            1 => {
                let rr = &sec[156..190];
                pvd = Some(Vd {
                    space_size: r32le(sec, 80),
                    pt_le: r32le(sec, 140),
                    pt_size: r32le(sec, 132) as usize,
                    root_extent: r32le(rr, 2),
                    root_len: r32le(rr, 10),
                    joliet: false,
                    boot_catalog: None,
                });
                if r32le(sec, 80) != r32be(sec, 84) {
                    panic!("PVD space size endian mismatch");
                }
            }
            2 => {
                let rr = &sec[156..190];
                let esc = &sec[88..91];
                let jol =
                    esc == b"%/@" || esc == b"%/C" || esc == b"%/E" || &sec[88..96] == b"%/@%/C%/E";
                svd = Some(Vd {
                    space_size: r32le(sec, 80),
                    pt_le: r32le(sec, 140),
                    pt_size: r32le(sec, 132) as usize,
                    root_extent: r32le(rr, 2),
                    root_len: r32le(rr, 10),
                    joliet: jol,
                    boot_catalog: None,
                });
            }
            0 => {
                boot_catalog = Some(r32le(sec, 71));
            }
            255 => break,
            _ => panic!("unknown volume descriptor type {}", sec[0]),
        }
        lba += 1;
        if lba > 128 {
            panic!("no terminator descriptor found");
        }
    }
    let p = pvd.expect("no PVD");
    (p, svd)
}

/// Parse the records of one directory block.
fn dir_records(img: &[u8], extent: u32, data_len: u32) -> Vec<Vec<u8>> {
    let start = extent as usize * SECTOR;
    let end = start + data_len as usize;
    let b = &img[start..end.min(img.len())];
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < b.len() {
        let rl = b[pos] as usize;
        if rl == 0 {
            // zero padding: skip to next sector boundary
            pos = ((pos / SECTOR) + 1) * SECTOR;
            continue;
        }
        let id_len = b[pos + 32] as usize;
        let rec_end = pos + rl;
        assert!(rec_end <= b.len(), "record overruns dir block");
        let id = b[pos + 33..pos + 33 + id_len].to_vec();
        out.push(id);
        pos = rec_end;
    }
    out
}

fn decode_id(id: &[u8], joliet: bool) -> Option<String> {
    if id.is_empty() {
        return None;
    }
    // structural dot/dotdot identifiers: "."=0x00, ".."=0x01 (ECMA-119 9.1.4)
    if id == [0x00] || id == [0x01] {
        return None;
    }
    if !joliet {
        // strip ";version"
        let name = match id.iter().position(|&c| c == b';') {
            Some(i) => &id[..i],
            None => id,
        };
        if name.is_empty() {
            return None;
        }
        return Some(String::from_utf8_lossy(name).into_owned());
    }
    let units: Vec<u16> = id
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Walk a whole namespace, returning every entry (dirs and files) with paths.
/// Marks *directory* blocks into `mark`; file data ranges are returned for the
/// caller to validate (base and Joliet namespaces share the same file extents).
fn walk_ns(
    img: &[u8],
    root_ext: u32,
    root_len: u32,
    joliet: bool,
    mark: &mut [bool],
) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut seen_extents: HashSet<u32> = HashSet::new();

    fn rec_walk(
        img: &[u8],
        ext: u32,
        len: u32,
        joliet: bool,
        prefix: String,
        out: &mut Vec<Entry>,
        seen: &mut HashSet<u32>,
        mark: &mut [bool],
    ) {
        if !seen.insert(ext) {
            panic!("directory block visited twice at LBA {}", ext);
        }
        let sectors = (len as usize).div_ceil(SECTOR).max(1);
        for s in 0..sectors {
            let lba = ext as usize + s;
            assert!(!mark[lba], "directory block overlap at LBA {}", lba);
            mark[lba] = true;
        }
        // parse raw records with attributes
        let start = ext as usize * SECTOR;
        let b = &img[start..start + len as usize];
        let mut pos = 0usize;
        while pos < b.len() {
            let rl = b[pos] as usize;
            if rl == 0 {
                pos = ((pos / SECTOR) + 1) * SECTOR;
                continue;
            }
            let id_len = b[pos + 32] as usize;
            let is_dir = b[pos + 25] & 0x02 != 0;
            let child_ext = r32le(b, pos + 2);
            let child_len = r32le(b, pos + 10);
            let id = b[pos + 33..pos + 33 + id_len].to_vec();
            let name = decode_id(&id, joliet);
            let rec_end = pos + rl;
            pos = rec_end;
            let Some(name) = name else { continue };
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{}/{}", prefix, name)
            };
            out.push(Entry {
                path: path.clone(),
                is_dir,
                size: child_len as u64,
                extent: child_ext,
                data_len: child_len,
            });
            if is_dir {
                rec_walk(img, child_ext, child_len, joliet, path, out, seen, mark);
            }
        }
    }

    rec_walk(
        img,
        root_ext,
        root_len,
        joliet,
        String::new(),
        &mut out,
        &mut seen_extents,
        mark,
    );
    out
}

/// Assert that a set of (start_lba, byte_len) ranges are pairwise disjoint and
/// lie inside [0, total).
fn assert_disjoint(ranges: &mut Vec<(u32, u32)>, total: usize, what: &str) {
    ranges.sort_unstable();
    for w in ranges.windows(2) {
        let (s0, l0) = w[0];
        let (s1, _) = w[1];
        assert!(
            s1 >= s0 + l0.div_ceil(SECTOR_U32),
            "{} ranges overlap: {:?} vs {:?}",
            what,
            w[0],
            w[1]
        );
    }
    for &(s, l) in ranges.iter() {
        assert!(
            (s as usize) < total && (s as usize) + l.div_ceil(SECTOR_U32) as usize <= total,
            "{} range {} beyond volume end {}",
            what,
            s,
            total
        );
    }
}

/// Verify a path table (L) against the directories found by walking records.
fn check_path_table(img: &[u8], vd: &Vd, joliet: bool, dirs: &[Entry]) {
    let start = vd.pt_le as usize * SECTOR;
    let mut pos = start;
    let end = pos + vd.pt_size;
    let mut entries: Vec<(Vec<u8>, u32, u32)> = Vec::new(); // (id, parent, extent)
    while pos < end {
        let blk = &img[pos..];
        let id_len = blk[0] as usize;
        let extent = r32le(blk, 2);
        let parent = r16le(blk, 6) as u32;
        let id = blk[8..8 + id_len].to_vec();
        entries.push((id, parent, extent));
        pos += 8 + id_len + (id_len & 1);
    }
    assert_eq!(entries.len(), dirs.len() + 1, "path table dir count"); // + root

    // root entry first, parent = 1, empty id
    assert_eq!(entries[0].0.len(), 0);
    assert_eq!(entries[0].1, 1);
    assert_eq!(entries[0].2, vd.root_extent);

    // parent index always smaller than child index (level ordering)
    for (i, (_, p, _)) in entries.iter().enumerate().skip(1) {
        assert!(
            (*p as usize) >= 1 && (*p as usize) <= i,
            "bad parent number {} at {}",
            p,
            i
        );
    }

    // every dir entry found by walking must appear exactly once, with same extent
    let mut found = 0usize;
    let dir_map: Vec<(&str, u32)> = dirs
        .iter()
        .filter(|e| e.is_dir)
        .map(|e| {
            let name = e.path.rsplit('/').next().unwrap();
            (name, e.extent)
        })
        .collect();
    for (id, _parent, extent) in &entries[1..] {
        let name = decode_id(id, joliet).expect("path table id");
        // the dir's record extent must match a walked dir with that name at some level
        assert!(
            dir_map.iter().any(|(n, e)| *n == name && *e == *extent),
            "path table entry {} @ {} not found in walked dirs",
            name,
            extent
        );
        found += 1;
    }
    assert_eq!(found, dir_map.len());
}

fn read_sector_range(img: &[u8], extent: u32, size: usize) -> &[u8] {
    &img[extent as usize * SECTOR..extent as usize * SECTOR + size]
}

fn check_eltorito(img: &[u8], catalog_lba: u32, expected_boot_extent: u32, expected_size: u64) {
    let cat = &img[catalog_lba as usize * SECTOR..];
    // validation entry
    assert_eq!(cat[0], 0x01);
    assert_eq!(cat[1], 0xEF, "platform must be EFI");
    assert_eq!(cat[0x1e], 0x55);
    assert_eq!(cat[0x1f], 0xaa);
    let mut sum: u32 = 0;
    for w in cat[..32].chunks_exact(2) {
        sum += u32::from(w[0]) | (u32::from(w[1]) << 8);
    }
    assert_eq!(
        sum & 0xffff,
        0,
        "validation entry checksum must wrap to zero"
    );
    // initial entry
    assert_eq!(cat[32], 0x88, "bootable");
    assert_eq!(cat[33], 0x00, "no emulation");
    let count = u16::from_le_bytes([cat[38], cat[39]]);
    let rba = u32::from_le_bytes([cat[40], cat[41], cat[42], cat[43]]);
    assert_eq!(rba, expected_boot_extent);
    assert_eq!(
        count as u64,
        ((expected_size + 511) / 512).min(0xFFFF),
        "sector count"
    );
}

// ---------------------------------------------------------------------------
// payload fixture
// ---------------------------------------------------------------------------

struct Fixture {
    dir: PathBuf,
    /// map joliet path ("pkg/...") -> content
    files: Vec<(String, Vec<u8>)>,
    boot_file: String,
}

fn make_payload() -> Fixture {
    let tag = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("fs2iso-test-{}-{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("pkg/efi/boot")).unwrap();
    fs::create_dir_all(dir.join("pkg/sub dir/deeper")).unwrap();
    fs::create_dir_all(dir.join("pkg/empty")).unwrap();

    let mut files = Vec::new();
    let mut add = |rel: &str, content: Vec<u8>, f: &mut Vec<(String, Vec<u8>)>| {
        let p = dir.join("pkg").join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, &content).unwrap();
        f.push((format!("pkg/{}", rel.replace('\\', "/")), content));
    };

    add("readme.txt", b"hello readme\n".to_vec(), &mut files);
    add("readme2.txt", b"hello readme 2\n".to_vec(), &mut files); // case collision covered by unit tests
    add("startup.nsh", b"echo hello\n".to_vec(), &mut files);
    add("empty.bin", Vec::new(), &mut files);
    // > 1 sector content with a pattern
    let big: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
    add("big file (payload).dat", big, &mut files);
    // chinese name
    add(
        "固件更新日志.txt",
        "中文内容ABC123\n".as_bytes().to_vec(),
        &mut files,
    );
    add(
        "efi/boot/bootx64.efi",
        vec![
            0x4d, 0x5a, 0x90, 0x00, 0x03, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00,
        ],
        &mut files,
    );
    add("tools/diskpart.nsh", b"map -r\n".to_vec(), &mut files);
    add("sub dir/deeper/x.Y", b"deep\n".to_vec(), &mut files);
    // trailing-dot and hidden-style names
    add("version1.0.release", b"v1\n".to_vec(), &mut files);
    add(".hidden", b"hidden\n".to_vec(), &mut files);

    files.sort();
    Fixture {
        dir,
        files,
        boot_file: "pkg/efi/boot/bootx64.efi".to_string(),
    }
}

fn build(img_path: &Path, opts: &Options) {
    let fx = make_payload();
    build_iso(img_path, &[fx.dir.join("pkg")], opts).expect("build_iso failed");
    let _ = fs::remove_dir_all(&fx.dir);
}

fn full_check(img: &[u8], expect_boot: bool, expect_joliet: bool, fixture: &Fixture) {
    let total = r32le(&img[16 * SECTOR..], 80) as usize;
    assert!(total > 16);

    // mark everything that must not overlap
    let mut mark = vec![false; total];
    for s in 0..16 {
        mark[s] = true; // system area
    }
    // descriptors + catalog are sectors 16..~24 (covered by walk of specific lba ranges later)

    let (pvd, svd) = parse_vds(img);
    assert_eq!(pvd.space_size as usize, total);
    assert_eq!(pvd.joliet, false);
    if expect_joliet {
        let svd_ref = svd.as_ref().expect("joliet SVD expected");
        assert!(svd_ref.joliet);
        assert_eq!(svd_ref.space_size as usize, total);
    } else {
        assert!(svd.is_none() || !svd.as_ref().unwrap().joliet);
    }

    // walk base namespace from PVD root record
    let base_entries = walk_ns(img, pvd.root_extent, pvd.root_len, false, &mut mark);
    let base_files: Vec<&Entry> = base_entries.iter().filter(|e| !e.is_dir).collect();

    // walk joliet namespace
    let jol_entries = if expect_joliet {
        let svd = svd.as_ref().unwrap();
        walk_ns(img, svd.root_extent, svd.root_len, true, &mut mark)
    } else {
        vec![]
    };

    // ---- joliet tree must mirror the payload exactly (names + contents) ----
    if expect_joliet {
        let jol_files: Vec<&Entry> = jol_entries.iter().filter(|e| !e.is_dir).collect();
        assert_eq!(jol_files.len(), fixture.files.len(), "joliet file count");
        for (want_path, want_content) in &fixture.files {
            let e = jol_files
                .iter()
                .find(|e| &e.path == want_path)
                .unwrap_or_else(|| panic!("joliet entry {:?} missing", want_path));
            assert_eq!(e.size as usize, want_content.len());
            let got = read_sector_range(img, e.extent, want_content.len());
            assert_eq!(
                got,
                want_content.as_slice(),
                "content mismatch for {}",
                want_path
            );
        }
        // directory count sanity: joliet dirs == base dirs
        assert_eq!(
            jol_entries.iter().filter(|e| e.is_dir).count(),
            base_entries.iter().filter(|e| e.is_dir).count()
        );
    }

    // ---- base tree: same file count/sizes (uppercase-mangled names) ----
    assert_eq!(base_files.len(), fixture.files.len(), "base file count");
    let mut base_sizes: Vec<u64> = base_files.iter().map(|e| e.size).collect();
    let mut want_sizes: Vec<u64> = fixture.files.iter().map(|(_, c)| c.len() as u64).collect();
    base_sizes.sort_unstable();
    want_sizes.sort_unstable();
    assert_eq!(base_sizes, want_sizes, "base tree sizes");
    // all base ids must be uppercase ASCII (mangled) and no ";N" visible duplicates
    let mut names: Vec<String> = base_files.iter().map(|e| e.path.clone()).collect();
    names.sort();
    for n in &names {
        for b in n.bytes() {
            assert!(
                b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_' || b == b'.' || b == b'/',
                "base name {:?} has lowercase/odd char",
                n
            );
        }
    }
    names.dedup();
    assert_eq!(names.len(), base_files.len(), "duplicate base names");

    // ---- path tables ----
    let base_dirs: Vec<Entry> = base_entries.iter().filter(|e| e.is_dir).cloned().collect();
    check_path_table(img, &pvd, false, &base_dirs);
    if expect_joliet {
        let svd = svd.as_ref().unwrap();
        let jol_dirs: Vec<Entry> = jol_entries.iter().filter(|e| e.is_dir).cloned().collect();
        check_path_table(img, svd, true, &jol_dirs);
    }

    // ---- file extents: no overlap with anything; joliet shares base extents ----
    let mut file_ranges: Vec<(u32, u32)> = base_files
        .iter()
        .map(|e| (e.extent, e.size as u32))
        .collect();
    if expect_joliet {
        let mut jol_ranges: Vec<(u32, u32)> = jol_entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| (e.extent, e.size as u32))
            .collect();
        file_ranges.sort_unstable();
        jol_ranges.sort_unstable();
        assert_eq!(
            file_ranges, jol_ranges,
            "joliet file extents must mirror the base tree exactly"
        );
    }
    for (ext, size) in &file_ranges {
        if *size == 0 {
            continue; // empty files store no data; extent LBA may alias the next file
        }
        let sectors = ((*size as usize).div_ceil(SECTOR)).max(1);
        for s in 0..sectors {
            let lba = *ext as usize + s;
            assert!(
                !mark[lba],
                "file extent overlaps dir/structure at LBA {}",
                lba
            );
            mark[lba] = true;
        }
    }

    // ---- El Torito ----
    if expect_boot {
        // find the boot record VD (type 0) among the descriptors
        let mut lba = 16usize;
        let mut boot_catalog = None;
        loop {
            let sec = &img[lba * SECTOR..(lba + 1) * SECTOR];
            if sec[0] == 0 {
                boot_catalog = Some(r32le(sec, 71));
            }
            if sec[0] == 255 {
                break;
            }
            lba += 1;
        }
        let cat = boot_catalog.expect("boot record VD present");
        // expected boot file: pkg/efi/boot/bootx64.efi — find its extent via joliet or base walk
        let boot_entry = if expect_joliet {
            jol_entries
                .iter()
                .find(|e| !e.is_dir && e.path == fixture.boot_file)
                .unwrap()
        } else {
            base_entries
                .iter()
                .find(|e| !e.is_dir && e.path.to_ascii_lowercase() == fixture.boot_file)
                .unwrap()
        };
        check_eltorito(img, cat, boot_entry.extent, boot_entry.size);
    }

    // ---- no extent may lie past the volume end ----
    for (i, m) in mark.iter().enumerate() {
        if *m {
            assert!(i < total, "extent {} beyond volume end {}", i, total);
        }
    }
}

#[test]
fn e2e_default_joliet_boot() {
    let fx = make_payload();
    let img_path = std::env::temp_dir().join(format!("fs2iso-e2e-{}.iso", std::process::id()));
    let opts = Options {
        label: Some("EFI tools".to_string()),
        ..Options::default()
    };
    build_iso(&img_path, &[fx.dir.join("pkg")], &opts).expect("build failed");
    let img = fs::read(&img_path).unwrap();
    full_check(&img, true, true, &fx);
    // image size must equal the declared volume size
    let total = r32le(&img[16 * SECTOR..], 80) as usize;
    assert_eq!(img.len(), total * SECTOR, "image file size == volume size");
    fs::remove_file(&img_path).ok();
    let _ = fs::remove_dir_all(&fx.dir);
}

#[test]
fn e2e_no_joliet_no_boot() {
    let fx = make_payload();
    let img_path = std::env::temp_dir().join(format!("fs2iso-e2e-nb-{}.iso", std::process::id()));
    let opts = Options {
        no_joliet: true,
        no_eltorito: true,
        flat: false,
        label: None,
        boot_efi: None,
    };
    build_iso(&img_path, &[fx.dir.join("pkg")], &opts).expect("build failed");
    let img = fs::read(&img_path).unwrap();
    full_check(&img, false, false, &fx);
    fs::remove_file(&img_path).ok();
    let _ = fs::remove_dir_all(&fx.dir);
}

#[test]
fn flat_mode_merges_contents() {
    let fx = make_payload();
    let img_path = std::env::temp_dir().join(format!("fs2iso-e2e-flat-{}.iso", std::process::id()));
    let opts = Options {
        flat: true,
        no_eltorito: true,
        ..Options::default()
    };
    build_iso(&img_path, &[fx.dir.join("pkg")], &opts).expect("build failed");
    let img = fs::read(&img_path).unwrap();
    // with flat, paths lose the "pkg/" prefix
    let flat_fx = Fixture {
        files: fx
            .files
            .iter()
            .map(|(p, c)| (p.trim_start_matches("pkg/").to_string(), c.clone()))
            .collect(),
        boot_file: "efi/boot/bootx64.efi".to_string(),
        dir: fx.dir.clone(),
    };
    full_check(&img, false, true, &flat_fx); // --no-eltorito honored
    fs::remove_file(&img_path).ok();
    let _ = fs::remove_dir_all(&fx.dir);
}

#[test]
fn explicit_boot_efi_flag() {
    let fx = make_payload();
    let img_path = std::env::temp_dir().join(format!("fs2iso-e2e-xb-{}.iso", std::process::id()));
    let shell = fx.dir.join("pkg/tools/diskpart.nsh");
    // --boot-efi must be a payload file; use the .nsh file (content irrelevant)
    let opts = Options {
        boot_efi: Some(shell.clone()),
        no_eltorito: false,
        ..Options::default()
    };
    build_iso(&img_path, &[fx.dir.join("pkg")], &opts).expect("build failed");
    let img = fs::read(&img_path).unwrap();
    let mut marked = vec![false; (img.len() / SECTOR) + 1];
    let (pvd, svd) = parse_vds(&img);
    walk_ns(&img, pvd.root_extent, pvd.root_len, false, &mut marked);
    let jol = svd.as_ref().expect("joliet").root_extent;
    let jol_len = svd.as_ref().unwrap().root_len;
    let jol_entries = walk_ns(&img, jol, jol_len, true, &mut marked);
    let mut lba = 16usize;
    loop {
        let sec = &img[lba * SECTOR..(lba + 1) * SECTOR];
        if sec[0] == 0 {
            let cat = r32le(sec, 71);
            let entry = jol_entries
                .iter()
                .find(|e| e.path == "pkg/tools/diskpart.nsh")
                .unwrap();
            check_eltorito(&img, cat, entry.extent, entry.size);
            break;
        }
        if sec[0] == 255 {
            panic!("no boot record");
        }
        lba += 1;
    }
    fs::remove_file(&img_path).ok();
    let _ = fs::remove_dir_all(&fx.dir);
}

#[test]
fn rejects_missing_boot_efi() {
    let fx = make_payload();
    let img_path = std::env::temp_dir().join(format!("fs2iso-e2e-mb-{}.iso", std::process::id()));
    let opts = Options {
        boot_efi: Some(PathBuf::from("NOPE.EFI")),
        ..Options::default()
    };
    let r = build_iso(&img_path, &[fx.dir.join("pkg")], &opts);
    assert!(r.is_err(), "boot file not in payload must be rejected");
    let _ = fs::remove_dir_all(&fx.dir);
}
