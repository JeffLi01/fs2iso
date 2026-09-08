//! FAT container (`esp.img`) generation.
//!
//! fs2iso's disc exposes the payload to the EFI shell through this FAT
//! image: the El Torito boot record points at it, so firmware loads the FAT
//! volume and EDK2-class shells mount it (fs0/fsX). This is the only way
//! EDK2 firmware — which has no ISO9660 data driver — can see the payload.
//!
//! Every payload file/directory goes in here verbatim (original names,
//! original structure). Nothing is injected, renamed or special-cased: the
//! container does not care whether some file is an EFI boot program, and no
//! default boot file is added.
//!
//! The image is sized dynamically to the payload (FAT16 up to 2 GiB, FAT32
//! above, minimum 1 MiB so firmware/partitioning stays happy).

use std::collections::HashMap;
use std::io::{Cursor, Write};

const SECTOR_SIZE: u64 = 512;
/// Smallest image we format.
const MINIMUM_IMAGE_BYTES: u64 = 1024 * 1024;
/// Fixed slack for FAT tables / root dir / formatting overhead.
const FORMATTING_SLACK_BYTES: u64 = 512 * 1024;
/// FAT32 kicks in above this size.
const FAT32_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Hard ceiling for the FAT container.
const MAXIMUM_IMAGE_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Characters FAT forbids in names (ECMA-107 / Microsoft FAT spec), beyond
/// the '/' path separator and control bytes handled separately.
const FAT_FORBIDDEN_CHARS: &str = "\"*/:<>?\\|";

/// Validate every entry's path up front and reject, with a clear message:
///   - components FAT cannot represent (forbidden characters, control
///     bytes, leading/trailing spaces, trailing dots — legal on NTFS and
///     other filesystems, illegal or order-dependent inside FAT);
///   - two entries folding to the same uppercase name inside one FAT
///     directory. FAT is case-insensitive: naive writers silently clobber
///     or drop one of the pair (fs2iso's collector already rejects these,
///     but `build_esp` is a public API and must not lose data on its own).
fn validate_entries(entries: &[(String, Vec<u8>)]) -> Result<(), String> {
    // FOLDED image dir path -> (folded leaf name -> original leaf name).
    // Directory keys are folded too: FAT directories are case-insensitive,
    // so `A/x` and `a/X` would land in the same real directory.
    let mut per_directory: HashMap<String, HashMap<String, &str>> = HashMap::new();
    for (relative_path, _) in entries {
        let mut components: Vec<&str> = relative_path.split('/').collect();
        let leaf = match components.pop() {
            Some("") | None => {
                return Err(format!(
                    "bad path {:?} in FAT container: empty or trailing-slash component",
                    relative_path
                ))
            }
            Some(leaf) => leaf,
        };
        let image_dir = components.join("/");
        if components.contains(&"") {
            return Err(format!(
                "bad path {:?} in FAT container: empty path component",
                relative_path
            ));
        }
        let names = per_directory
            .entry(image_dir.to_ascii_uppercase())
            .or_default();
        for component in components.iter().chain(std::iter::once(&leaf)) {
            if let Some(bad) = component.chars().find(|c| {
                FAT_FORBIDDEN_CHARS.contains(*c) || (*c as u32) < 0x20 || *c as u32 == 0x7F
            }) {
                return Err(format!(
                    "cannot pack {:?} into the FAT container: character {:?} in {:?} is not \
                     allowed in FAT names",
                    relative_path, bad, component
                ));
            }
            if component.starts_with(' ') || component.ends_with(' ') || component.ends_with('.') {
                return Err(format!(
                    "cannot pack {:?} into the FAT container: {:?} starts/ends with a space or \
                     ends with a dot, which FAT cannot represent",
                    relative_path, component
                ));
            }
        }
        let folded = leaf.to_ascii_uppercase();
        if let Some(previous) = names.get(&folded) {
            let where_at = if image_dir.is_empty() {
                "the FAT container root".to_string()
            } else {
                format!("FAT directory '{}'", image_dir)
            };
            return Err(format!(
                "duplicate name {:?} in {}: collides with {:?} (FAT names are \
                 case-insensitive; rename one of the payload files)",
                leaf, where_at, previous
            ));
        }
        names.insert(folded, leaf);
    }
    Ok(())
}

