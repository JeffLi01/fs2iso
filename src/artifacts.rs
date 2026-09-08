//! Post-write pass over the finished image: mark the engine's internal
//! artifacts (`esp.img`, `boot.catalog`) as ISO9660 HIDDEN in both the base
//! and Joliet directory trees.
//!
//! Why: the disc's data tree is the face the user sees when mounting it on
//! Windows — only the user's own files should appear there. The artifacts
//! stay on the disc (El Torito addresses `esp.img` by sector, and the
//! boot catalog must exist), but ordinary listings (Explorer) skip hidden
//! records. ECMA-119 9.1.6: directory-record flags bit0 = hidden file.
//!
//! The writer (hadris-cd) exposes no per-file hidden flag, so we patch the
//! finished image directly: locate the primary volume descriptor (base
//! namespace) and the real Joliet supplementary descriptor (escape sequence
//! "%/E", ECMA-119 8.4), parse each root directory extent and set bit0 of
//! the flags byte of matching records. Hidden files remain fully readable.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// ISO9660 logical block / sector size.
const SECTOR_SIZE: u64 = 2048;

/// Volume descriptor types (ECMA-119 8.3 / ISO 9660 6.2).
const DESCRIPTOR_TYPE_PRIMARY: u8 = 1;
const DESCRIPTOR_TYPE_SUPPLEMENTARY: u8 = 2;
const DESCRIPTOR_TYPE_TERMINATOR: u8 = 0xFF;

/// Identifier every volume descriptor starts with.
const DESCRIPTOR_SIGNATURE: &[u8; 5] = b"CD001";

/// Real Joliet supplementary descriptors carry this escape sequence at
/// bytes 88..90 of the descriptor (ECMA-119 8.4.1). hadris-cd also emits a
/// placeholder supplementary descriptor (empty root) that must be skipped.
const JOLIET_ESCAPE_SEQUENCE: [u8; 3] = *b"%/E";
const JOLIET_ESCAPE_OFFSET: usize = 88;

/// Root directory record location within a volume descriptor: ECMA-119 8.4
/// places it at descriptor offset 156; fields follow the directory record
/// layout of ECMA-119 9.1.
const ROOT_RECORD_OFFSET: usize = 156;
const ROOT_EXTENT_OFFSET: usize = 2; // data extent (LE32) inside the record
const ROOT_DATA_LENGTH_OFFSET: usize = 10; // data length (LE32) inside the record

/// Fields of a directory record (ECMA-119 9.1). Records are variable-length;
/// the record length (byte 0) starts each one.
const RECORD_LENGTH_OFFSET: usize = 0;
const RECORD_FLAGS_OFFSET: usize = 25;
const RECORD_IDENTIFIER_LENGTH_OFFSET: usize = 32;
const RECORD_IDENTIFIER_OFFSET: usize = 33;

/// Directory-record flag bit0 = hidden (ECMA-119 9.1.6).
const FLAG_HIDDEN: u8 = 0b0000_0001;

/// Names of the engine artifacts to hide, in their original (Joliet) form.
const ARTIFACT_NAMES: [&str; 2] = ["esp.img", "boot.catalog"];

fn read_sector(file: &mut File, logical_block: u32) -> Result<[u8; SECTOR_SIZE as usize], String> {
    file.seek(SeekFrom::Start(logical_block as u64 * SECTOR_SIZE))
        .map_err(|e| e.to_string())?;
    let mut sector = [0u8; SECTOR_SIZE as usize];
    file.read_exact(&mut sector).map_err(|e| e.to_string())?;
    Ok(sector)
}

fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

/// Decode a directory-record identifier to a name string.
/// Joliet identifiers are UCS-2 (big-endian UTF-16); base identifiers are
/// ASCII with an optional ";<version>" suffix (ECMA-119 9.1.6).
fn record_name(record: &[u8], joliet: bool) -> String {
    let identifier_length = record[RECORD_IDENTIFIER_LENGTH_OFFSET] as usize;
    let identifier = &record[RECORD_IDENTIFIER_OFFSET..RECORD_IDENTIFIER_OFFSET + identifier_length];
    if joliet {
        let units: Vec<u16> = identifier
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        let without_version = match identifier.iter().position(|&byte| byte == b';') {
            Some(semicolon_index) => &identifier[..semicolon_index],
            None => identifier,
        };
        String::from_utf8_lossy(without_version)
            .into_owned()
            .to_uppercase()
    }
}

