//! fs2iso — pack files/directories into an ISO9660 image (BMC virtual media /
//! UEFI shell use).
//!
//! The image is produced by the [`isobemak`] crate (pure Rust, UEFI/BIOS
//! El Torito aware). isobemak writes a single ISO9660 base namespace (ASCII
//! names uppercased, other bytes kept as-is) and no Joliet tree; this crate
//! supplies the CLI-facing semantics (keep-parent / --flat collection,
//! duplicate guards, boot-file resolution, summary) and then runs a
//! conformance pass ([`iso_fix`]) that appends ISO9660 path tables and fixes
//! the PVD both-endian fields, which isobemak 0.4.x leaves broken.

pub mod iso_fix;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use isobemak::{
    build_iso as isobemak_build, BootInfo, IsoImage, IsoImageFile, IsoLayoutProfile, UefiBootInfo,
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
// payload collection
// ---------------------------------------------------------------------------

struct FileRec {
    rel: String, // ISO-relative path, forward slashes
    src: PathBuf,
}

struct Collected {
    files: Vec<IsoImageFile>,
    recs: Vec<FileRec>,
    dirs: u64,
    payload_bytes: u64,
}

fn walk_dir(
    disk_dir: &Path,
    iso_prefix: &str,
    depth: usize,
    visited: &mut std::collections::HashSet<PathBuf>,
    out: &mut Vec<IsoImageFile>,
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
            name
        } else {
            format!("{}/{}", iso_prefix, name)
        };
        let meta =
            std::fs::metadata(&full).map_err(|e| format!("cannot stat {:?}: {}", full, e))?;
        if meta.is_dir() {
            *dirs += 1;
            walk_dir(&full, &rel, depth + 1, visited, out, recs, dirs, payload)?;
        } else {
            let size = meta.len();
            *payload += size;
            recs.push(FileRec {
                rel: rel.clone(),
                src: full.clone(),
            });
            out.push(IsoImageFile {
                source: full,
                destination: rel,
            });
        }
    }
    visited.remove(&canon);
    Ok(())
}

/// ASCII-uppercased name: how the engine will (approximately) render it in
/// the ISO9660 namespace, used for collision detection across merged inputs.
fn fold(name: &str) -> String {
    name.chars().map(|c| c.to_ascii_uppercase()).collect()
}

fn collect(inputs: &[PathBuf], flat: bool) -> Result<Collected, String> {
    let mut out = Collected {
        files: Vec::new(),
        recs: Vec::new(),
        dirs: 0,
        payload_bytes: 0,
    };
    let mut root_names: HashMap<String, PathBuf> = HashMap::new();
    let mut visited = std::collections::HashSet::new();

    for input in inputs {
        let meta =
            std::fs::metadata(input).map_err(|e| format!("cannot access {:?}: {}", input, e))?;
        if !meta.is_dir() {
            let name = input
                .file_name()
                .ok_or_else(|| format!("bad input path {:?}", input))?
                .to_string_lossy()
                .into_owned();
            let key = fold(&name);
            if let Some(prev) = root_names.get(&key) {
                return Err(format!(
                    "duplicate root name '{}' from {:?} and {:?}",
                    name, prev, input
                ));
            }
            root_names.insert(key, input.clone());
            let size = meta.len();
            out.payload_bytes += size;
            out.recs.push(FileRec {
                rel: name.clone(),
                src: input.clone(),
            });
            out.files.push(IsoImageFile {
                source: input.clone(),
                destination: name,
            });
            continue;
        }

        // directory argument
        if flat {
            let mut before = out.recs.len();
            walk_dir(
                input,
                "",
                0,
                &mut visited,
                &mut out.files,
                &mut out.recs,
                &mut out.dirs,
                &mut out.payload_bytes,
            )?;
            // the walk appended top-level entries; check root-name collisions
            for r in &out.recs[before..] {
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
            before = out.recs.len(); // silence unused-mut style
            let _ = before;
        } else {
            let name = input
                .file_name()
                .ok_or_else(|| format!("bad input path {:?}", input))?
                .to_string_lossy()
                .into_owned();
            let key = fold(&name);
            if let Some(prev) = root_names.get(&key) {
                return Err(format!(
                    "duplicate root name '{}' from {:?} and {:?}",
                    name, prev, input
                ));
            }
            root_names.insert(key, input.clone());
            out.dirs += 1;
            walk_dir(
                input,
                &name,
                0,
                &mut visited,
                &mut out.files,
                &mut out.recs,
                &mut out.dirs,
                &mut out.payload_bytes,
            )?;
        }
    }

    if out.files.is_empty() {
        return Err("no files found in the inputs".to_string());
    }
    Ok(out)
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
        // auto-detect EFI/BOOT/BOOTX64.EFI anywhere in the tree
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

    let collected = collect(inputs, opts.flat)?;

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

    let boot_info = match &boot_rel {
        Some(rel) => {
            // find the local source of the boot file (engine needs the path)
            let src = collected
                .recs
                .iter()
                .find(|r| &r.rel == rel)
                .map(|r| r.src.clone())
                .expect("boot rel resolved from a payload file");
            BootInfo {
                bios_boot: None,
                uefi_boot: Some(UefiBootInfo {
                    boot_image: src.clone(),
                    kernel_image: src,
                    destination_in_iso: rel.clone(),
                    additional_efi_boot_files: Vec::new(),
                    grub_cfg_content: None,
                }),
            }
        }
        None => BootInfo {
            bios_boot: None,
            uefi_boot: None,
        },
    };

    let image = IsoImage {
        volume_id: Some(label.clone()),
        files: collected.files,
        boot_info,
        layout_profile: IsoLayoutProfile::default(),
    };

    isobemak_build(output, &image, false).map_err(|e| format!("isobemak build failed: {}", e))?;
    iso_fix::conform(output, boot_rel.is_some())?;

    let sectors = std::fs::metadata(output)
        .map(|m| m.len() / 2048)
        .unwrap_or(0);

    Ok(BuildSummary {
        label,
        dirs: collected.dirs,
        files: collected.recs.len() as u64,
        payload_bytes: collected.payload_bytes,
        sectors,
        boot_path: boot_rel,
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
