//! End-to-end tests: pack real payloads with `build_iso` and read the result
//! back with an independent minimal ISO9660 parser (base + Joliet trees,
//! directory chains, file contents, El Torito boot entries). This validates
//! whatever writer engine produces the image (currently hadris-iso).

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
        let dir = std::env::temp_dir().join(format!("fs2iso-mig-{}-{}", std::process::id(), n));
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

/// payload -> (contents, sha) for each file, rel paths without "pkg/" prefix.
fn std_payload(fx: &Fx) -> Vec<(String, Vec<u8>)> {
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
    files
}

fn sorted_payload(fx: &Fx) -> Vec<(String, Vec<u8>)> {
    let mut f = std_payload(fx);
    f.sort();
    f
}

// ---------------------------------------------------------------------------
// minimal independent reader
// ---------------------------------------------------------------------------

fn r16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn r32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

struct Rec {
    id: Vec<u8>,
    is_dir: bool,
    extent: u32,
    dlen: u32,
}

/// Iterate the records of a directory block (starting at sector `lba`).
fn dir_recs(img: &[u8], lba: u32, dlen: u32) -> Vec<Rec> {
    let start = (lba as usize) * S;
    let end = start + (dlen as usize).min(img.len() - start);
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
        return None; // structural "." / ".."
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

/// Walk one namespace from its root dir record.
/// Returns relpath -> (is_dir, size, extent).
fn walk(img: &[u8], root_sector: &[u8], joliet: bool) -> HashMap<String, (bool, u64, u32)> {
    let root_lba = r32(root_sector, 156 + 2);
    let root_len = r32(root_sector, 156 + 10);
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
            let Some(name) = decode_id(&r.id, joliet) else {
                continue;
            };
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
    svd_lba: Option<usize>,
    boot_vd_lba: Option<usize>,
}

fn parse_image(path: &Path) -> Image {
    let img = fs::read(path).unwrap();
    let mut pvd = 0;
    let mut svd = None;
    let mut boot = None;
    for lba in 16..64usize {
        let sec = &img[lba * S..(lba + 1) * S];
        if &sec[1..6] != b"CD001" {
            continue;
        }
        match sec[0] {
            1 => pvd = lba,
            2 if svd.is_none() => svd = Some(lba),
            0 => boot = Some(lba),
            255 => break,
            _ => {}
        }
    }
    Image {
        img,
        pvd_lba: pvd,
        svd_lba: svd,
        boot_vd_lba: boot,
    }
}

/// El Torito boot entries from the boot catalog: (media_type, load_rba).
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
        match block[0] {
            0x88 => out.push((block[1], r32(block, 8))), // boot entry
            0x90 | 0x91 | 0x99 => continue,              // section headers / done
            _ => {}
        }
    }
    out
}

fn file_content(img: &[u8], extent: u32, size: u64) -> Vec<u8> {
    if size == 0 {
        return Vec::new();
    }
    let start = (extent as usize) * S;
    img[start..start + size as usize].to_vec()
}

