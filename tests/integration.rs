//! End-to-end tests: pack real payloads with `build_iso` (isobemak engine +
//! conformance pass) and read the result back with an independent minimal
//! ISO9660 parser: full tree walk, content equality, El Torito boot entries,
//! PVD both-endian fields and path tables.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use fs2iso::{build_iso, BuildSummary, Options};

const S: usize = 2048;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fx {
    dir: PathBuf,
    pkg: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fs2iso-iso-{}-{}", std::process::id(), n));
        let pkg = dir.join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        Fx { dir, pkg }
    }
    fn file(&self, rel: &str, data: &[u8]) -> PathBuf {
        let p = self.pkg.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        p
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn sorted_payload(fx: &Fx) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |fx: &Fx, rel: &str, data: &[u8]| {
        fx.file(rel, data);
        files.push((rel.to_string(), data.to_vec()));
    };
    add(fx, "readme.txt", b"hello readme\n");
    add(fx, ".hidden", b"dot file\n");
    add(fx, "a.b.c", b"dots\n");
    add(fx, "固件更新 2024.txt", "中文内容\n".as_bytes());
    add(fx, "empty.bin", b"");
    add(fx, "big file (payload).dat", &vec![0xabu8; 5000]);
    add(fx, "scripts/update.nsh", b"echo updating\r\n");
    add(fx, "scripts/tools/diskpart.nsh", b"echo disk\n");
    add(fx, "sub dir/deeper/x.Y", b"deep\n");
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// minimal independent reader
// ---------------------------------------------------------------------------

fn r32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn r16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

struct Rec {
    id: Vec<u8>,
    is_dir: bool,
    extent: u32,
    dlen: u32,
}

fn dir_recs(img: &[u8], lba: u32, dlen: u32) -> Vec<Rec> {
    let start = (lba as usize) * S;
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
        out.push(Rec {
            id: r[33..33 + idlen].to_vec(),
            is_dir: r[25] & 0x02 != 0,
            extent: r32(r, 2),
            dlen: r32(r, 10),
        });
        pos += ln;
    }
    out
}

/// Base-namespace name: strip ";version". (The engine only writes this one
/// namespace; ASCII is uppercased, other bytes pass through.)
fn decode_base(id: &[u8]) -> Option<String> {
    if id.is_empty() || id == [0x00] || id == [0x01] {
        return None;
    }
    let name = match id.iter().position(|&c| c == b';') {
        Some(i) => &id[..i],
        None => id,
    };
    if name.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(name).into_owned())
}

/// ASCII-uppercased form of an ISO path (how the engine renders names).
fn fold(s: &str) -> String {
    s.chars().map(|c| c.to_ascii_uppercase()).collect()
}

struct Image {
    img: Vec<u8>,
    pvd_lba: usize,
    boot_vd_lba: Option<usize>,
}

fn parse_image(path: &Path) -> Image {
    let img = fs::read(path).unwrap();
    let mut pvd = 0;
    let mut boot = None;
    for lba in 16..(img.len() / S).saturating_sub(1) {
        let sec = &img[lba * S..(lba + 1) * S];
        if sec[0] == 255 {
            break;
        }
        if &sec[1..6] != b"CD001" {
            continue;
        }
        match sec[0] {
            1 => pvd = lba,
            0 => boot = Some(lba),
            255 => break,
            _ => {}
        }
    }
    Image {
        img,
        pvd_lba: pvd,
        boot_vd_lba: boot,
    }
}

/// Walk the base tree: relpath -> (is_dir, size, extent).
fn walk(img: &[u8], root: &[u8]) -> HashMap<String, (bool, u64, u32)> {
    let root_lba = r32(root, 156 + 2);
    let root_len = r32(root, 156 + 10);
    let mut out = HashMap::new();
    fn rec(
        img: &[u8],
        lba: u32,
        dlen: u32,
        prefix: &str,
        out: &mut HashMap<String, (bool, u64, u32)>,
    ) {
        for r in dir_recs(img, lba, dlen) {
            let Some(name) = decode_base(&r.id) else {
                continue;
            };
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{}/{}", prefix, name)
            };
            if r.is_dir {
                out.insert(rel.clone(), (true, r.dlen as u64, r.extent));
                rec(img, r.extent, r.dlen, &rel, out);
            } else {
                out.insert(rel, (false, r.dlen as u64, r.extent));
            }
        }
    }
    rec(img, root_lba, root_len, "", &mut out);
    out
}

fn content(img: &[u8], extent: u32, size: u64) -> Vec<u8> {
    if size == 0 {
        return Vec::new();
    }
    let start = extent as usize * S;
    img[start..start + size as usize].to_vec()
}

