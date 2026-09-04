//! ISO9660 / Joliet identifier generation.
//!
//! Every entry lives in two namespaces:
//!  * base ISO9660 tree: uppercase d-characters (A-Z 0-9 _), extension split at the last
//!    dot, <= 30 characters, files carry a ";1" version. This is the namespace every
//!    UEFI firmware / OS *must* understand (ECMA-119 level 2 rules).
//!  * Joliet tree: the original file name encoded as UCS-2 (UTF-16BE), <= 64 units,
//!    no version suffix. Windows and most CD drivers prefer this one for display.
use std::collections::HashSet;

const BASE_NAME_MAX: usize = 30; // ECMA-119 level 2: filename incl. extension+dot <= 30
const BASE_EXT_MAX: usize = 12; // pragmatic cap for the extension part only
const JOLIET_MAX: usize = 64; // Joliet: max 64 UCS-2 units

/// Map one byte to the ISO9660 d-character set (uppercase) or '_' when not allowed.
fn d_map(b: u8) -> u8 {
    match b {
        b'a'..=b'z' => b - 32,
        b'A'..=b'Z' | b'0'..=b'9' | b'_' => b,
        _ => b'_',
    }
}

/// Split a file name into (stem, extension). A leading-dot name (e.g. ".bashrc") is
/// treated as an all-stem name. Only the *last* dot separates stem and extension.
fn split_ext(name: &str) -> (&str, &str) {
    if name.starts_with('.') {
        return (name, "");
    }
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

/// Truncate `s` to `max` bytes while keeping at least one byte.
fn trunc_at_least1(s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut cut = s;
    cut.truncate(max);
    if cut.is_empty() {
        cut.push('_');
    }
    cut
}

/// Compose a <=30-char base name from a mangled stem + extension.
fn compose30(stem: &str, ext: &str) -> String {
    if ext.is_empty() {
        let st = trunc_at_least1(stem.to_string(), BASE_NAME_MAX);
        return st;
    }
    let ext_short: String = ext.chars().take(BASE_EXT_MAX).collect();
    let budget = BASE_NAME_MAX - 1 - ext_short.len(); // >= 1 because ext <= 12 => budget >= 17
    let st = trunc_at_least1(stem.to_string(), budget);
    format!("{}.{}", st, ext_short)
}

/// Base-tree identifier *without* version for a directory.
pub fn base_dir_id(name: &str, used: &mut HashSet<Vec<u8>>) -> Vec<u8> {
    base_id_impl(name, false, used)
}

/// Base-tree identifier (with ";1" version) for a file.
pub fn base_file_id(name: &str, used: &mut HashSet<Vec<u8>>) -> Vec<u8> {
    base_id_impl(name, true, used)
}

fn base_id_impl(name: &str, is_file: bool, used: &mut HashSet<Vec<u8>>) -> Vec<u8> {
    let (stem0, ext0) = if is_file { split_ext(name) } else { (name, "") };
    let stem0 = String::from_utf8(stem0.bytes().map(d_map).collect()).expect("d_map is ASCII");
    let ext0 = String::from_utf8(ext0.bytes().map(d_map).collect()).expect("d_map is ASCII");

    let mut k: u64 = 0;
    loop {
        let stem = if k == 0 {
            stem0.clone()
        } else {
            // make room for the "_<k>" uniqueness suffix inside the length budget
            let base = format!("{}_{}", stem0, k);
            let room = if ext0.is_empty() {
                BASE_NAME_MAX
            } else {
                BASE_NAME_MAX - 1 - ext0.chars().count().min(BASE_EXT_MAX)
            };
            trunc_at_least1(base, room.max(1))
        };
        let composed = compose30(&stem, &ext0);
        let mut id = Vec::with_capacity(composed.len() + 3);
        id.extend_from_slice(composed.as_bytes());
        if is_file {
            id.extend_from_slice(b";1");
        }
        if used.insert(id.clone()) {
            return id;
        }
        k += 1;
        if k > 100_000 {
            panic!("cannot uniquify base identifier for {:?}", name);
        }
    }
}

/// Is this UCS-2 code unit allowed inside a Joliet name?
fn joliet_ok(u: u16) -> bool {
    match u {
        0x0000..=0x001F => false,
        0x002F => false,                                                       // '/'
        0x005C => false,                                                       // '\\'
        0x003A | 0x002A | 0x003F | 0x0022 | 0x003C | 0x003E | 0x007C => false, // : * ? " < > |
        0x007F..=0x009F => false,
        _ => true,
    }
}

/// Encode `s` as UTF-16BE with forbidden characters replaced by '_' and capped at
/// JOLIET_MAX units. A trailing lone surrogate produced by truncation is dropped.
fn joliet_encode(s: &str, max_units: usize) -> Vec<u8> {
    let mut units: Vec<u16> = Vec::new();
    for u in s.encode_utf16() {
        let u = if joliet_ok(u) { u } else { b'_' as u16 };
        units.push(u);
        if units.len() == max_units {
            break;
        }
    }
    // never leave a dangling high surrogate
    let last = units.last().copied();
    if let Some(u) = last {
        if (0xD800..0xDC00).contains(&u) {
            units.pop();
        }
    }
    let mut out = Vec::with_capacity(units.len() * 2);
    for u in units {
        out.extend_from_slice(&u.to_be_bytes());
    }
    out
}

/// Joliet tree identifier (no version). `used` is shared between dirs and files of one
/// parent directory.
pub fn joliet_id(name: &str, used: &mut HashSet<Vec<u8>>) -> Vec<u8> {
    let mut k: u64 = 0;
    loop {
        let id = if k == 0 {
            joliet_encode(name, JOLIET_MAX)
        } else {
            let suffix = format!("_{}", k);
            let room = JOLIET_MAX.saturating_sub(suffix.encode_utf16().count());
            let mut base = joliet_encode(name, room);
            for u in suffix.encode_utf16() {
                base.extend_from_slice(&u.to_be_bytes());
            }
            base
        };
        if used.insert(id.clone()) {
            return id;
        }
        k += 1;
        if k > 100_000 {
            panic!("cannot uniquify joliet identifier for {:?}", name);
        }
    }
}

/// Clean a volume label: uppercase ASCII alnum kept, everything else -> '_', <= 32.
pub fn sanitize_label(raw: &str) -> String {
    let mut out = String::new();
    for b in raw.bytes() {
        match b {
            b'a'..=b'z' => out.push((b - 32) as char),
            b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' => out.push(b as char),
            _ => out.push('_'),
        }
        if out.len() == 32 {
            break;
        }
    }
    if out.is_empty() {
        out.push_str("FS2ISO");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(name: &str) -> String {
        let mut used = HashSet::new();
        String::from_utf8(base_file_id(name, &mut used)).unwrap()
    }

    #[test]
    fn simple() {
        assert_eq!(base("readme.txt"), "README.TXT;1");
        assert_eq!(base("BOOTX64.EFI"), "BOOTX64.EFI;1");
        assert_eq!(base("startup.nsh"), "STARTUP.NSH;1");
    }

    #[test]
    fn spaces_and_symbols() {
        // space + '(' are adjacent => double underscore
        assert_eq!(base("my long name (x).efi"), "MY_LONG_NAME__X_.EFI;1");
        assert_eq!(base("a.b.c"), "A_B.C;1");
    }

    #[test]
    fn chinese_falls_back() {
        let id = base("固件更新.bin");
        // UTF-8: each CJK char is 3 bytes => run of '_' (byte-wise d-char mapping)
        assert!(id.starts_with('_'));
        assert!(id.ends_with(".BIN;1"));
        // joliet keeps it intact
        let mut used = HashSet::new();
        let j = joliet_id("固件更新.bin", &mut used);
        let s = String::from_utf16(
            &j.chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert_eq!(s, "固件更新.bin");
    }

    #[test]
    fn hidden_file() {
        assert_eq!(base(".profile"), "_PROFILE;1");
    }

    #[test]
    fn long_name_truncated() {
        let id = base("this_is_a_very_long_file_name_that_should_be_cut.efi");
        assert!(id.len() <= BASE_NAME_MAX + 3); // + ";1"
        assert!(id.ends_with(".EFI;1") || id.ends_with(";1"));
    }

    #[test]
    fn case_collision_unique() {
        let mut used = HashSet::new();
        let a = base_file_id("Readme.TXT", &mut used);
        let b = base_file_id("readme.txt", &mut used);
        assert_ne!(a, b);
        assert!(a.starts_with(b"README.TXT;1"));
        assert_eq!(b, b"README_1.TXT;1");
    }

    #[test]
    fn label() {
        assert_eq!(sanitize_label("my iso 2024!"), "MY_ISO_2024_");
        assert_eq!(sanitize_label("中文标签"), "____________");
        assert_eq!(sanitize_label(""), "FS2ISO");
    }

    #[test]
    fn joliet_len_cap() {
        let mut used = HashSet::new();
        let long = "x".repeat(100);
        let id = joliet_id(&long, &mut used);
        assert!(id.len() <= JOLIET_MAX * 2);
    }
}
