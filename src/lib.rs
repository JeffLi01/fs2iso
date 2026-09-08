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
//! Module layout: `collect` owns the payload-tree walk (keep-parent/--flat,
//! duplicate guards); `esp` builds the FAT container; `artifacts` hides the
//! engine's internal files in the finished data tree; this file is the
//! orchestrator plus the CLI-facing types.

mod artifacts;
mod collect;
mod esp;

use std::path::{Path, PathBuf};

use collect::{canonical_existing, canonical_maybe_missing, collect_payload, CollectedPayload};
use hadris_cd::OpticalImageOptions;

#[derive(Default)]
pub struct Options {
    /// Volume label (cleaned). None -> derived from the output file name.
    pub label: Option<String>,
    /// mkisofs-style: directory arguments merge their *contents* into the root.
    pub flat: bool,
    /// Do not pack the FAT container / add an El Torito entry — produce a
    /// plain ISO9660+Joliet data disc.
    pub no_eltorito: bool,
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

/// Volume labels must be printable: ASCII alnum kept (uppercased), everything
/// else becomes '_'.
pub(crate) fn sanitize_label(raw: &str) -> String {
    let mut out = String::new();
    for byte in raw.bytes() {
        match byte {
            b'0'..=b'9' | b'A'..=b'Z' => out.push(byte as char),
            b'a'..=b'z' => out.push((byte - 32) as char),
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

/// Build the image at `output` from `inputs`, honouring `options`.
///
/// Returns a summary for the CLI to print. Errors are user-presentable
/// strings: every failure path carries the offending path/name.
pub fn build_iso(
    output: &Path,
    inputs: &[PathBuf],
    options: &Options,
) -> Result<BuildSummary, String> {
    if inputs.is_empty() {
        return Err("no input paths given".to_string());
    }

    let mut collected_payload = collect_payload(inputs, options.flat)?;

    // Refuse to clobber a payload file with the output image.
    let output_canonical = canonical_maybe_missing(output)?;
    for record in &collected_payload.records {
        if canonical_existing(&record.source_path)? == output_canonical {
            return Err(format!(
                "output image {:?} would overwrite a payload file",
                output
            ));
        }
    }

    let label = match &options.label {
        Some(label) => sanitize_label(label),
        None => match output.file_stem() {
            Some(stem) => sanitize_label(&stem.to_string_lossy()),
            None => "FS2ISO".to_string(),
        },
    };

    // UDF is disabled on purpose: hadris-cd's UDF layer is not
    // spec-complete (missing ECMA-167 file-set terminator), and Windows
    // udfs.sys refuses such volumes outright.
    let mut image_options = OpticalImageOptions {
        volume_id: label.clone(),
        udf: hadris_cd::UdfOptions {
            enabled: false,
            ..Default::default()
        },
        ..OpticalImageOptions::default()
    };

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
    let bootable = !options.no_eltorito;
    if bootable {
        // Root-level payload names that collide with the engine's own
        // artifacts are rejected up front: hadris-cd would silently rename
        // the user's file (observed: boot.catalog -> boot_1.catalog in both
        // trees), breaking the "files keep their names" promise and
        // desyncing the data tree from the esp.img FAT copy.
        const RESERVED_ARTIFACT_NAMES: [&str; 2] = ["esp.img", "boot.catalog"];
        let clash = collected_payload.records.iter().find(|record| {
            RESERVED_ARTIFACT_NAMES
                .iter()
                .any(|reserved| record.relative_path.eq_ignore_ascii_case(reserved))
        });
        if let Some(clash) = clash {
            return Err(format!(
                "payload contains a root-level file named '{}', which is reserved for an \
                 engine artifact ({}); rename it",
                clash.relative_path,
                RESERVED_ARTIFACT_NAMES.join(" / ")
            ));
        }
        let esp_container = build_esp_container(&mut collected_payload)?;
        collected_payload
            .tree
            .root
            .add_file(hadris_cd::FileEntry::from_buffer("esp.img", esp_container));
        collected_payload.tree.root.sort();
        configure_eltorito(&mut image_options);
    }

    let output_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(output)
        .map_err(|e| format!("cannot create {:?}: {}", output, e))?;
    hadris_cd::OpticalImageWriter::create(output_file, collected_payload.tree, image_options)
        .map_err(|e| format!("image build failed: {}", e))?;

    // Engine artifacts (esp.img, boot.catalog) must not clutter the data
    // tree the user sees: mark them hidden in both namespaces.
    if bootable {
        artifacts::hide_engine_artifacts(output)?;
    }

    let sectors = std::fs::metadata(output)
        .map(|metadata| metadata.len() / 2048)
        .unwrap_or(0);

    Ok(BuildSummary {
        label,
        dirs: collected_payload.directory_count,
        files: collected_payload.records.len() as u64,
        payload_bytes: collected_payload.payload_bytes,
        sectors,
        bootable,
    })
}

/// Read every payload file into memory and build the FAT container (esp.img)
/// that mirrors the image's file layout.
fn build_esp_container(collected_payload: &mut CollectedPayload) -> Result<Vec<u8>, String> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(collected_payload.records.len());
    for record in &collected_payload.records {
        let bytes = std::fs::read(&record.source_path)
            .map_err(|e| format!("cannot read {:?}: {}", record.source_path, e))?;
        entries.push((record.relative_path.clone(), bytes));
    }
    esp::build_esp(&entries)
}

/// Point the disc's El Torito boot record at the esp.img FAT container.
///
/// A single boot entry only: a second El Torito entry (an extra platform
/// section) pointing at the same esp.img makes the firmware map the FAT
/// image twice (two identical FS/CDROMs appear in the EFI shell).
fn configure_eltorito(image_options: &mut OpticalImageOptions) {
    use hadris_iso::boot::options::{BootEntryOptions, BootOptions};
    use hadris_iso::boot::EmulationType;
    let entry = BootEntryOptions {
        load_size: None,
        boot_image_path: "esp.img".to_string(),
        boot_info_table: false,
        grub2_boot_info: false,
        emulation: EmulationType::NoEmulation,
    };
    image_options.boot = Some(BootOptions {
        write_boot_catalog: true,
        default: entry,
        entries: Vec::new(),
    });
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
        assert!(long.bytes().all(|byte| byte == b'X'));
    }
}