fn files_only(w: &HashMap<String, (bool, u64, u32)>) -> Vec<(String, u64, u32)> {
    let mut v: Vec<_> = w
        .iter()
        .filter(|(_, (d, _, _))| !*d)
        .map(|(k, (_, s, e))| (k.clone(), *s, *e))
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Default (keep-parent) build: every payload file visible under pkg/ in the
/// Joliet tree with exact names and contents; auto El Torito boot on the
/// EFI/BOOT/BOOTX64.EFI inside the payload.
#[test]
fn keep_parent_auto_boot() {
    let fx = Fx::new();
    let payload = sorted_payload(&fx);
    fx.file("EFI/BOOT/BOOTX64.EFI", b"fake efi boot image\n");
    let out = fx.dir.join("out.iso");
    let sum = build_iso(&out, &[fx.pkg.clone()], &Options::default()).unwrap();
    assert!(sum.joliet);
    assert_eq!(sum.files, payload.len() as u64 + 1); // + bootx64.efi

    let im = parse_image(&out);
    let svd = im.svd_lba.expect("joliet SVD present");
    let jol = walk(&im.img, &im.img[svd * S..(svd + 1) * S], true);
    // engine adds its own boot.catalog at the root
    let mut expected: Vec<(String, u64, u32)> = payload
        .iter()
        .map(|(rel, data)| (format!("pkg/{}", rel), data.len() as u64, 0))
        .collect();
    expected.push(("pkg/EFI/BOOT/BOOTX64.EFI".to_string(), 20, 0));
    expected.sort();

    let jol_files = files_only(&jol);
    let filtered: Vec<_> = jol_files
        .iter()
        .filter(|(n, _, _)| n != "boot.catalog")
        .map(|(n, s, e)| (n.clone(), *s, *e))
        .collect();
    assert_eq!(filtered.len(), expected.len());
    for (n, s, e) in &filtered {
        let want = expected
            .iter()
            .find(|(wn, _, _)| wn == n)
            .expect("name found");
        assert_eq!(*s, want.1, "size of {}", n);
        assert!(jol.contains_key(n));
        assert_eq!(
            file_content(&im.img, *e, *s),
            fs::read(fx.pkg.join(&n["pkg/".len()..])).unwrap(),
            "content {}",
            n
        );
    }
    // content of a nested file read back byte-exact through joliet
    let deep = jol.get("pkg/sub dir/deeper/x.Y").unwrap();
    assert_eq!(file_content(&im.img, deep.2, deep.1), b"deep\n");

    // base namespace: files = payload + bootx64 + boot.catalog, unique names
    let base = walk(
        &im.img,
        &im.img[im.pvd_lba * S..(im.pvd_lba + 1) * S],
        false,
    );
    let base_files = files_only(&base);
    let mut names: Vec<String> = base_files.iter().map(|(n, _, _)| n.clone()).collect();
    names.sort();
    let uniq: std::collections::HashSet<_> = names.iter().collect();
    assert_eq!(
        names.len(),
        payload.len() + 2,
        "base has all files + boot.catalog"
    );
    assert_eq!(uniq.len(), names.len(), "no duplicate base identifiers");
    // El Torito: no-emulation entry must point at the boot file's extent
    let boot_ext = jol
        .get("pkg/EFI/BOOT/BOOTX64.EFI")
        .map(|(_, _, e)| *e)
        .unwrap();
    let entries = boot_entries(&im);
    assert!(!entries.is_empty(), "boot catalog has entries");
    assert!(
        entries
            .iter()
            .any(|(media, rba)| *media == 0 && *rba == boot_ext),
        "EFI boot entry points at the boot file (entries {:?}, want lba {})",
        entries,
        boot_ext
    );
    // summary sanity
    assert_eq!(sum.boot_path.as_deref(), Some("pkg/EFI/BOOT/BOOTX64.EFI"));
    assert!(sum.sectors > 0);
    assert!(sum.payload_bytes >= 5000);
}

/// Flat build without El Torito: the Joliet tree equals the payload exactly.
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
    assert!(sum.joliet);
    assert_eq!(sum.files, payload.len() as u64);

    let im = parse_image(&out);
    assert!(im.boot_vd_lba.is_none(), "no El Torito boot record");
    let svd = im.svd_lba.unwrap();
    let jol = walk(&im.img, &im.img[svd * S..(svd + 1) * S], true);
    let jol_files = files_only(&jol);
    assert_eq!(jol_files.len(), payload.len());
    for (n, s, e) in &jol_files {
        let want = payload
            .iter()
            .find(|(wn, _)| wn == n)
            .expect("payload file");
        assert_eq!(*s, want.1.len() as u64, "size of {}", n);
        assert_eq!(file_content(&im.img, *e, *s), want.1, "content of {}", n);
    }
}

