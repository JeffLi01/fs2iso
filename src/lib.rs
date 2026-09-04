//! fs2iso — pack files/directories into an ISO9660 image (BMC virtual media /
//! UEFI shell use).
//!
//! The image is written by the [`hadris_iso`] crate (pure Rust; ISO9660 base
//! tree + Joliet original-name tree, optional El Torito EFI boot). This crate
//! owns the CLI-facing semantics: payload collection (keep-parent or --flat),
//! naming-agnostic file tree, boot-image resolution and the build summary.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use hadris_iso::boot::options::{BootEntryOptions, BootOptions, BootSectionOptions};
use hadris_iso::boot::{EmulationType, PlatformId};
use hadris_iso::joliet::JolietLevel;
use hadris_iso::read::PathSeparator;
use hadris_iso::write::options::{BaseIsoLevel, CreationFeatures, IsoFormatOptions};
use hadris_iso::write::{InputEntry, InputTree, IsoImageWriter};

pub struct Options {
    /// Volume label (cleaned). None -> derived from the output file name.
    pub label: Option<String>,
    /// mkisofs-style: directory arguments merge their *contents* into the root.
    pub flat: bool,
    /// Explicit El Torito boot file (must be part of the payload).
    pub boot_efi: Option<PathBuf>,
    /// Disable automatic El Torito detection of efi/boot/bootx64.efi.
    pub no_eltorito: bool,
    /// Disable the Joliet (UCS-2) name space.
    pub no_joliet: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            label: None,
            flat: false,
            boot_efi: None,
            no_eltorito: false,
            no_joliet: false,
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
    pub joliet: bool,
}

// ---------------------------------------------------------------------------
// payload collection
// ---------------------------------------------------------------------------

/// One collected file: its ISO-relative path (forward-slash), source path and
/// byte size. Used for boot resolution and the build summary.
struct FileRec {
    rel: String,
    src: PathBuf,
    size: u64,
}

struct Collected {
    root_entries: Vec<InputEntry>,
    files: Vec<FileRec>,
    dirs: u64,
    payload_bytes: u64,
}

fn walk_dir(
    disk_dir: &Path,
    iso_prefix: &str,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    files: &mut Vec<FileRec>,
    dirs: &mut u64,
    payload_bytes: &mut u64,
) -> Result<Vec<InputEntry>, String> {
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

    let mut names: Vec<_> = std::fs::read_dir(disk_dir)
        .map_err(|e| format!("cannot read directory {:?}: {}", disk_dir, e))?
        .map(|ent| {
            ent.map(|e| e.file_name().to_string_lossy().into_owned())
                .map_err(|e| format!("read_dir error in {:?}: {}", disk_dir, e))
        })
        .collect::<Result<_, _>>()?;
    names.sort();

    let mut entries = Vec::new();
    for name in names {
        let full = disk_dir.join(&name);
        let rel = if iso_prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", iso_prefix, name)
        };
        let meta =
            std::fs::metadata(&full).map_err(|e| format!("cannot stat {:?}: {}", full, e))?;
        if meta.is_dir() {
            *dirs += 1;
            let children = walk_dir(&full, &rel, depth + 1, visited, files, dirs, payload_bytes)?;
            entries.push(InputEntry::directory(name, children));
        } else {
            let data =
                std::fs::read(&full).map_err(|e| format!("cannot read {:?}: {}", full, e))?;
            *payload_bytes += data.len() as u64;
            files.push(FileRec {
                rel,
                src: full,
                size: data.len() as u64,
            });
            entries.push(InputEntry::file(name, data));
        }
    }

    visited.remove(&canon);
    Ok(entries)
}

