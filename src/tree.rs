//! Collect the payload from the filesystem into a node arena.
//!
//! Semantics:
//!  * a FILE argument becomes a file at the ISO root;
//!  * a DIRECTORY argument becomes a directory at the ISO root (default) or, with
//!    `flat`, its *contents* are merged into the root (mkisofs-style);
//!  * collisions at the same level are hard errors.
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub struct Node {
    /// Original file name (lossy UTF-8 of the on-disk name).
    pub name: String,
    pub is_dir: bool,
    /// Source path for regular files.
    pub src: Option<PathBuf>,
    pub size: u64,
    /// mtime as Unix seconds (UTC). 0 when unknown.
    pub mtime: u64,
    pub parent: usize,
    pub children: Vec<usize>,
    /// Base-tree identifier (uppercase mangled, files carry ";1").
    pub base_id: Vec<u8>,
    /// Joliet identifier (UTF-16BE of the original name, no version).
    pub joliet_id: Vec<u8>,
    // --- assigned during layout ---
    pub base_lba: u32, // directory-record block (dirs) or file data (files), base namespace
    pub jol_lba: u32,  // directory-record block in the joliet namespace (dirs only)
    pub sectors: u32,  // base-namespace dir block sectors (dirs only)
    pub jol_sectors: u32,
}

pub struct Tree {
    pub arena: Vec<Node>,
    pub root: usize,
}

pub fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn mtime_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn collect_into(
    arena: &mut Vec<Node>,
    parent: usize,
    src: &Path,
    name: String,
    depth: usize,
    ancestor_dirs: &mut HashSet<PathBuf>,
) -> Result<usize, String> {
    if depth > 64 {
        return Err(format!("directory nesting too deep at {:?}", src));
    }
    let meta = fs::metadata(src).map_err(|e| format!("cannot stat {:?}: {}", src, e))?;

    if meta.is_dir() {
        let canon = fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
        if !ancestor_dirs.insert(canon.clone()) {
            return Err(format!("directory loop detected at {:?}", src));
        }
        let idx = arena.len();
        arena.push(Node {
            name: name.clone(),
            is_dir: true,
            src: None,
            size: 0,
            mtime: mtime_secs(&meta),
            parent,
            children: Vec::new(),
            base_id: Vec::new(),
            joliet_id: Vec::new(),
            base_lba: 0,
            jol_lba: 0,
            sectors: 0,
            jol_sectors: 0,
        });

        let mut entries: Vec<(String, PathBuf)> = Vec::new();
        let rd = fs::read_dir(src).map_err(|e| format!("cannot read dir {:?}: {}", src, e))?;
        for ent in rd {
            let ent = ent.map_err(|e| format!("read_dir entry in {:?}: {}", src, e))?;
            let p = ent.path();
            entries.push((file_name_of(&p), p));
        }
        // deterministic order regardless of OS enumeration order
        entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

        let mut kids = Vec::with_capacity(entries.len());
        for (n, p) in entries {
            let c = collect_into(arena, idx, &p, n, depth + 1, ancestor_dirs)?;
            kids.push(c);
        }
        arena[idx].children = kids;
        ancestor_dirs.remove(&canon);
        Ok(idx)
    } else if meta.is_file() {
        let size = meta.len();
        if size > u32::MAX as u64 {
            return Err(format!("file too large for ISO9660 (max 4 GiB): {:?}", src));
        }
        let idx = arena.len();
        arena.push(Node {
            name,
            is_dir: false,
            src: Some(src.to_path_buf()),
            size,
            mtime: mtime_secs(&meta),
            parent,
            children: Vec::new(),
            base_id: Vec::new(),
            joliet_id: Vec::new(),
            base_lba: 0,
            jol_lba: 0,
            sectors: 0,
            jol_sectors: 0,
        });
        Ok(idx)
    } else {
        Err(format!(
            "unsupported file type (not a regular file or directory): {:?}",
            src
        ))
    }
}

/// Decide which nodes an input contributes at the ISO root:
/// file arg -> itself; dir arg (non-flat) -> itself; dir arg (flat) -> its children.
fn place(arena: &[Node], idx: usize, flat: bool) -> Result<Vec<usize>, String> {
    let node = &arena[idx];
    if node.is_dir && flat {
        Ok(node.children.clone())
    } else {
        Ok(vec![idx])
    }
}