/// Format a FAT volume and write every `(relative_path, content)` entry into
/// it, creating intermediate directories as needed. Returns the raw FAT
/// image bytes, ready to be stored as `esp.img` on the disc.
pub fn build_esp(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let payload_total: u64 = entries
        .iter()
        .map(|(_, content)| content.len() as u64)
        .sum();
    if payload_total == 0 {
        return Err("nothing to pack into the FAT container".to_string());
    }
    validate_entries(entries)?;
    let mut image_bytes =
        ((payload_total + FORMATTING_SLACK_BYTES).div_ceil(SECTOR_SIZE)) * SECTOR_SIZE;
    if image_bytes < MINIMUM_IMAGE_BYTES {
        image_bytes = MINIMUM_IMAGE_BYTES;
    }
    if image_bytes > MAXIMUM_IMAGE_BYTES {
        return Err(format!(
            "payload too large for the FAT container ({} bytes > 4 GiB)",
            payload_total
        ));
    }

    let mut image_data = vec![0u8; image_bytes as usize];
    {
        let mut cursor = Cursor::new(&mut image_data[..]);
        let fat_type = if image_bytes <= FAT32_THRESHOLD_BYTES {
            fatfs::FatType::Fat16
        } else {
            fatfs::FatType::Fat32
        };
        let format_options = fatfs::FormatVolumeOptions::new()
            .fat_type(fat_type)
            .volume_label(*b"FS2ISO_EFI!");
        fatfs::format_volume(&mut cursor, format_options)
            .map_err(|e| format!("FAT format failed: {}", e))?;
        let filesystem = fatfs::FileSystem::new(cursor, fatfs::FsOptions::new())
            .map_err(|e| format!("FAT open failed: {}", e))?;

        // Sizing sanity: the image was dimensioned as payload + fixed slack,
        // but the FAT tables themselves are geometry-dependent (up to several
        // MiB near the 4 GiB cap). Refuse up front instead of failing
        // mid-write with an opaque error.
        let stats = filesystem
            .stats()
            .map_err(|e| format!("FAT stats failed: {}", e))?;
        let free_bytes = stats.free_clusters() as u64 * stats.cluster_size() as u64;
        if free_bytes < payload_total {
            return Err(format!(
                "internal sizing error: FAT volume free space ({free_bytes} B) < payload \
                 ({payload_total} B); increase FORMATTING_SLACK_BYTES in src/esp.rs"
            ));
        }

        for (relative_path, content) in entries {
            let mut path_components: Vec<&str> = relative_path.split('/').collect();
            let leaf_name = path_components.pop().unwrap_or(relative_path.as_str());
            let mut directory = filesystem.root_dir();
            for component in path_components {
                directory = match directory.open_dir(component) {
                    Ok(existing) => existing,
                    Err(_) => directory.create_dir(component).map_err(|e| {
                        format!(
                            "cannot create directory {:?} for {:?}: {}",
                            component, relative_path, e
                        )
                    })?,
                };
            }
            let mut file = directory.create_file(leaf_name).map_err(|e| {
                format!("cannot create {:?} in FAT container: {}", relative_path, e)
            })?;
            file.write_all(content)
                .map_err(|e| format!("cannot write {:?} in FAT container: {}", relative_path, e))?;
            file.flush()
                .map_err(|e| format!("cannot flush {:?} in FAT container: {}", relative_path, e))?;
        }
    }
    Ok(image_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> (String, Vec<u8>) {
        (name.to_string(), b"content".to_vec())
    }

    #[test]
    fn rejects_case_fold_duplicates_in_same_dir() {
        let err = build_esp(&[entry("A.TXT"), entry("a.txt")]).unwrap_err();
        assert!(err.contains("case-insensitive"), "{}", err);
        // Directories fold too: `A/x` and `a/X` share one real FAT dir.
        let err = build_esp(&[entry("A/x.TXT"), entry("a/X.txt")]).unwrap_err();
        assert!(err.contains("duplicate name"), "{}", err);
    }

    #[test]
    fn allows_case_distinct_names_in_distinct_dirs() {
        build_esp(&[entry("dir/x.TXT"), entry("other/x.txt")]).unwrap();
    }

    #[test]
    fn rejects_forbidden_characters() {
        let err = build_esp(&[entry("bad:name.txt")]).unwrap_err();
        assert!(err.contains("bad:name.txt"), "{}", err);
        assert!(build_esp(&[entry("star*name")]).is_err());
    }

    #[test]
    fn rejects_unrepresentable_edges() {
        // trailing dot / space and empty components are FAT-hostile
        assert!(build_esp(&[entry("tail.")]).is_err());
        assert!(build_esp(&[entry("tail ")]).is_err());
        assert!(build_esp(&[entry("")]).is_err());
        assert!(build_esp(&[entry("/abs")]).is_err());
        assert!(build_esp(&[entry("a//b")]).is_err());
    }
}
