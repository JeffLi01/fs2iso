//! End-to-end tests: pack real payloads with `build_iso` (hadris-cd engine:
//! ISO9660 + Joliet) and read the result back with an independent minimal
//! ISO9660 parser (base + Joliet trees, directory chains, file contents,
//! El Torito boot entries, directory-record hidden flags). Real-firmware
//! behaviour is exercised separately by the QEMU+OVMF harness in
//! tests/efi/ (see run_acceptance.sh).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use fs2iso::{build_iso, Options};

const SECTOR_BYTES: usize = 2048;

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    dir: PathBuf,
    pkg: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("fs2iso-test-{}-{}", std::process::id(), n));
        let pkg = dir.join("pkg");
        fs::create_dir_all(&pkg).unwrap();
        Fixture { dir, pkg }
    }
    fn file(&self, relative_path: &str, data: &[u8]) -> PathBuf {
        let p = self.pkg.join(relative_path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        p
    }
    fn mkdir(&self, relative_path: &str) -> PathBuf {
        let p = self.pkg.join(relative_path);
        fs::create_dir_all(&p).unwrap();
        p
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn sorted_payload(fixture: &Fixture) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut add = |fixture: &Fixture, relative_path: &str, data: &[u8]| {
        fixture.file(relative_path, data);
        files.push((relative_path.to_string(), data.to_vec()));
    };
    add(fixture, "readme.txt", b"hello readme\n");
    add(fixture, ".hidden", b"dot file\n");
    add(fixture, "a.b.c", b"dots\n");
    add(fixture, "固件更新 2024.txt", "中文内容\n".as_bytes());
    add(fixture, "empty.bin", b"");
    add(fixture, "big file (payload).dat", &vec![0xabu8; 5000]);
    add(fixture, "scripts/update.nsh", b"echo updating\r\n");
    add(fixture, "scripts/tools/diskpart.nsh", b"echo disk\n");
    add(fixture, "sub dir/deeper/x.Y", b"deep\n");
    fixture.mkdir("scripts/tools/empty dir");
    files.sort();
    files
}

// ---------------------------------------------------------------------------
// minimal independent ISO9660 reader (base + Joliet namespaces)
// ---------------------------------------------------------------------------

fn read_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

struct DirectoryRecord {
    identifier: Vec<u8>,
    flags: u8,
    is_directory: bool,
    extent: u32,
    data_length: u32,
}

fn directory_records(img: &[u8], lba: u32, data_length: u32) -> Vec<DirectoryRecord> {
    let start = lba as usize * SECTOR_BYTES;
    let end = (start + data_length as usize).min(img.len());
    let mut records = Vec::new();
    let mut offset = start;
    while offset + 33 <= end {
        let record_length = img[offset] as usize;
        if record_length == 0 {
            break;
        }
        if offset + record_length > end {
            break;
        }
        let record = &img[offset..offset + record_length];
        let identifier_length = record[32] as usize;
        records.push(DirectoryRecord {
            identifier: record[33..33 + identifier_length].to_vec(),
            flags: record[25],
            is_directory: record[25] & 0x02 != 0,
            extent: read_u32_le(record, 2),
            data_length: read_u32_le(record, 10),
        });
        offset += record_length;
    }
    records
}

fn decode_identifier(identifier: &[u8], joliet: bool) -> Option<String> {
    if identifier.is_empty() || identifier == [0x00] || identifier == [0x01] {
        return None;
    }
    if !joliet {
        let name = match identifier.iter().position(|&byte| byte == b';') {
            Some(semicolon_index) => &identifier[..semicolon_index],
            None => identifier,
        };
        if name.is_empty() {
            return None;
        }
        return Some(String::from_utf8_lossy(name).into_owned());
    }
    let units: Vec<u16> = identifier
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Walk one ISO namespace: relpath -> (is_dir, size, extent).
/// `desc_off` is the byte offset of the PVD/SVD within the image; its root
/// directory record starts at descriptor offset 156 (ECMA-119 8.4/9.1).
fn walk(img: &[u8], desc_off: usize, joliet: bool) -> HashMap<String, (bool, u64, u32)> {
    let root_lba = read_u32_le(img, desc_off + 156 + 2);
    let root_len = read_u32_le(img, desc_off + 156 + 10);
    let mut out = HashMap::new();
    fn visit(
        img: &[u8],
        joliet: bool,
        lba: u32,
        data_length: u32,
        prefix: &str,
        out: &mut HashMap<String, (bool, u64, u32)>,
    ) {
        for record in directory_records(img, lba, data_length) {
            let Some(name) = decode_identifier(&record.identifier, joliet) else {
                continue;
            };
            let relative_path = if prefix.is_empty() {
                name
            } else {
                format!("{}/{}", prefix, name)
            };
            if record.is_directory {
                out.insert(
                    relative_path.clone(),
                    (true, record.data_length as u64, record.extent),
                );
                visit(
                    img,
                    joliet,
                    record.extent,
                    record.data_length,
                    &relative_path,
                    out,
                );
            } else {
                out.insert(
                    relative_path,
                    (false, record.data_length as u64, record.extent),
                );
            }
        }
    }
    visit(img, joliet, root_lba, root_len, "", &mut out);
    out
}

struct Image {
    img: Vec<u8>,
    primary_lba: usize,
    supplementary_lbas: Vec<usize>, // all supplementary descriptors, in order
    boot_descriptor_lba: Option<usize>,
}

/// Parse the volume-descriptor chain (sectors 16.. until the terminator)
/// into the locations needed by the readers below.
fn parse_image(path: &Path) -> Image {
    let img = fs::read(path).unwrap();
    let mut primary = 0;
    let mut supplementary = Vec::new();
    let mut boot = None;
    for lba in 16..(img.len() / SECTOR_BYTES).saturating_sub(1) {
        let sector = &img[lba * SECTOR_BYTES..(lba + 1) * SECTOR_BYTES];
        if sector[0] == 255 {
            break;
        }
        if &sector[1..6] != b"CD001" {
            continue;
        }
        match sector[0] {
            1 => primary = lba,
            2 => supplementary.push(lba),
            0 => boot = Some(lba),
            _ => {}
        }
    }
    Image {
        img,
        primary_lba: primary,
        supplementary_lbas: supplementary,
        boot_descriptor_lba: boot,
    }
}

/// Real Joliet supplementary descriptor: escape sequence "%/E"
/// (ECMA-119 8.4). hadris-cd may emit other supplementary descriptors; only
/// the one with this escape carries the Joliet tree.
fn joliet_lba(image: &Image) -> Option<usize> {
    image.supplementary_lbas.iter().copied().find(|lba| {
        let sec = &image.img[lba * SECTOR_BYTES..lba * SECTOR_BYTES + SECTOR_BYTES];
        sec[88] == b'%' && sec[89] == b'/' && sec[90] == b'E'
    })
}

fn content(img: &[u8], extent: u32, size: u64) -> Vec<u8> {
    if size == 0 {
        return Vec::new();
    }
    let start = extent as usize * SECTOR_BYTES;
    img[start..start + size as usize].to_vec()
}

/// Recursively list every file inside a FAT image as an uppercased,
/// forward-slash relative path — used to assert esp.img mirrors the payload
/// exactly (no injected files).
fn list_fat_files(fat_bytes: Vec<u8>) -> Vec<String> {
    let filesystem =
        fatfs::FileSystem::new(std::io::Cursor::new(fat_bytes), fatfs::FsOptions::new()).unwrap();
    let mut files = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(prefix) = pending.pop() {
        let mut directory = filesystem.root_dir();
        for component in prefix.split('/') {
            if !component.is_empty() {
                directory = directory.open_dir(component).unwrap();
            }
        }
        for entry in directory.iter() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name == "." || name == ".." {
                continue;
            }
            let relative_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };
            if entry.is_dir() {
                pending.push(relative_path);
            } else {
                files.push(relative_path.to_uppercase());
            }
        }
    }
    files
}