/// no_joliet: base namespace only, still fully readable; uppercase mangled
/// names with content intact.
#[test]
fn base_only_no_joliet() {
    let fx = Fx::new();
    let payload = sorted_payload(&fx);
    let out = fx.dir.join("out.iso");
    let opts = Options {
        no_joliet: true,
        no_eltorito: true,
        ..Options::default()
    };
    let sum = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert!(!sum.joliet);

    let im = parse_image(&out);
    assert!(im.svd_lba.is_none(), "no joliet SVD");
    let base = walk(
        &im.img,
        &im.img[im.pvd_lba * S..(im.pvd_lba + 1) * S],
        false,
    );
    let base_files = files_only(&base);
    assert_eq!(base_files.len(), payload.len());
    // find the base-namespace entry for readme.txt (engine-mangled name)
    let found = base
        .iter()
        .find(|(n, _)| n.to_lowercase().ends_with("readme.txt"))
        .expect("uppercase base name present");
    let (_, size, extent) = *found.1;
    let want = payload.iter().find(|(n, _)| n == "readme.txt").unwrap();
    assert_eq!(size, want.1.len() as u64);
    assert_eq!(file_content(&im.img, extent, size), want.1);
}

/// Explicit --boot-efi on a nested payload file (flat tree).
#[test]
fn explicit_boot_file() {
    let fx = Fx::new();
    fx.file("efi/tools/shell.efi", b"shell binary\n");
    fx.file("data.txt", b"payload\n");
    let out = fx.dir.join("out.iso");
    let boot_src = fx.pkg.join("efi/tools/shell.efi");
    let opts = Options {
        flat: true,
        boot_efi: Some(boot_src.clone()),
        ..Options::default()
    };
    let sum = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert_eq!(sum.boot_path.as_deref(), Some("efi/tools/shell.efi"));

    let im = parse_image(&out);
    let svd = im.svd_lba.unwrap();
    let jol = walk(&im.img, &im.img[svd * S..(svd + 1) * S], true);
    let boot_ext = jol.get("efi/tools/shell.efi").map(|(_, _, e)| *e).unwrap();
    let entries = boot_entries(&im);
    assert!(
        entries
            .iter()
            .any(|(media, rba)| *media == 0 && *rba == boot_ext),
        "boot entry points at shell.efi extent"
    );
}

/// --boot-efi naming a file outside the payload must fail clearly.
#[test]
fn boot_file_not_in_payload() {
    let fx = Fx::new();
    std_payload(&fx);
    let out = fx.dir.join("out.iso");
    let outside = fx.dir.join("elsewhere.efi");
    fs::write(&outside, b"x").unwrap();
    let opts = Options {
        boot_efi: Some(outside),
        ..Options::default()
    };
    let err = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap_err();
    assert!(err.contains("not part of the payload"), "{}", err);
}

/// Duplicate names at the merged root (flat, two dir args with same child)
/// are rejected with a clear message.
#[test]
fn duplicate_root_names_rejected() {
    let fx = Fx::new();
    let a = fx.pkg.join("a");
    let b = fx.pkg.join("b");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("same.txt"), b"1").unwrap();
    fs::write(b.join("same.txt"), b"2").unwrap();
    let out = fx.dir.join("out.iso");
    let opts = Options {
        flat: true,
        ..Options::default()
    };
    let err = build_iso(&out, &[a, b], &opts).unwrap_err();
    assert!(err.contains("duplicate root name"), "{}", err);
}

/// Output image may not overwrite one of its own payload files.
#[test]
fn refuses_to_overwrite_payload() {
    let fx = Fx::new();
    std_payload(&fx);
    let out = fx.pkg.join("readme.txt"); // an existing payload file
    let err = build_iso(&out, &[fx.pkg.clone()], &Options::default()).unwrap_err();
    assert!(err.contains("would overwrite"), "{}", err);
}

/// Label cleaning is applied and reported.
#[test]
fn label_and_summary() {
    let fx = Fx::new();
    std_payload(&fx);
    let out = fx.dir.join("out.iso");
    let opts = Options {
        flat: true,
        label: Some("My Tools 2024!".to_string()),
        ..Options::default()
    };
    let sum: BuildSummary = build_iso(&out, &[fx.pkg.clone()], &opts).unwrap();
    assert_eq!(sum.label, "MY_TOOLS_2024_");
    assert!(sum.dirs >= 4, "dirs counted: {}", sum.dirs);
    assert!(sum.sectors > 0);
    let img = fs::read(&out).unwrap();
    assert_eq!(img.len(), sum.sectors as usize * S);
}