/// Collect payload: files become root files; dirs become root dirs (or are flattened
/// with `flat`). Returns the arena with a synthetic root at index 0.
pub fn collect_payload(inputs: &[PathBuf], flat: bool) -> Result<Tree, String> {
    let mut arena: Vec<Node> = Vec::new();
    arena.push(Node {
        name: String::from("ROOT"),
        is_dir: true,
        src: None,
        size: 0,
        mtime: 0,
        parent: 0,
        children: Vec::new(),
        base_id: Vec::new(),
        joliet_id: Vec::new(),
        base_lba: 0,
        jol_lba: 0,
        sectors: 0,
        jol_sectors: 0,
    });

    let mut root_children: Vec<usize> = Vec::new();
    let mut seen: HashSet<(bool, String)> = HashSet::new(); // (is_dir-ish, name) — keyed by name only

    for src in inputs {
        let name = file_name_of(src);
        if !src.exists() {
            return Err(format!("input does not exist: {:?}", src));
        }
        let idx = collect_into(&mut arena, 0, src, name.clone(), 0, &mut HashSet::new())?;
        let new_kids = place(&arena, idx, flat)?;
        if flat {
            // root children get parent = root
            for &k in &new_kids {
                arena[k].parent = 0;
            }
        }
        for &k in &new_kids {
            let n = arena[k].name.clone();
            if !seen.insert((false, n.clone())) {
                return Err(format!(
                    "duplicate root entry name {:?} (two inputs map to the same ISO path)",
                    n
                ));
            }
            arena[k].parent = 0;
            root_children.push(k);
        }
        if !flat {
            // the dir node itself is a root child; it was already pushed via new_kids when not a dir? handle:
            // place() returned [idx] for files and non-flat dirs, so nothing more to do.
        }
    }

    if root_children.is_empty() {
        return Err("no payload given".to_string());
    }

    // keep root children sorted by name for deterministic output
    root_children.sort_by(|&a, &b| arena[a].name.as_bytes().cmp(arena[b].name.as_bytes()));
    arena[0].children = root_children;
    Ok(Tree { arena, root: 0 })
}

/// Assign base + joliet identifiers to every entry inside every directory.
pub fn assign_ids(t: &mut Tree) {
    // DFS over dirs; for each dir, assign ids to its children in deterministic
    // (original-name) order.
    let mut stack = vec![t.root];
    while let Some(d) = stack.pop() {
        // push children dirs (DFS order doesn't matter for id assignment)
        let kids = t.arena[d].children.clone();
        let mut base_used: HashSet<Vec<u8>> = HashSet::new();
        let mut jol_used: HashSet<Vec<u8>> = HashSet::new();
        // assign in original-name order so suffixes are stable
        let mut ordered = kids.clone();
        ordered.sort_by(|&a, &b| t.arena[a].name.as_bytes().cmp(t.arena[b].name.as_bytes()));
        for k in ordered {
            let (is_dir, name) = (t.arena[k].is_dir, t.arena[k].name.clone());
            let (bi, ji) = if is_dir {
                (
                    crate::names::base_dir_id(&name, &mut base_used),
                    crate::names::joliet_id(&name, &mut jol_used),
                )
            } else {
                (
                    crate::names::base_file_id(&name, &mut base_used),
                    crate::names::joliet_id(&name, &mut jol_used),
                )
            };
            t.arena[k].base_id = bi;
            t.arena[k].joliet_id = ji;
        }
        let mut dirs: Vec<usize> = kids.into_iter().filter(|&c| t.arena[c].is_dir).collect();
        dirs.sort_unstable_by(|&a, &b| t.arena[a].name.as_bytes().cmp(t.arena[b].name.as_bytes()));
        // DFS: process in reverse so the stack yields original order
        for c in dirs.into_iter().rev() {
            stack.push(c);
        }
    }
}

/// Compute the ISO-relative path (case-insensitive, '/' joined) of a node, for
/// El Torito auto-detection.
pub fn iso_path_of(t: &Tree, mut node: usize) -> String {
    let mut parts: Vec<String> = Vec::new();
    loop {
        parts.push(t.arena[node].name.to_lowercase());
        if node == t.root {
            break;
        }
        node = t.arena[node].parent;
        if node == usize::MAX {
            break;
        }
    }
    parts.reverse();
    parts.join("/")
}

/// DFS over all entries (parents before children), any order of children.
pub fn dfs_all(t: &Tree) -> Vec<usize> {
    let mut out = Vec::new();
    let mut stack = vec![t.root];
    while let Some(n) = stack.pop() {
        out.push(n);
        // push children in reverse so they pop in stored order
        let kids = t.arena[n].children.clone();
        for &c in kids.iter().rev() {
            stack.push(c);
        }
    }
    out
}

pub fn file_err(e: io::Error) -> String {
    e.to_string()
}
