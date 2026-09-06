//! fs2iso — pack files/directories into an optical image (BMC virtual media /
//! UEFI shell use).
//!
//! The image is produced by the [`hadris_cd`] crate (pure Rust) as a
//! **ISO9660 + Joliet** disc: base namespace for legacy/ISO9660-only readers,
//! Joliet (Windows, original names incl. Chinese). UDF is intentionally
//! disabled (see `build_iso`): hadris-cd's UDF layer is not spec-complete
//! (missing ECMA-167 file-set terminator) — EDK2's UdfDxe reads it, but
//! Windows' udfs.sys rejects the whole volume, so a UDF bridge would not
//! mount on Windows at all.
//!
//! This crate owns the CLI-facing semantics: keep-parent / --flat payload
//! collection, duplicate and overwrite guards, El Torito boot-file
//! resolution and the build summary.

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
    /// Explicit El Torito boot file (must be part of the payload).
    pub boot_efi: Option<PathBuf>,
    /// Disable El Torito boot entirely.
    pub no_eltorito: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            label: None,
            flat: false,
            boot_efi: None,
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
    /// ISO-relative path of the El Torito boot file, if any.
    pub boot_path: Option<String>,
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

        if flat {
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

fn resolve_boot(opts: &Options, recs: &[FileRec]) -> Result<Option<String>, String> {
    if opts.no_eltorito {
        return Ok(None);
    }
    let rel = if let Some(bf) = &opts.boot_efi {
        let want = canonical(bf)?;
        recs.iter()
            .find(|r| canonical(&r.src).map(|c| c == want).unwrap_or(false))
            .map(|r| r.rel.clone())
            .ok_or_else(|| {
                format!(
                    "--boot-efi {:?} is not part of the payload (add it as an input)",
                    bf
                )
            })?
    } else {
        match recs
            .iter()
            .find(|r| r.rel.to_lowercase().ends_with("efi/boot/bootx64.efi"))
        {
            Some(r) => r.rel.clone(),
            None => return Ok(None),
        }
    };
    Ok(Some(rel))
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

    let boot_rel = resolve_boot(opts, &collected.recs)?;
    let mut image_options = OpticalImageOptions::default();
    image_options.volume_id = label.clone();
    // UDF layer disabled: hadris-cd's UDF metadata is incomplete (missing
    // ECMA-167 file-set terminator). EDK2's UdfDxe tolerates it (QEMU
    // acceptance passed) but Windows' udfs.sys rejects the whole volume —
    // verified on this host: bridge images mount as a drive but are
    // unreadable, while ISO9660(+Joliet) images from the same writer mount
    // fine. Default output is therefore plain ISO9660 + Joliet, which
    // Windows and mainstream (AMI-class) BMC firmware both read. The UDF
    // bridge variant lives in git history (commit 9f17b72) until the UDF
    // writer is spec-complete.
    image_options.udf.enabled = false;

    // === EFI-shell medium: payload goes into a FAT container too ===
    // The disc is bootable by default: the El Torito entry loads a generated
    // FAT image (esp.img) whose volume mirrors the ISO root and carries every
    // payload file, so EDK2 firmware — which has no ISO9660 data driver —
    // shows the payload in the shell as fs0 after boot. --no-eltorito keeps
    // a plain ISO9660+Joliet data disc (no FAT, no boot entry).
    let boot_path_out: Option<String> = if opts.no_eltorito {
        None
    } else {
        let rel = match &boot_rel {
            Some(r) => r.clone(),
            None => {
                return Err(
                    "no boot file: put EFI/BOOT/BOOTX64.EFI in the payload or pass \
                     --boot-efi <file> (this tool builds bootable EFI-shell media; \
                     use --no-eltorito for a plain data disc)"
                        .to_string(),
                )
            }
        };
        if collected.recs.iter().any(|r| fold(&r.rel) == "ESP.IMG") {
            return Err(
                "payload contains 'esp.img' which is reserved for the generated \
                 EFI boot container; rename it"
                    .to_string(),
            );
        }

        // mirror every payload file into the FAT container
        let mut esp_entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(collected.recs.len() + 1);
        for r in &collected.recs {
            let b = std::fs::read(&r.src)
                .map_err(|e| format!("cannot read {:?}: {}", r.src, e))?;
            esp_entries.push((r.rel.clone(), b));
        }
        // guaranteed standard boot path (wins over a mirrored same-name file)
        const STD_BOOT: &str = "EFI/BOOT/BOOTX64.EFI";
        let boot_bytes = esp_entries
            .iter()
            .find(|(pp, _)| fold(pp) == fold(STD_BOOT))
            .or_else(|| esp_entries.iter().find(|(pp, _)| *pp == rel))
            .map(|(_, b)| b.clone())
            .ok_or_else(|| format!("boot file record missing: {}", rel))?;
        if !esp_entries.iter().any(|(pp, _)| fold(pp) == fold(STD_BOOT)) {
            esp_entries.push((STD_BOOT.to_string(), boot_bytes.clone()));
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
        Some(rel)
    };

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
        boot_path: boot_path_out,
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
