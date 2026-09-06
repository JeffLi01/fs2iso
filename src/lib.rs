//! fs2iso — pack files/directories into an optical image for BMC virtual
//! media / UEFI shell use.
//!
//! Every payload file/directory is packed into a **FAT image (`esp.img`)** —
//! original names, original structure, no special-casing (it does not matter
//! whether some file is an EFI boot program). The El Torito entry points at
//! `esp.img`, so firmware loads/exposes the FAT volume and the EFI shell
//! sees the payload as `fs0`/`fsX`. The same payload is ALSO kept in the
//! ISO9660(+Joliet) data tree of the disc (base = uppercase names for
//! ISO9660-only readers; Joliet = original names incl. Chinese) for readers
//! that mount the disc data directly (Windows, ISO9660-capable firmware
//! shells). UDF is not used (hadris-cd's UDF layer is not spec-complete).
//!
//! `--no-eltorito` produces a plain ISO9660+Joliet data disc without the
//! FAT container.
//!
//! This crate owns the CLI-facing semantics: keep-parent / --flat payload
//! collection, duplicate and overwrite guards and the build summary.

mod esp;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use hadris_cd::{
    Directory, FileEntry, FileTree, OpticalImageOptions, OpticalImageWriter,
};

pub struct Options {
    /// Volume label (cleaned). None -> derived from the output file name.
    pub label: Option<String>,
    /// mkisofs-style: directory arguments merge their *contents* into the root.
    pub flat: bool,
    /// Do not pack the FAT container / add an El Torito entry — produce a
    /// plain ISO9660+Joliet data disc.
    pub no_eltorito: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            label: None,
            flat: false,
            no_eltorito: false,
        }
    }
}

#[derive(Debug)]
pub struct BuildSummary {
    pub label: String,
    pub dirs: u64,
    pub files: u64,
    pub payload_bytes: u64,
    pub sectors: u64,
    /// True when the disc carries a FAT container (esp.img) with an El
    /// Torito boot entry.
    pub bootable: bool,
}

// ---------------------------------------------------------------------------
// payload collection -> hadris-cd Directory tree
// ---------------------------------------------------------------------------

struct FileRec {
    rel: String, // ISO-relative path, forward slashes
    src: PathBuf,
}

struct Collected {
    tree: FileTree,
    recs: Vec<FileRec>,
    dirs: u64,
    payload_bytes: u64,
}

/// ASCII-uppercased name: collision detection across merged inputs only.
fn fold(name: &str) -> String {
    name.chars().map(|c| c.to_ascii_uppercase()).collect()
}

/// Add a whole directory's contents to the hadris tree.
/// `root` is the node receiving children; `iso_prefix` is the rel path of
/// `disk_dir` inside the image (empty => contents go to the image root).
fn add_dir_contents(
    root: &mut Directory,
    disk_dir: &Path,
    iso_prefix: &str,
    depth: usize,
    visited: &mut std::collections::HashSet<PathBuf>,
    recs: &mut Vec<FileRec>,
    dirs: &mut u64,
    payload: &mut u64,
) -> Result<(), String> {
    if depth > 64 {
        return Err(format!(
            "directory nesting too deep under {:?} (possible link cycle?)",
            disk_dir
        ));
    }
    let canon = std::fs::canonicalize(disk_dir)
        .map_err(|e| format!("cannot resolve {:?}: {}", disk_dir, e))?;
    if !visited.insert(canon.clone()) {
        return Err(format!(
            "directory cycle detected at {:?} (re-entered a junction/link)",
            disk_dir
        ));
    }

    let mut names: Vec<String> = std::fs::read_dir(disk_dir)
        .map_err(|e| format!("cannot read directory {:?}: {}", disk_dir, e))?
        .map(|e| {
            e.map(|e| e.file_name().to_string_lossy().into_owned())
                .map_err(|e| format!("read_dir error in {:?}: {}", disk_dir, e))
        })
        .collect::<Result<_, _>>()?;
    names.sort();

    for name in names {
        let full = disk_dir.join(&name);
        let rel = if iso_prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", iso_prefix, name)
        };
        let meta = std::fs::metadata(&full).map_err(|e| format!("cannot stat {:?}: {}", full, e))?;
        if meta.is_dir() {
            *dirs += 1;
            let mut sub = Directory::new(name);
            add_dir_contents(
                &mut sub,
                &full,
                &rel,
                depth + 1,
                visited,
                recs,
                dirs,
                payload,
            )?;
            root.add_subdir(sub);
        } else {
            let size = meta.len();
            *payload += size;
            recs.push(FileRec {
                rel: rel.clone(),
                src: full.clone(),
            });
            let _ = size;
            root.add_file(FileEntry::from_path(name, full));
        }
    }
    visited.remove(&canon);
    Ok(())
}

