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
use std::fmt;

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

#[derive(Debug)]
pub enum BuildError {
    NoInputs,
    Collection(String),
    OutputPath(String),
    OutputConflict(PathBuf),
    ReservedArtifact(String),
    PayloadRead { path: PathBuf, message: String },
    ImageBuild(String),
    OutputCreate { path: PathBuf, message: String },
    OutputFinalize { path: PathBuf, message: String },
    ArtifactPatch(String),
}

impl fmt::Display for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoInputs => write!(formatter, "no input paths given"),
            Self::Collection(message) | Self::OutputPath(message) => write!(formatter, "{}", message),
            Self::OutputConflict(path) => {
                write!(formatter, "output image {:?} would overwrite a payload file", path)
            }
            Self::ReservedArtifact(name) => write!(
                formatter,
                "payload contains a root-level file named '{}', which is reserved for an engine artifact ({}); rename it",
                name,
                RESERVED_ARTIFACT_NAMES.join(" / ")
            ),
            Self::PayloadRead { path, message } => {
                write!(formatter, "cannot read {:?}: {}", path, message)
            }
            Self::ImageBuild(message) => write!(formatter, "image build failed: {}", message),
            Self::OutputCreate { path, message } => {
                write!(formatter, "cannot create {:?}: {}", path, message)
            }
            Self::OutputFinalize { path, message } => {
                write!(formatter, "cannot finalize {:?}: {}", path, message)
            }
            Self::ArtifactPatch(message) => write!(formatter, "{}", message),
        }
    }
}

impl std::error::Error for BuildError {}

const RESERVED_ARTIFACT_NAMES: [&str; 2] = ["esp.img", "boot.catalog"];

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

fn resolve_label(output: &Path, requested: Option<&str>) -> String {
    requested
        .map(sanitize_label)
        .or_else(|| output.file_stem().map(|stem| sanitize_label(&stem.to_string_lossy())))
        .unwrap_or_else(|| "FS2ISO".to_string())
}

fn validate_artifact_names(collected_payload: &CollectedPayload) -> Result<(), BuildError> {
    let clash = collected_payload.records.iter().find(|record| {
        RESERVED_ARTIFACT_NAMES
            .iter()
            .any(|reserved| record.relative_path.eq_ignore_ascii_case(reserved))
    });
    if let Some(clash) = clash {
        return Err(BuildError::ReservedArtifact(clash.relative_path.clone()));
    }
    Ok(())
}

fn sectors_for(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() / 2048)
        .unwrap_or(0)
}

fn temporary_output_path(output: &Path) -> PathBuf {
    let file_name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("fs2iso-output");
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let process_id = std::process::id();
    for attempt in 0u32.. {
        let suffix = if attempt == 0 {
            format!("{}.tmp-{}", file_name, process_id)
        } else {
            format!("{}.tmp-{}-{}", file_name, process_id, attempt)
        };
        let candidate = parent.join(suffix);
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!()
}

/// Build the image at `output` from `inputs`, honouring `options`.
///
/// Returns a summary for the CLI to print. Errors are user-presentable
/// strings: every failure path carries the offending path/name.
pub fn build_iso(
    output: &Path,
    inputs: &[PathBuf],
    options: &Options,
) -> Result<BuildSummary, BuildError> {
    if inputs.is_empty() {
        return Err(BuildError::NoInputs);
    }

    let mut collected_payload =
        collect_payload(inputs, options.flat).map_err(BuildError::Collection)?;

    // Refuse to clobber a payload file with the output image.
    let output_canonical = canonical_maybe_missing(output).map_err(BuildError::OutputPath)?;
    for record in &collected_payload.records {
        if canonical_existing(&record.source_path).map_err(BuildError::Collection)?
            == output_canonical
        {
            return Err(BuildError::OutputConflict(output.to_path_buf()));
        }
    }

    let label = resolve_label(output, options.label.as_deref());

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
        validate_artifact_names(&collected_payload)?;
        let esp_container = build_esp_container(&mut collected_payload)?;
        collected_payload
            .tree
            .root
            .add_file(hadris_cd::FileEntry::from_buffer("esp.img", esp_container));
        collected_payload.tree.root.sort();
        configure_eltorito(&mut image_options);
    }

    let temporary_output = temporary_output_path(output);
    let build_result = (|| {
        let output_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .create_new(true)
        .open(&temporary_output)
        .map_err(|error| BuildError::OutputCreate {
            path: temporary_output.clone(),
            message: error.to_string(),
        })?;
        hadris_cd::OpticalImageWriter::create(output_file, collected_payload.tree, image_options)
        .map_err(|error| BuildError::ImageBuild(error.to_string()))?;

        // Engine artifacts (esp.img, boot.catalog) must not clutter the data
        // tree the user sees: mark them hidden in both namespaces.
        if bootable {
            artifacts::hide_engine_artifacts(&temporary_output).map_err(BuildError::ArtifactPatch)?;
        }

        let sectors = sectors_for(&temporary_output);
        if output.exists() {
            std::fs::remove_file(output).map_err(|error| BuildError::OutputFinalize {
                path: output.to_path_buf(),
                message: error.to_string(),
            })?;
        }
        std::fs::rename(&temporary_output, output).map_err(|error| BuildError::OutputFinalize {
            path: output.to_path_buf(),
            message: error.to_string(),
        })?;

        Ok(BuildSummary {
            label,
            dirs: collected_payload.directory_count,
            files: collected_payload.records.len() as u64,
            payload_bytes: collected_payload.payload_bytes,
            sectors,
            bootable,
        })
    })();
    if build_result.is_err() {
        let _ = std::fs::remove_file(&temporary_output);
    }
    build_result
}