/// Set the HIDDEN flag on every root-directory record whose name matches an
/// artifact, within the namespace described by `descriptor_lba` (a PVD or a
/// real Joliet SVD). Returns the number of records patched.
fn hide_artifacts_in_tree(
    file: &mut File,
    descriptor_lba: u32,
    joliet: bool,
) -> Result<usize, String> {
    let descriptor = read_sector(file, descriptor_lba)?;
    let root_record =
        &descriptor[ROOT_RECORD_OFFSET..ROOT_RECORD_OFFSET + RECORD_IDENTIFIER_OFFSET];
    let root_extent = read_u32_le(root_record, ROOT_EXTENT_OFFSET);
    let root_data_length = read_u32_le(root_record, ROOT_DATA_LENGTH_OFFSET) as usize;

    // The root directory data lives in one contiguous extent.
    let mut directory_data = vec![0u8; root_data_length];
    file.seek(SeekFrom::Start(root_extent as u64 * SECTOR_SIZE))
        .map_err(|e| e.to_string())?;
    file.read_exact(&mut directory_data)
        .map_err(|e| e.to_string())?;

    let mut patched = 0usize;
    let mut record_offset = 0usize;
    while record_offset + RECORD_IDENTIFIER_OFFSET + 1 <= root_data_length {
        let record_length = directory_data[record_offset + RECORD_LENGTH_OFFSET] as usize;
        if record_length == 0 || record_offset + record_length > root_data_length {
            break;
        }
        let record = &directory_data[record_offset..record_offset + record_length];
        let name = record_name(record, joliet);
        if name.is_empty() || name == "." || name == ".." {
            record_offset += record_length;
            continue;
        }
        let is_artifact = ARTIFACT_NAMES.iter().any(|artifact| {
            if joliet {
                name == *artifact
            } else {
                name == artifact.to_uppercase()
            }
        });
        if is_artifact {
            let flags_offset = root_extent as u64 * SECTOR_SIZE + record_offset as u64
                + RECORD_FLAGS_OFFSET as u64;
            file.seek(SeekFrom::Start(flags_offset))
                .map_err(|e| e.to_string())?;
            let mut flags_byte = [0u8; 1];
            file.read_exact(&mut flags_byte).map_err(|e| e.to_string())?;
            flags_byte[0] |= FLAG_HIDDEN;
            file.seek(SeekFrom::Start(flags_offset))
                .map_err(|e| e.to_string())?;
            file.write_all(&flags_byte).map_err(|e| e.to_string())?;
            patched += 1;
        }
        record_offset += record_length;
    }
    Ok(patched)
}

/// Mark the engine artifacts hidden in both namespaces of the finished
/// image at `path` (the file is reopened read-write).
pub(crate) fn hide_engine_artifacts(path: &Path) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("reopen {:?} for artifact hiding: {}", path, e))?;

    // Locate the primary descriptor (base tree) and the real Joliet
    // supplementary descriptor (the one with the %/E escape sequence).
    let mut primary_lba: Option<u32> = None;
    let mut joliet_lba: Option<u32> = None;
    for logical_block in 16u32.. {
        let sector = read_sector(&mut file, logical_block)?;
        if sector[0] == DESCRIPTOR_TYPE_TERMINATOR {
            break;
        }
        if &sector[1..6] != DESCRIPTOR_SIGNATURE {
            continue;
        }
        match sector[0] {
            DESCRIPTOR_TYPE_PRIMARY => primary_lba = Some(logical_block),
            DESCRIPTOR_TYPE_SUPPLEMENTARY
                if sector[JOLIET_ESCAPE_OFFSET..JOLIET_ESCAPE_OFFSET + 3]
                    == JOLIET_ESCAPE_SEQUENCE =>
            {
                if joliet_lba.is_none() {
                    joliet_lba = Some(logical_block);
                }
            }
            _ => {}
        }
    }

    let mut hidden_count = 0usize;
    for (descriptor_lba, joliet) in [(primary_lba, false), (joliet_lba, true)] {
        if let Some(lba) = descriptor_lba {
            hidden_count += hide_artifacts_in_tree(&mut file, lba, joliet)?;
        }
    }
    if hidden_count == 0 {
        return Err(format!(
            "internal: expected to hide engine artifacts in {:?}, none found",
            path
        ));
    }
    Ok(())
}