/// El Torito boot entries: (media_type, load_rba).
fn boot_entries(im: &Image) -> Vec<(u8, u32)> {
    let Some(b) = im.boot_vd_lba else {
        return vec![];
    };
    let vd = &im.img[b * S..(b + 1) * S];
    let catalog = r32(vd, 71) as usize;
    let sec = &im.img[catalog * S..(catalog + 1) * S];
    let mut out = Vec::new();
    for i in (32..S).step_by(32) {
        let block = &sec[i..i + 32];
        if block[0] == 0x88 {
            out.push((block[1], r32(block, 8)));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Default (keep-parent) build with auto-detected El Torito boot: every
/// payload file must be readable in the ISO with exact content; names are the
/// engine-mangled (ASCII-uppercased) forms; the boot entry points at the
/// boot file; PVD is conformant (both-endian fields, real path tables).
#[test]
fn keep_parent_auto_boot_conformant() {
    let fx = Fx::new();
    let payload = sorted_payload(&fx);
    fx.file("EFI/BOOT/BOOTX64.EFI", b"fake efi boot image\n");
    let out = fx.dir.join("out.iso");
    let sum = build_iso(&out, &[fx.pkg.clone()], &Options::default()).unwrap();
    assert_eq!(sum.files, payload.len() as u64 + 1);
    assert_eq!(sum.boot_path.as_deref(), Some("pkg/EFI/BOOT/BOOTX64.EFI"));

    let im = parse_image(&out);
    // no supplementary (Joliet) descriptor is ever written
    let mut svd = false;
    for lba in 16..(im.img.len() / S).saturating_sub(1) {
        let sec = &im.img[lba * S..(lba + 1) * S];
        if sec[0] == 255 {
            break;
        }
        if &sec[1..6] == b"CD001" && sec[0] == 2 {
            svd = true;
        }
    }
    assert!(!svd, "single namespace only");

    let base = walk(&im.img, &im.img[im.pvd_lba * S..(im.pvd_lba + 1) * S]);

    // every payload file present with content intact (map by folded name)
    let mut expected: Vec<(String, Vec<u8>)> = payload
        .iter()
        .map(|(rel, data)| (fold(&format!("pkg/{}", rel)), data.clone()))
        .collect();
    expected.push((
        fold("pkg/EFI/BOOT/BOOTX64.EFI"),
        b"fake efi boot image\n".to_vec(),
    ));
    for (rel, data) in &expected {
        let (_, size, ext) = base
            .get(rel)
            .copied()
            .unwrap_or_else(|| panic!("missing {}", rel));
        assert_eq!(size, data.len() as u64, "size of {}", rel);
        assert_eq!(&content(&im.img, ext, size), data, "content of {}", rel);
    }
    // no extra files beyond expected
    let files: Vec<_> = base
        .iter()
        .filter(|(_, (d, _, _))| !*d)
        .map(|(k, _)| k.clone())
        .collect();
    assert_eq!(files.len(), expected.len(), "file set matches: {:?}", files);

    // El Torito boot entry targets the boot file's extent
    let boot_ext = base
        .get("PKG/EFI/BOOT/BOOTX64.EFI")
        .map(|(_, _, e)| *e)
        .unwrap();
    let entries = boot_entries(&im);
    assert!(
        entries
            .iter()
            .any(|(media, rba)| *media == 0 && *rba == boot_ext),
        "no-emulation boot entry at the boot file (entries {:?}, boot ext {})",
        entries,
        boot_ext
    );
    // validation entry platform forced to EFI (0xEF)
    let b = im.boot_vd_lba.unwrap();
    let vd = &im.img[b * S..(b + 1) * S];
    let catalog = r32(vd, 71) as usize;
    assert_eq!(im.img[catalog * S + 1], 0xEF, "validation platform is EFI");

    // PVD conformance: both-endian volume fields and real path tables
    let pvd = &im.img[im.pvd_lba * S..(im.pvd_lba + 1) * S];
    let le16 = |o: usize| u16::from_le_bytes(pvd[o..o + 2].try_into().unwrap()) as u32;
    let be16 = |o: usize| u16::from_be_bytes(pvd[o..o + 2].try_into().unwrap()) as u32;
    let le32 = |o: usize| u32::from_le_bytes(pvd[o..o + 4].try_into().unwrap());
    let be32 = |o: usize| u32::from_be_bytes(pvd[o..o + 4].try_into().unwrap());
    assert_eq!(le32(80), be32(84), "volume space size both-endian");
    assert_eq!(le16(120), be16(122), "volume set size both-endian");
    assert_eq!(le16(124), be16(126), "sequence number both-endian");
    assert_eq!(le16(128), be16(130), "block size both-endian");
    let pt_size = le32(132);
    let pt_le = le32(140) as usize;
    let pt_be = be32(148) as usize;
    assert!(pt_size > 0 && pt_le > 0 && pt_be > 0, "path tables present");
    assert_eq!(pt_size, be32(136), "PT size both-endian");
    // PT location points at real data (sector with a valid first entry)
    let first = &im.img[pt_le * S..pt_le * S + 16];
    assert_eq!(first[0], 0, "PT[0] root has empty identifier");
    // M table starts a whole number of sectors after L (or right after)
    assert!(pt_be >= pt_le && pt_be <= pt_le + 2, "M table follows L");
    // volume space size equals file size
    assert_eq!(le32(80) as usize, im.img.len() / S);
    assert_eq!(sum.sectors as usize, im.img.len() / S);
    assert!(sum.label.starts_with("OUT"), "label from output stem");
}

/// Flat + no boot: names are payload names ASCII-uppercased; all content OK.
#[test]
fn flat_no_boot() {
    let fx = Fx::new();
    let payload = sorted_payload(&fx);
    let out = fx.dir.join("out.iso");
    let opts = Options {
        flat: true,
        no_eltorito: true,
        ..Options::default()
    };
    let sum = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert!(sum.boot_path.is_none());

    let im = parse_image(&out);
    let base = walk(&im.img, &im.img[im.pvd_lba * S..(im.pvd_lba + 1) * S]);
    let files: Vec<_> = base
        .iter()
        .filter(|(_, (d, _, _))| !*d)
        .map(|(k, _)| k.clone())
        .collect();
    assert_eq!(files.len(), payload.len(), "{:?}", files);
    for (rel, data) in &payload {
        let folded = fold(rel);
        let (_, size, ext) = base
            .get(&folded)
            .copied()
            .unwrap_or_else(|| panic!("missing {}", folded));
        assert_eq!(size, data.len() as u64);
        assert_eq!(&content(&im.img, ext, size), data);
    }
    // no bootable entries in the catalog (engine may still write a BR VD)
    assert!(
        boot_entries(&im).is_empty(),
        "no El Torito entries requested"
    );
}

/// Explicit --boot-efi on a nested payload file.
#[test]
fn explicit_boot_file() {
    let fx = Fx::new();
    fx.file("efi/tools/shell.efi", b"shell binary\n");
    fx.file("data.txt", b"payload\n");
    let out = fx.dir.join("out.iso");
    let boot_src = fx.pkg.join("efi/tools/shell.efi");
    let opts = Options {
        flat: true,
        boot_efi: Some(boot_src),
        ..Options::default()
    };
    let sum = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert_eq!(sum.boot_path.as_deref(), Some("efi/tools/shell.efi"));

    let im = parse_image(&out);
    let base = walk(&im.img, &im.img[im.pvd_lba * S..(im.pvd_lba + 1) * S]);
    let boot_ext = base.get("EFI/TOOLS/SHELL.EFI").map(|(_, _, e)| *e).unwrap();
    let entries = boot_entries(&im);
    assert!(
        entries
            .iter()
            .any(|(media, rba)| *media == 0 && *rba == boot_ext),
        "boot entry at shell.efi ({:?})",
        entries
    );
}

/// Errors: boot file outside payload, duplicate root names, output
/// overwriting a payload file.
#[test]
fn error_paths() {
    let fx = Fx::new();
    sorted_payload(&fx);
    let out = fx.dir.join("out.iso");

    let outside = fx.dir.join("elsewhere.efi");
    fs::write(&outside, b"x").unwrap();
    let err = build_iso(
        &out,
        &[fx.pkg.clone()],
        &Options {
            boot_efi: Some(outside),
            ..Options::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("not part of the payload"), "{}", err);

    let err = build_iso(
        &fx.pkg.join("readme.txt"),
        &[fx.pkg.clone()],
        &Options::default(),
    )
    .unwrap_err();
    assert!(err.contains("would overwrite"), "{}", err);

    // duplicate merged root names (case-insensitively identical)
    let a = fx.pkg.join("a");
    let b = fx.pkg.join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("same.txt"), b"1").unwrap();
    fs::write(b.join("same.TXT"), b"2").unwrap(); // same folded name
    let err = build_iso(
        &out,
        &[a, b],
        &Options {
            flat: true,
            ..Options::default()
        },
    )
    .unwrap_err();
    assert!(err.contains("duplicate root name"), "{}", err);
}

/// Label cleaning and summary sanity.
#[test]
fn label_and_summary() {
    let fx = Fx::new();
    sorted_payload(&fx);
    let out = fx.dir.join("out.iso");
    let opts = Options {
        flat: true,
        label: Some("My Tools 2024!".to_string()),
        ..Options::default()
    };
    let sum: BuildSummary = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert_eq!(sum.label, "MY_TOOLS_2024_");
    assert_eq!(sum.files, 9);
    assert!(sum.dirs >= 4, "dirs counted: {}", sum.dirs);
    assert!(sum.sectors > 0);
    assert_eq!(
        fs::metadata(&out).unwrap().len(),
        sum.sectors as u64 * S as u64
    );
}