/// Read every payload file into memory and build the FAT container (esp.img)
/// that mirrors the image's file layout.
fn build_esp_container(collected_payload: &mut CollectedPayload) -> Result<Vec<u8>, BuildError> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(collected_payload.records.len());
    for record in &collected_payload.records {
        let bytes = std::fs::read(&record.source_path)
            .map_err(|error| BuildError::PayloadRead {
                path: record.source_path.clone(),
                message: error.to_string(),
            })?;
        entries.push((record.relative_path.clone(), bytes));
    }
    esp::build_esp(&entries).map_err(BuildError::ImageBuild)
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
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{resolve_label, sanitize_label, sectors_for, validate_artifact_names};
    use crate::collect::{CollectedPayload, FileRecord};
    use hadris_cd::{Directory, FileTree};

    static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

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

    #[test]
    fn resolve_label_prefers_requested_label_then_output_stem() {
        assert_eq!(
            resolve_label(Path::new("derived-name.iso"), Some("My Label")),
            "MY_LABEL"
        );
        assert_eq!(resolve_label(Path::new("derived-name.iso"), None), "DERIVED_NAME");
        assert_eq!(resolve_label(Path::new("."), None), "FS2ISO");
    }

    #[test]
    fn validate_artifact_names_only_rejects_root_files() {
        let mut tree = FileTree { root: Directory::root() };
        let payload = CollectedPayload {
            tree,
            records: vec![FileRecord {
                relative_path: "nested/esp.img".to_string(),
                source_path: Path::new("nested/esp.img").to_path_buf(),
            }],
            directory_count: 0,
            payload_bytes: 0,
        };
        validate_artifact_names(&payload).unwrap();

        tree = FileTree { root: Directory::root() };
        let payload = CollectedPayload {
            tree,
            records: vec![FileRecord {
                relative_path: "ESP.IMG".to_string(),
                source_path: Path::new("ESP.IMG").to_path_buf(),
            }],
            directory_count: 0,
            payload_bytes: 0,
        };
        let error = validate_artifact_names(&payload).unwrap_err();
        assert!(matches!(error, super::BuildError::ReservedArtifact(ref name) if name == "ESP.IMG"));
        assert!(error.to_string().contains("reserved"));
    }

    #[test]
    fn sectors_for_reports_complete_iso_sectors() {
        let path = std::env::temp_dir().join(format!(
            "fs2iso-sectors-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, vec![0u8; 4096 + 17]).unwrap();
        assert_eq!(sectors_for(&path), 2);
        let _ = std::fs::remove_file(path);
    }
}