/// File flags (bit0 = hidden) of the record named `target` at the ROOT of
/// the namespace whose descriptor sits at byte offset `descriptor_offset`
/// (ECMA-119 8.4: the root directory record starts at descriptor offset 156).
fn root_record_flags(
    image: &Image,
    descriptor_offset: usize,
    target: &str,
    joliet: bool,
) -> Option<u8> {
    let descriptor = &image.img[descriptor_offset..descriptor_offset + SECTOR_BYTES];
    let root_lba = read_u32_le(descriptor, 156 + 2);
    let root_length = read_u32_le(descriptor, 156 + 10);
    let root_records = directory_records(&image.img, root_lba, root_length);
    root_records.iter().find_map(|record| {
        let name = decode_identifier(&record.identifier, joliet)?;
        let matches = if joliet {
            name == target
        } else {
            name == target.to_uppercase()
        };
        if matches {
            Some(record.flags)
        } else {
            None
        }
    })
}

/// El Torito boot entries: (media_type, load_rba).
fn boot_entries(image: &Image) -> Vec<(u8, u32)> {
    let Some(b) = image.boot_descriptor_lba else {
        return vec![];
    };
    let vd = &image.img[b * SECTOR_BYTES..(b + 1) * SECTOR_BYTES];
    let catalog = read_u32_le(vd, 71) as usize;
    let sec = &image.img[catalog * SECTOR_BYTES..(catalog + 1) * SECTOR_BYTES];
    let mut out = Vec::new();
    for i in (32..SECTOR_BYTES).step_by(32) {
        let block = &sec[i..i + 32];
        if block[0] == 0x88 {
            out.push((block[1], read_u32_le(block, 8)));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

/// Default (keep-parent) build with the El Torito FAT container: every
/// payload file goes into esp.img verbatim — original paths, no injected
/// boot file — and esp.img is ALWAYS generated, whether or not the payload
/// contains any boot program.
#[test]
fn default_keep_parent_packs_esp() {
    let fixture = Fixture::new();
    let payload = sorted_payload(&fixture);
    fixture.file("EFI/BOOT/BOOTX64.EFI", b"a user-provided efi\n"); // treated as plain data
    let out = fixture.dir.join("out.iso");
    let sum = build_iso(
        &out,
        std::slice::from_ref(&fixture.pkg),
        &Options::default(),
    )
    .unwrap();
    assert!(sum.bootable);
    assert_eq!(sum.files, payload.len() as u64 + 1);

    let image = parse_image(&out);
    let joliet_tree = walk(&image.img, joliet_lba(&image).unwrap() * SECTOR_BYTES, true);

    // ISO9660(+Joliet) data tree: payload under pkg/, plus engine artifacts
    let mut expected: Vec<(String, Vec<u8>)> = payload
        .iter()
        .map(|(relative_path, data)| (format!("pkg/{}", relative_path), data.clone()))
        .collect();
    expected.push((
        "pkg/EFI/BOOT/BOOTX64.EFI".to_string(),
        b"a user-provided efi\n".to_vec(),
    ));
    for (relative_path, data) in &expected {
        let (_, size, ext) = joliet_tree
            .get(relative_path)
            .copied()
            .unwrap_or_else(|| panic!("missing in joliet: {}", relative_path));
        assert_eq!(size, data.len() as u64, "size of {}", relative_path);
        assert_eq!(
            &content(&image.img, ext, size),
            data,
            "content of {}",
            relative_path
        );
    }
    let jfiles: Vec<String> = joliet_tree
        .iter()
        .filter(|(_, (is_directory, _, _))| !*is_directory)
        .map(|(relative_path, _)| relative_path.clone())
        .filter(|k| k != "boot.catalog" && k != "esp.img")
        .collect();
    assert_eq!(jfiles.len(), expected.len(), "{:?}", jfiles);

    // El Torito points at esp.img
    let (_, esp_size, esp_ext) = joliet_tree
        .get("esp.img")
        .copied()
        .expect("esp.img present");
    assert!(esp_size > 0);
    assert_eq!(
        &content(&image.img, esp_ext, esp_size)[510..512],
        &[0x55, 0xAA]
    );
    assert!(
        boot_entries(&image)
            .iter()
            .any(|(m, rba)| *m == 0 && *rba == esp_ext),
        "El Torito entry at esp.img"
    );

    // Engine artifacts are HIDDEN in both namespaces (bit0 of file flags);
    // the user's own files are not.
    for artifact in ["esp.img", "boot.catalog"] {
        let base_flags =
            root_record_flags(&image, image.primary_lba * SECTOR_BYTES, artifact, false)
                .expect("artifact in base tree");
        assert_eq!(base_flags & 1, 1, "{} hidden in base tree", artifact);
        let jol_flags = root_record_flags(
            &image,
            joliet_lba(&image).unwrap() * SECTOR_BYTES,
            artifact,
            true,
        )
        .expect("artifact in joliet tree");
        assert_eq!(jol_flags & 1, 1, "{} hidden in joliet tree", artifact);
    }

    // FAT read-back: esp.img == payload, verbatim, no additions
    let esp_bytes = content(&image.img, esp_ext, esp_size);
    let mut actual = list_fat_files(esp_bytes);
    let mut want: Vec<String> = expected
        .iter()
        .map(|(relative_path, _)| relative_path.to_uppercase())
        .collect();
    want.sort();
    actual.sort();
    assert_eq!(
        actual, want,
        "esp.img mirrors payload exactly (no injected files)"
    );
    assert_eq!(sum.label, "OUT");
}

/// --flat + esp: files at the image root, esp.img mirrors them at root.
#[test]
fn flat_packs_esp_at_root() {
    let fixture = Fixture::new();
    let payload = sorted_payload(&fixture);
    let out = fixture.dir.join("out.iso");
    let opts = Options {
        flat: true,
        ..Options::default()
    };
    let sum = build_iso(&out, std::slice::from_ref(&fixture.pkg), &opts).unwrap();
    assert!(sum.bootable);

    let image = parse_image(&out);
    let joliet_tree = walk(&image.img, joliet_lba(&image).unwrap() * SECTOR_BYTES, true);
    let (_, esp_size, esp_ext) = joliet_tree
        .get("esp.img")
        .copied()
        .expect("esp.img present");
    let esp_bytes = content(&image.img, esp_ext, esp_size);
    let mut actual = list_fat_files(esp_bytes);
    let mut want: Vec<String> = payload
        .iter()
        .map(|(relative_path, _)| relative_path.to_uppercase())
        .collect();
    want.sort();
    actual.sort();
    assert_eq!(actual, want, "flat esp.img mirrors payload at root");
}

/// --no-eltorito: plain ISO9660+Joliet data disc, no esp.img, no boot.
#[test]
fn no_eltorito_is_plain_data_disc() {
    let fixture = Fixture::new();
    let payload = sorted_payload(&fixture);
    let out = fixture.dir.join("out.iso");
    let opts = Options {
        flat: true,
        no_eltorito: true,
        ..Options::default()
    };
    let sum = build_iso(&out, std::slice::from_ref(&fixture.pkg), &opts).unwrap();
    assert!(!sum.bootable);
    let image = parse_image(&out);
    let joliet_tree = walk(&image.img, joliet_lba(&image).unwrap() * SECTOR_BYTES, true);
    assert!(
        !joliet_tree.contains_key("esp.img"),
        "no esp.img on --no-eltorito"
    );
    assert!(boot_entries(&image).is_empty());
    let files: Vec<String> = joliet_tree
        .iter()
        .filter(|(_, (is_directory, _, _))| !*is_directory)
        .map(|(relative_path, _)| relative_path.clone())
        .collect();
    assert_eq!(files.len(), payload.len(), "{:?}", files);
    for (relative_path, data) in &payload {
        let (_, size, ext) = joliet_tree
            .get(relative_path)
            .copied()
            .unwrap_or_else(|| panic!("missing {}", relative_path));
        assert_eq!(size, data.len() as u64);
        assert_eq!(
            &content(&image.img, ext, size),
            data,
            "content of {}",
            relative_path
        );
    }
}

/// Error paths: output overwriting a payload file and duplicate merged
/// root names.
#[test]
fn error_paths() {
    let fixture = Fixture::new();
    sorted_payload(&fixture);
    let out = fixture.dir.join("out.iso");

    let err = build_iso(
        &fixture.pkg.join("readme.txt"),
        std::slice::from_ref(&fixture.pkg),
        &Options::default(),
    )
    .unwrap_err();
    assert!(err.contains("would overwrite"), "{}", err);

    let a = fixture.pkg.join("a");
    let b = fixture.pkg.join("b");
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

/// Same-named subdirectory arriving from two different --flat inputs: both
/// `a/sub` and `b/sub` contents merge into image dir `sub`, so the second
/// one would silently clobber/merge into the first (the old collector only
/// guarded root-level FILES, not directories or nested names). Also covers
/// case-folding collisions: `sub/X.TXT` + `sub/x.txt` inside one image dir
/// fold to the same ISO9660/FAT name and must reject the build.
#[test]
fn case_fold_collision_inside_merged_dir_rejected() {
    let fixture = Fixture::new();
    let a = fixture.pkg.join("a");
    let b = fixture.pkg.join("b");
    fs::create_dir_all(a.join("sub")).unwrap();
    fs::create_dir_all(b.join("sub")).unwrap();
    fs::write(a.join("sub/X.TXT"), b"1").unwrap();
    fs::write(b.join("sub/x.txt"), b"2").unwrap();
    let out = fixture.dir.join("out.iso");
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
    assert!(err.contains("duplicate"), "{}", err);
    assert!(err.contains("sub"), "{}", err);
}

/// Label cleaning and summary sanity.
#[test]
fn label_and_summary() {
    let fixture = Fixture::new();
    sorted_payload(&fixture);
    let out = fixture.dir.join("out.iso");
    let opts = Options {
        flat: true,
        label: Some("My Tools 2024!".to_string()),
        no_eltorito: true,
    };
    let sum = build_iso(&out, std::slice::from_ref(&fixture.pkg), &opts).unwrap();
    assert_eq!(sum.label, "MY_TOOLS_2024_");
    assert_eq!(sum.files, 9);
    assert!(sum.dirs >= 4, "dirs counted: {}", sum.dirs);
    assert!(sum.sectors > 0);
    assert_eq!(
        fs::metadata(&out).unwrap().len(),
        sum.sectors as u64 * SECTOR_BYTES as u64
    );
}