fn collect(inputs: &[PathBuf], flat: bool) -> Result<Collected, String> {
    let mut out = Collected {
        root_entries: Vec::new(),
        files: Vec::new(),
        dirs: 0,
        payload_bytes: 0,
    };
    // name -> first source that contributed it (duplicate detection across args)
    let mut root_names: std::collections::HashMap<String, PathBuf> =
        std::collections::HashMap::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();

    for input in inputs {
        let meta =
            std::fs::metadata(input).map_err(|e| format!("cannot access {:?}: {}", input, e))?;
        if !meta.is_dir() {
            let name = input
                .file_name()
                .ok_or_else(|| format!("bad input path {:?}", input))?
                .to_string_lossy()
                .into_owned();
            let data =
                std::fs::read(input).map_err(|e| format!("cannot read {:?}: {}", input, e))?;
            if let Some(prev) = root_names.get(&name) {
                return Err(format!(
                    "duplicate root name '{}' from {:?} and {:?}",
                    name, prev, input
                ));
            }
            root_names.insert(name.clone(), input.clone());
            out.payload_bytes += data.len() as u64;
            out.files.push(FileRec {
                rel: name.clone(),
                src: input.clone(),
                size: data.len() as u64,
            });
            out.root_entries.push(InputEntry::file(name, data));
            continue;
        }

        if flat {
            let children = walk_dir(
                input,
                "",
                0,
                &mut visited,
                &mut out.files,
                &mut out.dirs,
                &mut out.payload_bytes,
            )?;
            for ent in children {
                let name = ent.name().to_string();
                if let Some(prev) = root_names.get(&name) {
                    return Err(format!(
                        "duplicate root name '{}' from {:?} and {:?}",
                        name, prev, input
                    ));
                }
                root_names.insert(name, input.clone());
                out.root_entries.push(ent);
            }
        } else {
            let name = input
                .file_name()
                .ok_or_else(|| format!("bad input path {:?}", input))?
                .to_string_lossy()
                .into_owned();
            if let Some(prev) = root_names.get(&name) {
                return Err(format!(
                    "duplicate root name '{}' from {:?} and {:?}",
                    name, prev, input
                ));
            }
            root_names.insert(name.clone(), input.clone());
            out.dirs += 1;
            let children = walk_dir(
                input,
                &name,
                0,
                &mut visited,
                &mut out.files,
                &mut out.dirs,
                &mut out.payload_bytes,
            )?;
            out.root_entries.push(InputEntry::directory(name, children));
        }
    }

    if out.root_entries.is_empty() {
        return Err("no files or directories found in the inputs".to_string());
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn canonical(p: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(p).map_err(|e| format!("cannot resolve {:?}: {}", p, e))
}

/// Canonical form of a path that may not exist yet (output file).
fn canonical_maybe_missing(p: &Path) -> Result<PathBuf, String> {
    if p.exists() {
        return canonical(p);
    }
    let parent = p.parent().unwrap_or_else(|| Path::new("."));
    let base = p.file_name().ok_or_else(|| format!("bad path {:?}", p))?;
    Ok(canonical(parent)?.join(base))
}

/// Volume labels must be printable ISO9660 a-characters; keep it simple:
/// ASCII alnum/space kept (uppercased), everything else becomes '_'.
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

/// Resolve the El Torito boot file to its ISO-relative path.
fn resolve_boot(
    opts: &Options,
    files: &[FileRec],
    collected: &Collected,
) -> Result<Option<String>, String> {
    if opts.no_eltorito {
        return Ok(None);
    }
    let rel = if let Some(bf) = &opts.boot_efi {
        let want = canonical(bf)?;
        files
            .iter()
            .find(|f| canonical(&f.src).map(|c| c == want).unwrap_or(false))
            .map(|f| f.rel.clone())
            .ok_or_else(|| {
                format!(
                    "--boot-efi {:?} is not part of the payload (add it as an input)",
                    bf
                )
            })?
    } else {
        // auto-detect EFI/BOOT/BOOTX64.EFI anywhere in the tree
        match files
            .iter()
            .find(|f| f.rel.to_lowercase().ends_with("efi/boot/bootx64.efi"))
        {
            Some(f) => f.rel.clone(),
            None => return Ok(None),
        }
    };
    // directories can never be boot files (they are not in `files`)
    let _ = collected;
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

    let collected = collect(inputs, opts.flat)?;

    // refuse to clobber a payload file with the output image
    let out_canon = canonical_maybe_missing(output)?;
    for f in &collected.files {
        if canonical(&f.src)? == out_canon {
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

    let boot_rel = resolve_boot(opts, &collected.files, &collected)?;

    let use_joliet = !opts.no_joliet;
    let el_torito = boot_rel.as_ref().map(|rel| {
        let entry = BootEntryOptions {
            load_size: None,
            boot_image_path: rel.clone(),
            boot_info_table: false,
            grub2_boot_info: false,
            emulation: EmulationType::NoEmulation,
        };
        BootOptions {
            write_boot_catalog: true,
            default: entry.clone(),
            entries: vec![(
                BootSectionOptions {
                    platform: PlatformId::UEFI,
                },
                entry,
            )],
        }
    });

    let options = IsoFormatOptions {
        volume_name: label.clone(),
        system_id: Some("FS2ISO".to_string()),
        volume_set_id: None,
        publisher_id: None,
        preparer_id: None,
        application_id: None,
        sector_size: 2048,
        path_separator: PathSeparator::ForwardSlash,
        features: CreationFeatures {
            filenames: BaseIsoLevel::Level2 {
                supports_lowercase: false,
                supports_rrip: false,
            },
            long_filenames: false,
            joliet: if use_joliet {
                Some(JolietLevel::Level3)
            } else {
                None
            },
            rock_ridge: None,
            el_torito,
            hybrid_boot: None,
        },
        strict_charset: false,
    };

    let tree = InputTree::new(PathSeparator::ForwardSlash, collected.root_entries);
    // the writer needs read+write+seek access on the output
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)
        .map_err(|e| format!("cannot create {:?}: {}", output, e))?;
    let mut file = IsoImageWriter::create(file, tree, options)
        .map_err(|e| format!("ISO build failed: {}", e))?;
    file.flush().map_err(|e| format!("flush error: {}", e))?;

    let sectors = std::fs::metadata(output)
        .map(|m| m.len() / 2048)
        .unwrap_or(0);

    Ok(BuildSummary {
        label,
        dirs: collected.dirs,
        files: collected.files.len() as u64,
        payload_bytes: collected.payload_bytes,
        sectors,
        boot_path: boot_rel,
        joliet: use_joliet,
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
