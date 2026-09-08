//! Payload collection: turn the CLI input paths into a hadris-cd `FileTree`
//! plus a flat file manifest used to build the FAT container.
//!
//! Semantics owned here:
//!   - keep-parent (default): each directory input becomes a directory of
//!     the same name at the image root (whole subtree);
//!   - --flat: a directory's *contents* are merged into the image root
//!     (mkisofs style);
//!   - duplicate-root-name and output-clobbering guards.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use hadris_cd::{Directory, FileEntry, FileTree};

/// One payload file: where it lives on disk and where it goes in the image.
pub(crate) struct FileRecord {
    /// Image-relative path, forward slashes ("" prefix => image root).
    pub(crate) relative_path: String,
    pub(crate) source_path: PathBuf,
}

pub(crate) struct CollectedPayload {
    pub(crate) tree: FileTree,
    pub(crate) records: Vec<FileRecord>,
    pub(crate) directory_count: u64,
    pub(crate) payload_bytes: u64,
}

/// ASCII-uppercase folding of a name, for duplicate detection across merged
/// inputs (ISO9660 base namespace is case-insensitive).
fn ascii_upper(name: &str) -> String {
    name.chars().map(|c| c.to_ascii_uppercase()).collect()
}

/// Case-insensitive name registry, keyed by image directory path ("" = the
/// image root). Catches two source entries folding to the same name before
/// they reach the case-insensitive ISO9660/FAT writers — which would
/// silently drop or clobber one file. Two distinct real directories can
/// collide (inputs merged into one image directory, e.g. two --flat inputs
/// each carrying `sub/X.TXT` / `sub/x.txt`), and so can two entries of one
/// real directory on a case-sensitive filesystem (Linux-sourced payloads).
#[derive(Default)]
struct NameRegistry {
    dirs: HashMap<String, HashMap<String, (String, PathBuf)>>,
}

impl NameRegistry {
    /// Reserve `name` inside the image directory `image_dir` ("" = root).
    /// Errors when another entry already folded to the same uppercase name.
    fn reserve(&mut self, image_dir: &str, name: &str, source: &Path) -> Result<(), String> {
        let folded = ascii_upper(name);
        let names = self.dirs.entry(image_dir.to_string()).or_default();
        if let Some((previous_name, previous_source)) = names.get(&folded) {
            return if image_dir.is_empty() {
                Err(format!(
                    "duplicate root name '{}' from {:?} and {:?}",
                    name, previous_source, source
                ))
            } else {
                Err(format!(
                    "duplicate image name '{}' in directory '{}': collides with '{}' from {:?} \
                     (ISO9660/FAT names are case-insensitive)",
                    name, image_dir, previous_name, previous_source
                ))
            };
        }
        names.insert(folded, (name.to_string(), source.to_path_buf()));
        Ok(())
    }
}

fn input_name(input: &Path) -> Result<String, String> {
    Ok(input
        .file_name()
        .ok_or_else(|| format!("bad input path {:?}", input))?
        .to_string_lossy()
        .into_owned())
}

/// Resolve a path that is expected to exist.
pub(crate) fn canonical_existing(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|e| format!("cannot resolve {:?}: {}", path, e))
}

/// Resolve a path that may not exist yet (the output image): canonicalize
/// the parent, then re-append the file name.
pub(crate) fn canonical_maybe_missing(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        return canonical_existing(path);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("bad path {:?}", path))?;
    Ok(canonical_existing(parent)?.join(file_name))
}

/// Recursively add the contents of `source_dir` to `target_directory`.
///
/// `image_prefix` is the image-relative path of `source_dir` itself
/// (empty => its contents land in the image root). `depth` guards against
/// runaway nesting; `visited` detects directory cycles through junctions;
/// `registry` rejects case-folding name collisions inside every image
/// directory (files AND subdirectories).
#[allow(clippy::too_many_arguments)]
fn add_directory_contents(
    target_directory: &mut Directory,
    source_dir: &Path,
    image_prefix: &str,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    registry: &mut NameRegistry,
    records: &mut Vec<FileRecord>,
    directory_count: &mut u64,
    payload_bytes: &mut u64,
) -> Result<(), String> {
    if depth > 64 {
        return Err(format!(
            "directory nesting too deep under {:?} (possible link cycle?)",
            source_dir
        ));
    }
    let canonical_path = std::fs::canonicalize(source_dir)
        .map_err(|e| format!("cannot resolve {:?}: {}", source_dir, e))?;
    if !visited.insert(canonical_path.clone()) {
        return Err(format!(
            "directory cycle detected at {:?} (re-entered a junction/link)",
            source_dir
        ));
    }

    let mut entry_names: Vec<String> = std::fs::read_dir(source_dir)
        .map_err(|e| format!("cannot read directory {:?}: {}", source_dir, e))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|e| format!("read_dir error in {:?}: {}", source_dir, e))
        })
        .collect::<Result<_, _>>()?;
    entry_names.sort();

    for entry_name in entry_names {
        let full_path = source_dir.join(&entry_name);
        let relative_path = if image_prefix.is_empty() {
            entry_name.clone()
        } else {
            format!("{}/{}", image_prefix, entry_name)
        };
        // This entry lands in the image directory `image_prefix` (the image
        // path of `source_dir` itself); reserve its name there.
        registry.reserve(image_prefix, &entry_name, &full_path)?;
        let metadata = std::fs::metadata(&full_path)
            .map_err(|e| format!("cannot stat {:?}: {}", full_path, e))?;
        if metadata.is_dir() {
            *directory_count += 1;
            let mut subdirectory = Directory::new(entry_name);
            add_directory_contents(
                &mut subdirectory,
                &full_path,
                &relative_path,
                depth + 1,
                visited,
                registry,
                records,
                directory_count,
                payload_bytes,
            )?;
            target_directory.add_subdir(subdirectory);
        } else {
            *payload_bytes += metadata.len();
            records.push(FileRecord {
                relative_path: relative_path.clone(),
                source_path: full_path.clone(),
            });
            target_directory.add_file(FileEntry::from_path(entry_name, full_path));
        }
    }
    visited.remove(&canonical_path);
    Ok(())
}

