//! End-to-end tests: pack real payloads with `build_iso` (hadris-cd engine:
//! ISO9660 + Joliet + UDF bridge) and read the result back with an
//! independent minimal ISO9660 parser (base + Joliet trees, directory
//! chains, file contents, El Torito boot entries). The UDF namespace is
//! exercised separately by the QEMU+OVMF harness under tests/efi/.

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
        let dir = std::env::temp_dir().join(format!("fs2iso-udf-{}-{}", std::process::id(), n));
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
    fn mkdir(&self, rel: &str) -> PathBuf {
        let p = self.pkg.join(rel);
        fs::create_dir_all(&p).unwrap();
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
    fx.mkdir("scripts/tools/empty dir");
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// minimal independent ISO9660 reader (base + Joliet namespaces)
// ---------------------------------------------------------------------------

fn r32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

struct Rec {
    id: Vec<u8>,
    is_dir: bool,
    extent: u32,
    dlen: u32,
}

fn dir_recs(img: &[u8], lba: u32, dlen: u32) -> Vec<Rec> {
    let start = lba as usize * S;
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

fn decode_id(id: &[u8], joliet: bool) -> Option<String> {
    if id.is_empty() || id == [0x00] || id == [0x01] {
        return None;
    }
    if !joliet {
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

/// Walk one ISO namespace: relpath -> (is_dir, size, extent).
/// `desc_off` is the byte offset of the PVD/SVD within the image; its root
/// directory record starts at descriptor offset 156 (ECMA-119 8.4/9.1).
fn walk(img: &[u8], desc_off: usize, joliet: bool) -> HashMap<String, (bool, u64, u32)> {
    let root_lba = r32(img, desc_off + 156 + 2);
    let root_len = r32(img, desc_off + 156 + 10);
    let mut out = HashMap::new();
    fn rec(
        img: &[u8],
        joliet: bool,
        lba: u32,
        dlen: u32,
        prefix: &str,
        out: &mut HashMap<String, (bool, u64, u32)>,
    ) {
        for r in dir_recs(img, lba, dlen) {
            let Some(name) = decode_id(&r.id, joliet) else { continue };
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{}/{}", prefix, name)
            };
            if r.is_dir {
                out.insert(rel.clone(), (true, r.dlen as u64, r.extent));
                rec(img, joliet, r.extent, r.dlen, &rel, out);
            } else {
                out.insert(rel, (false, r.dlen as u64, r.extent));
            }
        }
    }
    rec(img, joliet, root_lba, root_len, "", &mut out);
    out
}

struct Image {
    img: Vec<u8>,
    pvd_lba: usize,
    svd_lbas: Vec<usize>, // all supplementary VDs, in order
    boot_vd_lba: Option<usize>,
}

fn parse_image(path: &Path) -> Image {
    let img = fs::read(path).unwrap();
    let mut pvd = 0;
    let mut svds = Vec::new();
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
            2 => svds.push(lba),
            0 => boot = Some(lba),
            _ => {}
        }
    }
    Image {
        img,
        pvd_lba: pvd,
        svd_lbas: svds,
        boot_vd_lba: boot,
    }
}

/// Real Joliet SVD: escape sequence "%/E" (ECMA-119 8.4). hadris-cd also
/// emits a placeholder SVD for the UDF bridge whose root record is empty.
fn joliet_lba(im: &Image) -> Option<usize> {
    im.svd_lbas.iter().copied().find(|lba| {
        let sec = &im.img[lba * S..lba * S + S];
        sec[88] == b'%' && sec[89] == b'/' && sec[90] == b'E'
    })
}

fn content(img: &[u8], extent: u32, size: u64) -> Vec<u8> {
    if size == 0 {
        return Vec::new();
    }
    let start = extent as usize * S;
    img[start..start + size as usize].to_vec()
}


/// Directory-record file flags (bit0 = hidden) of the record for `target` at
/// the ROOT of the given namespace (desc byte offset in the image).
fn root_record_flags(im: &Image, desc_off: usize, target: &str, joliet: bool) -> Option<u8> {
    let sec = &im.img[desc_off..desc_off + 2048];
    let ext = r32(sec, 156 + 2) as usize;
    let dlen = r32(sec, 156 + 10) as usize;
    let dir = &im.img[ext * S..ext * S + dlen];
    let mut pos = 0usize;
    while pos + 33 <= dlen {
        let ln = dir[pos] as usize;
        if ln == 0 {
            break;
        }
        if pos + ln > dlen {
            break;
        }
        let rec = &dir[pos..pos + ln];
        let idlen = rec[32] as usize;
        let id = &rec[33..33 + idlen];
        let name = if joliet {
            let units: Vec<u16> = id
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            let raw = match id.iter().position(|&c| c == b';') {
                Some(i) => &id[..i],
                None => id,
            };
            String::from_utf8_lossy(raw).into_owned().to_uppercase()
        };
        let hit = if joliet {
            name == target
        } else {
            name == target.to_uppercase()
        };
        if hit {
            return Some(rec[25]);
        }
        pos += ln;
    }
    None
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

/// Default (keep-parent) build with auto El Torito boot: the Joliet tree
/// shows every payload file under pkg/ with original names and byte-exact
/// content; the boot entry targets the boot file; base namespace readable.

/// Every payload file goes into the FAT container (esp.img) verbatim —
/// original paths, no injected boot file. El Torito points at esp.img.
/// Default (no --no-eltorito) means esp.img is ALWAYS generated, whether or
/// not the payload contains any boot program.
#[test]
fn default_keep_parent_packs_esp() {
    let fx = Fx::new();
    let payload = sorted_payload(&fx);
    fx.file("EFI/BOOT/BOOTX64.EFI", b"a user-provided efi\n"); // treated as plain data
    let out = fx.dir.join("out.iso");
    let sum = build_iso(&out, &[fx.pkg.clone()], &Options::default()).unwrap();
    assert!(sum.bootable);
    assert_eq!(sum.files, payload.len() as u64 + 1);

    let im = parse_image(&out);
    let jol = walk(&im.img, joliet_lba(&im).unwrap() * S, true);

    // ISO9660(+Joliet) data tree: payload under pkg/, plus engine artifacts
    let mut expected: Vec<(String, Vec<u8>)> = payload
        .iter()
        .map(|(rel, data)| (format!("pkg/{}", rel), data.clone()))
        .collect();
    expected.push((
        "pkg/EFI/BOOT/BOOTX64.EFI".to_string(),
        b"a user-provided efi\n".to_vec(),
    ));
    for (rel, data) in &expected {
        let (_, size, ext) = jol
            .get(rel)
            .copied()
            .unwrap_or_else(|| panic!("missing in joliet: {}", rel));
        assert_eq!(size, data.len() as u64, "size of {}", rel);
        assert_eq!(&content(&im.img, ext, size), data, "content of {}", rel);
    }
    let jfiles: Vec<String> = jol
        .iter()
        .filter(|(_, (d, _, _))| !*d)
        .map(|(k, _)| k.clone())
        .filter(|k| k != "boot.catalog" && k != "esp.img")
        .collect();
    assert_eq!(jfiles.len(), expected.len(), "{:?}", jfiles);

    // El Torito points at esp.img
    let (_, esp_size, esp_ext) = jol.get("esp.img").copied().expect("esp.img present");
    assert!(esp_size > 0);
    assert_eq!(&content(&im.img, esp_ext, esp_size)[510..512], &[0x55, 0xAA]);
    assert!(
        boot_entries(&im)
            .iter()
            .any(|(m, rba)| *m == 0 && *rba == esp_ext),
        "El Torito entry at esp.img"
    );

    // Engine artifacts are HIDDEN in both namespaces (bit0 of file flags);
    // the user's own files are not.
    for artifact in ["esp.img", "boot.catalog"] {
        let base_flags = root_record_flags(&im, im.pvd_lba * S, artifact, false)
            .expect("artifact in base tree");
        assert_eq!(base_flags & 1, 1, "{} hidden in base tree", artifact);
        let jol_flags = root_record_flags(&im, joliet_lba(&im).unwrap() * S, artifact, true)
            .expect("artifact in joliet tree");
        assert_eq!(jol_flags & 1, 1, "{} hidden in joliet tree", artifact);
    }

    // FAT read-back: esp.img == payload, verbatim, no additions
    let esp = content(&im.img, esp_ext, esp_size);
    let fs = fatfs::FileSystem::new(std::io::Cursor::new(esp), fatfs::FsOptions::new()).unwrap();
    let mut actual: Vec<String> = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(p) = pending.pop() {
        let mut dir = fs.root_dir();
        for comp in p.split('/') {
            if !comp.is_empty() {
                dir = dir.open_dir(comp).unwrap();
            }
        }
        for e in dir.iter() {
            let e = e.unwrap();
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let rel = if p.is_empty() { name.clone() } else { format!("{}/{}", p, name) };
            if e.is_dir() {
                pending.push(rel);
            } else {
                actual.push(rel.to_uppercase());
            }
        }
    }
    let mut want: Vec<String> = expected.iter().map(|(r, _)| r.to_uppercase()).collect();
    want.sort();
    actual.sort();
    assert_eq!(actual, want, "esp.img mirrors payload exactly (no injected files)");
    assert_eq!(sum.label, "OUT");
}

/// --flat + esp: files at the image root, esp.img mirrors them at root.
#[test]
fn flat_packs_esp_at_root() {
    let fx = Fx::new();
    let payload = sorted_payload(&fx);
    let out = fx.dir.join("out.iso");
    let opts = Options {
        flat: true,
        ..Options::default()
    };
    let sum = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert!(sum.bootable);

    let im = parse_image(&out);
    let jol = walk(&im.img, joliet_lba(&im).unwrap() * S, true);
    let (_, esp_size, esp_ext) = jol.get("esp.img").copied().expect("esp.img present");
    let esp = content(&im.img, esp_ext, esp_size);
    let fs = fatfs::FileSystem::new(std::io::Cursor::new(esp), fatfs::FsOptions::new()).unwrap();
    let mut actual: Vec<String> = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(p) = pending.pop() {
        let mut dir = fs.root_dir();
        for comp in p.split('/') {
            if !comp.is_empty() {
                dir = dir.open_dir(comp).unwrap();
            }
        }
        for e in dir.iter() {
            let e = e.unwrap();
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let rel = if p.is_empty() { name.clone() } else { format!("{}/{}", p, name) };
            if e.is_dir() {
                pending.push(rel);
            } else {
                actual.push(rel.to_uppercase());
            }
        }
    }
    let mut want: Vec<String> = payload.iter().map(|(r, _)| r.to_uppercase()).collect();
    want.sort();
    actual.sort();
    assert_eq!(actual, want, "flat esp.img mirrors payload at root");
}

/// --no-eltorito: plain ISO9660+Joliet data disc, no esp.img, no boot.
#[test]
fn no_eltorito_is_plain_data_disc() {
    let fx = Fx::new();
    let payload = sorted_payload(&fx);
    let out = fx.dir.join("out.iso");
    let opts = Options {
        flat: true,
        no_eltorito: true,
        ..Options::default()
    };
    let sum = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert!(!sum.bootable);
    let im = parse_image(&out);
    let jol = walk(&im.img, joliet_lba(&im).unwrap() * S, true);
    assert!(!jol.contains_key("esp.img"), "no esp.img on --no-eltorito");
    assert!(boot_entries(&im).is_empty());
    let files: Vec<String> = jol
        .iter()
        .filter(|(_, (d, _, _))| !*d)
        .map(|(k, _)| k.clone())
        .collect();
    assert_eq!(files.len(), payload.len(), "{:?}", files);
    for (rel, data) in &payload {
        let (_, size, ext) = jol
            .get(rel)
            .copied()
            .unwrap_or_else(|| panic!("missing {}", rel));
        assert_eq!(size, data.len() as u64);
        assert_eq!(&content(&im.img, ext, size), data, "content of {}", rel);
    }
}

/// Error paths: output overwriting a payload file and duplicate merged
/// root names.
#[test]
fn error_paths() {
    let fx = Fx::new();
    sorted_payload(&fx);
    let out = fx.dir.join("out.iso");

    let err = build_iso(
        &fx.pkg.join("readme.txt"),
        &[fx.pkg.clone()],
        &Options::default(),
    )
    .unwrap_err();
    assert!(err.contains("would overwrite"), "{}", err);

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
            no_eltorito: true,
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
        no_eltorito: true,
        ..Options::default()
    };
    let sum = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert_eq!(sum.label, "MY_TOOLS_2024_");
    assert_eq!(sum.files, 9);
    assert!(sum.dirs >= 4, "dirs counted: {}", sum.dirs);
    assert!(sum.sectors > 0);
    assert_eq!(
        fs::metadata(&out).unwrap().len(),
        sum.sectors as u64 * S as u64
    );
}
