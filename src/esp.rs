//! FAT container (ESP-style) generation.
//!
//! fs2iso's disc is a bootable UEFI shell medium: the payload is carried
//! BOTH in the ISO9660(+Joliet) data tree (for Windows / AMI-class readers)
//! and inside a FAT image that the El Torito entry loads at boot. EDK2
//! firmware always supports FAT, so once the disc boots, the EFI shell sees
//! the FAT volume (fs0) with every payload file in it — this is what makes
//! the payload visible on EDK2 firmware, which has no ISO9660 data driver.
//!
//! The FAT image is sized dynamically to the payload (FAT16 up to 2 GiB,
//! FAT32 above) and mirrors the ISO root layout 1:1 plus a guaranteed
//! `EFI/BOOT/BOOTX64.EFI` boot entry.

use std::io::{Cursor, Write};

const SECTOR: u64 = 512;
/// Smallest image we format.
const MIN_BYTES: u64 = 1 * 1024 * 1024;
/// Fixed slack for FAT tables / root dir / formatting overhead.
const SLACK: u64 = 512 * 1024;

/// `entries`: (ISO-relative path, file bytes) — the ESP mirrors the ISO root
/// layout. The caller guarantees an `EFI/BOOT/BOOTX64.EFI` entry (boot file)
/// which wins over any mirrored file of the same name.
pub fn build_esp(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let total: u64 = entries.iter().map(|(_, b)| b.len() as u64).sum();
    if total == 0 {
        return Err("nothing to pack into the FAT container".to_string());
    }
    let mut image_bytes = ((total + SLACK).div_ceil(SECTOR)) * SECTOR;
    if image_bytes < MIN_BYTES {
        image_bytes = MIN_BYTES;
    }
    if image_bytes > 4 * 1024 * 1024 * 1024 {
        return Err(format!(
            "payload too large for the FAT container ({} bytes > 4 GiB)",
            total
        ));
    }

    let mut data = vec![0u8; image_bytes as usize];
    {
        let mut cur = Cursor::new(&mut data[..]);
        let fat_type = if image_bytes <= 2 * 1024 * 1024 * 1024 {
            fatfs::FatType::Fat16
        } else {
            fatfs::FatType::Fat32
        };
        let opts = fatfs::FormatVolumeOptions::new()
            .fat_type(fat_type)
            .volume_label(*b"FS2ISO_EFI!");
        fatfs::format_volume(&mut cur, opts).map_err(|e| format!("FAT format failed: {}", e))?;
        let fs = fatfs::FileSystem::new(cur, fatfs::FsOptions::new())
            .map_err(|e| format!("FAT open failed: {}", e))?;

        for (rel, bytes) in entries {
            let mut parts: Vec<&str> = rel.split('/').collect();
            let leaf = parts.pop().unwrap_or(rel.as_str());
            let mut dir = fs.root_dir();
            for c in parts {
                dir = match dir.open_dir(c) {
                    Ok(d) => d,
                    Err(_) => dir.create_dir(c).map_err(|e| e.to_string())?,
                };
            }
            let mut f = dir.create_file(leaf).map_err(|e| e.to_string())?;
            f.write_all(bytes).map_err(|e| e.to_string())?;
            f.flush().map_err(|e| e.to_string())?;
        }
    }
    Ok(data)
}
