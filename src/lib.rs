//! fs2iso — pure-Rust packer: files/directories -> ISO9660 (+Joliet, optional
//! El Torito EFI boot) image for BMC virtual media / UEFI shell use.
//!
//! The library itself is dependency-free: ISO9660 structures are written from
//! first principles (ECMA-119). Only the CLI binary (src/main.rs) uses clap
//! for command-line argument parsing.

pub mod layout;
pub mod names;
pub mod timeutil;
pub mod tree;

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

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

pub fn build_iso(
    output: &Path,
    inputs: &[PathBuf],
    opts: &Options,
) -> Result<BuildSummary, String> {
    if inputs.is_empty() {
        return Err("no input paths given".to_string());
    }

    let mut tree = tree::collect_payload(inputs, opts.flat)?;

    // refuse to clobber a payload file with the output image
    let out_canon = canonical_maybe_missing(output)?;
    for f in all_source_files(&tree) {
        if canonical(&f)? == out_canon {
            return Err(format!(
                "output image {:?} would overwrite a payload file",
                output
            ));
        }
    }

    tree::assign_ids(&mut tree);

    let label = match &opts.label {
        Some(l) => names::sanitize_label(l),
        None => match output.file_stem() {
            Some(stem) => names::sanitize_label(&stem.to_string_lossy()),
            None => "FS2ISO".to_string(),
        },
    };

    // pick the El Torito boot file
    let boot: Option<usize> = if opts.no_eltorito {
        None
    } else if let Some(bf) = &opts.boot_efi {
        let want = canonical(bf)?;
        let mut found = None;
        for f in all_source_files(&tree) {
            if canonical(&f)? == want {
                found = Some(f);
                break;
            }
        }
        let node = found.ok_or_else(|| {
            format!(
                "--boot-efi {:?} is not part of the payload (add it as an input)",
                bf
            )
        })?;
        let idx = node_of_src(&tree, &node);
        if tree.arena[idx].is_dir {
            return Err("--boot-efi must name a file, not a directory".to_string());
        }
        Some(idx)
    } else {
        // auto-detect EFI/BOOT/BOOTX64.EFI anywhere in the tree
        let mut found = None;
        for (i, n) in tree.arena.iter().enumerate() {
            if !n.is_dir && tree::iso_path_of(&tree, i).ends_with("efi/boot/bootx64.efi") {
                found = Some(i);
                break;
            }
        }
        found
    };

    let use_joliet = !opts.no_joliet;
    let plan = layout::assign_lbas(&mut tree, boot, use_joliet)?;

    let out_file =
        File::create(output).map_err(|e| format!("cannot create {:?}: {}", output, e))?;
    let mut w = BufWriter::new(out_file);
    layout::render_image(&tree, &plan, boot, use_joliet, label.as_bytes(), &mut w)?;
    w.flush().map_err(|e| format!("flush error: {}", e))?;

    let (files, payload_bytes) = {
        let mut f = 0u64;
        let mut b = 0u64;
        for n in &tree.arena {
            if !n.is_dir {
                f += 1;
                b += n.size;
            }
        }
        (f, b)
    };
    let dirs = tree.arena.iter().filter(|n| n.is_dir).count() as u64 - 1; // minus synthetic root

    Ok(BuildSummary {
        label,
        dirs,
        files,
        payload_bytes,
        sectors: plan.total_sectors as u64,
        boot_path: boot.map(|i| tree::iso_path_of(&tree, i)),
        joliet: use_joliet,
    })
}

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

fn all_source_files(tree: &tree::Tree) -> Vec<PathBuf> {
    tree.arena.iter().filter_map(|n| n.src.clone()).collect()
}

fn node_of_src(tree: &tree::Tree, src: &Path) -> usize {
    tree.arena
        .iter()
        .position(|n| n.src.as_ref().map(|s| s == src).unwrap_or(false))
        .expect("source file must exist in tree")
}