fn input_name(input: &Path) -> Result<String, String> {
    Ok(input
        .file_name()
        .ok_or_else(|| format!("bad input path {:?}", input))?
        .to_string_lossy()
        .into_owned())
}

fn collect(inputs: &[PathBuf], flat: bool) -> Result<Collected, String> {
    let mut root = Directory::root();
    let mut recs = Vec::new();
    let mut dirs = 0u64;
    let mut payload = 0u64;
    let mut root_names: HashMap<String, PathBuf> = HashMap::new();
    let mut visited = std::collections::HashSet::new();

    for input in inputs {
        let meta =
            std::fs::metadata(input).map_err(|e| format!("cannot access {:?}: {}", input, e))?;
        if !meta.is_dir() {
            let n = input_name(input)?;
            let key = fold(&n);
            if let Some(prev) = root_names.get(&key) {
                return Err(format!(
                    "duplicate root name '{}' from {:?} and {:?}",
                    n, prev, input
                ));
            }
            root_names.insert(key, input.clone());
            let size = meta.len();
            payload += size;
            recs.push(FileRec {
                rel: n.clone(),
                src: input.clone(),
            });
            root.add_file(FileEntry::from_path(n, input.clone()));
            continue;
        }

        // A directory input normally keeps its name at the root
        // (keep-parent). Unnameable paths (".", "..", filesystem roots —
        // no file_name) merge their contents into the root instead, matching
        // mkisofs semantics for `fs2iso out.iso .`.
        let keep_parent = !flat
            && match input_name(input) {
                Ok(n) => n != "." && n != "..",
                Err(_) => false,
            };
        if !keep_parent {
            let before = recs.len();
            add_dir_contents(
                &mut root, input, "", 0, &mut visited, &mut recs, &mut dirs, &mut payload,
            )?;
            for r in &recs[before..] {
                if !r.rel.contains('/') {
                    let key = fold(&r.rel);
                    if let Some(prev) = root_names.get(&key) {
                        return Err(format!(
                            "duplicate root name '{}' from {:?} and {:?}",
                            r.rel, prev, r.src
                        ));
                    }
                    root_names.insert(key, r.src.clone());
                }
            }
        } else {
            let n = input_name(input)?;
            let key = fold(&n);
            if let Some(prev) = root_names.get(&key) {
                return Err(format!(
                    "duplicate root name '{}' from {:?} and {:?}",
                    n, prev, input
                ));
            }
            root_names.insert(key, input.clone());
            dirs += 1;
            let mut sub = Directory::new(n.clone());
            add_dir_contents(
                &mut sub,
                input,
                &n,
                0,
                &mut visited,
                &mut recs,
                &mut dirs,
                &mut payload,
            )?;
            root.add_subdir(sub);
        }
    }

    if recs.is_empty() {
        return Err("no files found in the inputs".to_string());
    }
    root.sort();
    Ok(Collected {
        tree: FileTree { root },
        recs,
        dirs,
        payload_bytes: payload,
    })
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn canonical(p: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(p).map_err(|e| format!("cannot resolve {:?}: {}", p, e))
}

fn canonical_maybe_missing(p: &Path) -> Result<PathBuf, String> {
    if p.exists() {
        return canonical(p);
    }
    let parent = p
        .parent()
        .filter(|s| !s.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let base = p.file_name().ok_or_else(|| format!("bad path {:?}", p))?;
    Ok(canonical(parent)?.join(base))
}

/// Volume labels must be printable: ASCII alnum kept (uppercased), everything
/// else becomes '_'. (UDF/ISO share the label.)
pub(crate) fn sanitize_label(raw: &str) -> String {
    let mut out = String::new();
    for b in raw.bytes() {
        match b {
            b'0'..=b'9' | b'A'..=b'Z' => out.push(b as char),
            b'a'..=b'z' => out.push((b - 32) as char),
            b' ' => out.push('_'),
            _ => out.push('_'),
        }
        if out.len() >= 32 {
            break;
        }
    }
    if out.is_empty() {
        out.push_str("FS2ISO");
    }
    out
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

pub fn build_iso(
    output: &Path,
    inputs: &[PathBuf],
    opts: &Options,
) -> Result<BuildSummary, String> {
    if inputs.is_empty() {
        return Err("no input paths given".to_string());
    }

    let mut collected = collect(inputs, opts.flat)?;

    // refuse to clobber a payload file with the output image
    let out_canon = canonical_maybe_missing(output)?;
    for r in &collected.recs {
        if canonical(&r.src)? == out_canon {
            return Err(format!(
                "output image {:?} would overwrite a payload file",
                output
            ));
        }
    }

    let label = match &opts.label {
        Some(l) => sanitize_label(l),
        None => match output.file_stem() {
            Some(stem) => sanitize_label(&stem.to_string_lossy()),
            None => "FS2ISO".to_string(),
        },
    };

    let mut image_options = OpticalImageOptions::default();
    image_options.volume_id = label.clone();
    image_options.udf.enabled = false;

    // === FAT container (esp.img) holds EVERY payload file ===
    // The user's files/directories are packed into a FAT image (esp.img)
    // verbatim — original names, original structure, whether or not any of
    // them is an EFI boot program. The El Torito entry points at esp.img so
    // firmware loads/exposes the FAT volume (shell flows then see the files
    // as fs0/fsX). The same payload also stays in the ISO9660(+Joliet) data
    // tree for readers that mount the disc data directly (Windows,
    // ISO9660-capable firmware shells). Nothing is added, renamed or
    // special-cased. --no-eltorito produces a plain data disc without the
    // FAT container.
    let bootable = !opts.no_eltorito;
    if bootable {
        if collected.recs.iter().any(|r| fold(&r.rel) == "ESP.IMG") {
            return Err(
                "payload contains a root-level file named 'esp.img', which is \
                 reserved for the generated FAT container; rename it"
                    .to_string(),
            );
        }
        let mut esp_entries: Vec<(String, Vec<u8>)> =
            Vec::with_capacity(collected.recs.len());
        for r in &collected.recs {
            let b = std::fs::read(&r.src)
                .map_err(|e| format!("cannot read {:?}: {}", r.src, e))?;
            esp_entries.push((r.rel.clone(), b));
        }
        let esp_bytes = esp::build_esp(&esp_entries)?;
        collected
            .tree
            .root
            .add_file(hadris_cd::FileEntry::from_buffer("esp.img", esp_bytes));
        collected.tree.root.sort();

        use hadris_iso::boot::options::{BootEntryOptions, BootOptions, BootSectionOptions};
        use hadris_iso::boot::{EmulationType, PlatformId};
        let entry = BootEntryOptions {
            load_size: None,
            boot_image_path: "esp.img".to_string(),
            boot_info_table: false,
            grub2_boot_info: false,
            emulation: EmulationType::NoEmulation,
        };
        image_options.boot = Some(BootOptions {
            write_boot_catalog: true,
            default: entry.clone(),
            entries: vec![(
                BootSectionOptions {
                    platform: PlatformId::UEFI,
                },
                entry,
            )],
        });
    }

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)
        .map_err(|e| format!("cannot create {:?}: {}", output, e))?;
    OpticalImageWriter::create(file, collected.tree, image_options)
        .map_err(|e| format!("image build failed: {}", e))?;

    let sectors = std::fs::metadata(output)
        .map(|m| m.len() / 2048)
        .unwrap_or(0);

    Ok(BuildSummary {
        label,
        dirs: collected.dirs,
        files: collected.recs.len() as u64,
        payload_bytes: collected.payload_bytes,
        sectors,
        bootable,
    })
}

#[cfg(test)]
mod tests {
    use super::sanitize_label;

    #[test]
    fn sanitize_ascii() {
        assert_eq!(sanitize_label("my iso 2024!"), "MY_ISO_2024_");
        assert_eq!(sanitize_label("中文标签"), "____________");
        assert_eq!(sanitize_label(""), "FS2ISO");
        assert_eq!(sanitize_label("a"), "A");
        let long = sanitize_label(&"x".repeat(80));
        assert_eq!(long.len(), 32);
        assert!(long.bytes().all(|b| b == b'X'));
    }
}
