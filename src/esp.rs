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

        for (relative_path, content) in entries {
            let mut path_components: Vec<&str> = relative_path.split('/').collect();
            let leaf_name = path_components.pop().unwrap_or(relative_path.as_str());
            let mut directory = filesystem.root_dir();
            for component in path_components {
                directory = match directory.open_dir(component) {
                    Ok(existing) => existing,
                    Err(_) => directory.create_dir(component).map_err(|e| e.to_string())?,
                };
            }
            let mut file = directory
                .create_file(leaf_name)
                .map_err(|e| e.to_string())?;
            file.write_all(content).map_err(|e| e.to_string())?;
            file.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(image_data)
}