/// Collect `inputs` into a tree + manifest. `flat` selects the merge-into-
/// root behaviour for directory inputs.
pub(crate) fn collect_payload(inputs: &[PathBuf], flat: bool) -> Result<CollectedPayload, String> {
    let mut root = Directory::root();
    let mut records = Vec::new();
    let mut directory_count = 0u64;
    let mut payload_bytes = 0u64;
    let mut registry = NameRegistry::default();
    let mut visited = HashSet::new();

    for input in inputs {
        let metadata =
            std::fs::metadata(input).map_err(|e| format!("cannot access {:?}: {}", input, e))?;
        if !metadata.is_dir() {
            let name = input_name(input)?;
            registry.reserve("", &name, input)?;
            payload_bytes += metadata.len();
            records.push(FileRecord {
                relative_path: name.clone(),
                source_path: input.clone(),
            });
            root.add_file(FileEntry::from_path(name, input.clone()));
            continue;
        }

        // A directory input normally keeps its name at the root
        // (keep-parent). Unnameable paths (".", "..", filesystem roots — no
        // file_name) merge their contents into the root instead, matching
        // mkisofs semantics for `fs2iso out.iso .`.
        let keep_parent = !flat
            && match input_name(input) {
                Ok(name) => name != "." && name != "..",
                Err(_) => false,
            };
        if !keep_parent {
            add_directory_contents(
                &mut root,
                input,
                "",
                0,
                &mut visited,
                &mut registry,
                &mut records,
                &mut directory_count,
                &mut payload_bytes,
            )?;
        } else {
            let name = input_name(input)?;
            registry.reserve("", &name, input)?;
            directory_count += 1;
            let mut subdirectory = Directory::new(name.clone());
            add_directory_contents(
                &mut subdirectory,
                input,
                &name,
                0,
                &mut visited,
                &mut registry,
                &mut records,
                &mut directory_count,
                &mut payload_bytes,
            )?;
            root.add_subdir(subdirectory);
        }
    }

    if records.is_empty() {
        return Err("no files found in the inputs".to_string());
    }
    root.sort();
    Ok(CollectedPayload {
        tree: FileTree { root },
        records,
        directory_count,
        payload_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_rejects_root_fold_collision() {
        let mut registry = NameRegistry::default();
        registry
            .reserve("", "readme.TXT", Path::new("a/readme.TXT"))
            .unwrap();
        let err = registry
            .reserve("", "readme.txt", Path::new("b/readme.txt"))
            .unwrap_err();
        assert!(err.contains("duplicate root name"), "{}", err);
    }

    #[test]
    fn registry_rejects_nested_fold_collision() {
        let mut registry = NameRegistry::default();
        registry
            .reserve("sub", "X.TXT", Path::new("a/sub/X.TXT"))
            .unwrap();
        let err = registry
            .reserve("sub", "x.txt", Path::new("b/sub/x.txt"))
            .unwrap_err();
        assert!(err.contains("duplicate image name"), "{}", err);
        assert!(err.contains("directory 'sub'"), "{}", err);
    }

    #[test]
    fn registry_rejects_dir_name_collisions_too() {
        let mut registry = NameRegistry::default();
        registry.reserve("", "Tools", Path::new("a/Tools")).unwrap();
        let err = registry
            .reserve("", "tools", Path::new("b/tools"))
            .unwrap_err();
        assert!(err.contains("duplicate root name"), "{}", err);
    }

    #[test]
    fn registry_allows_same_name_in_different_dirs() {
        let mut registry = NameRegistry::default();
        registry
            .reserve("a", "x.txt", Path::new("a/x.txt"))
            .unwrap();
        registry
            .reserve("b", "X.TXT", Path::new("b/X.TXT"))
            .unwrap();
    }
}
