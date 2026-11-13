use rayon::prelude::*;
use serde::Serialize;
use std::{
    collections::HashSet,
    fs::{self, Metadata},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

#[derive(Debug, Serialize)]
pub struct Node {
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub apparent: u64,
    pub allocated: u64,
    pub items: u64,
    pub errors: u64,
    pub error: Option<String>,
    pub children: Vec<Node>,
    #[serde(skip)]
    identity: Option<(u64, u64)>,
    #[serde(skip)]
    pub deletion_identity: Option<(u64, u64)>,
    #[serde(skip)]
    own_apparent: u64,
    #[serde(skip)]
    own_allocated: u64,
    #[serde(skip)]
    own_errors: u64,
}

fn serialize_path<S: serde::Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&path.to_string_lossy())
}

impl Node {
    pub fn size(&self, apparent: bool) -> u64 {
        if apparent {
            self.apparent
        } else {
            self.allocated
        }
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .unwrap_or(self.path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

#[derive(Default)]
pub struct Progress {
    pub entries: AtomicU64,
    pub errors: AtomicU64,
    pub allocated: AtomicU64,
    seen_links: Mutex<HashSet<(u64, u64)>>,
}

impl Progress {
    fn record_allocated(&self, metadata: &Metadata, count_links: bool) {
        #[cfg(unix)]
        if !count_links && !metadata.is_dir() {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1
                && !self
                    .seen_links
                    .lock()
                    .unwrap()
                    .insert((metadata.dev(), metadata.ino()))
            {
                return;
            }
        }
        #[cfg(not(unix))]
        let _ = count_links;
        self.allocated
            .fetch_add(allocated(metadata), Ordering::Relaxed);
    }
}

pub struct Options {
    pub threads: usize,
    pub one_file_system: bool,
    pub count_links: bool,
}

pub fn scan(path: &Path, options: &Options, progress: &Progress) -> anyhow::Result<Node> {
    let metadata = fs::symlink_metadata(path)?;
    let root_device = device(&metadata);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.threads)
        .build()?;
    let mut root = pool.install(|| visit(path.to_path_buf(), options, root_device, progress));
    // Account after scanning, in sorted path order, so hard-link ownership is deterministic.
    aggregate(&mut root, &mut HashSet::new(), options.count_links);
    Ok(root)
}

fn visit(path: PathBuf, options: &Options, root_device: u64, progress: &Progress) -> Node {
    progress.entries.fetch_add(1, Ordering::Relaxed);
    let mut node = Node {
        path,
        is_dir: false,
        is_symlink: false,
        apparent: 0,
        allocated: 0,
        items: 0,
        errors: 0,
        error: None,
        children: Vec::new(),
        identity: None,
        deletion_identity: None,
        own_apparent: 0,
        own_allocated: 0,
        own_errors: 0,
    };
    let metadata = match fs::symlink_metadata(&node.path) {
        Ok(metadata) => metadata,
        Err(error) => {
            record_error(&mut node, error, progress);
            return node;
        }
    };
    node.is_dir = metadata.is_dir();
    node.is_symlink = metadata.file_type().is_symlink();
    node.deletion_identity = file_identity(&metadata);
    if options.one_file_system && device(&metadata) != root_device {
        return node;
    }
    node.own_apparent = metadata.len();
    node.own_allocated = allocated(&metadata);
    progress.record_allocated(&metadata, options.count_links);
    // Keep all identities: a new hard link can appear during a later subtree rescan.
    node.identity = file_identity(&metadata);
    if node.is_dir {
        let entries = match fs::read_dir(&node.path) {
            Ok(entries) => entries,
            Err(error) => {
                record_error(&mut node, error, progress);
                return node;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => record_error(&mut node, error, progress),
            }
        }
        // Close directory handles before descending; traversal does not hold one per ancestor.
        paths.sort();
        node.children = paths
            .into_par_iter()
            .map(|path| visit(path, options, root_device, progress))
            .collect();
    }
    node
}

fn record_error(node: &mut Node, error: std::io::Error, progress: &Progress) {
    node.errors += 1;
    node.own_errors += 1;
    node.error = Some(error.to_string());
    progress.errors.fetch_add(1, Ordering::Relaxed);
}

fn aggregate(node: &mut Node, seen: &mut HashSet<(u64, u64)>, count_links: bool) {
    node.apparent = node.own_apparent;
    node.allocated = node.own_allocated;
    node.errors = node.own_errors;
    node.items = 0;
    if !node.is_dir && !count_links {
        if let Some(identity) = node.identity {
            if !seen.insert(identity) {
                node.apparent = 0;
                node.allocated = 0;
            }
        }
    }
    for child in &mut node.children {
        aggregate(child, seen, count_links);
        node.apparent = node.apparent.saturating_add(child.apparent);
        node.allocated = node.allocated.saturating_add(child.allocated);
        node.items = node.items.saturating_add(child.items).saturating_add(1);
        node.errors = node.errors.saturating_add(child.errors);
    }
}

pub fn rescan(
    root: &mut Node,
    path: &Path,
    options: &Options,
    progress: &Progress,
) -> anyhow::Result<()> {
    path.strip_prefix(&root.path)?;
    let root_metadata = fs::symlink_metadata(&root.path)?;
    if path != root.path {
        anyhow::ensure!(
            file_identity(&root_metadata) == root.deletion_identity,
            "scan root changed"
        );
        let expected_parent =
            fs::canonicalize(&root.path)?.join(path.strip_prefix(&root.path)?.parent().unwrap());
        anyhow::ensure!(
            fs::canonicalize(path.parent().unwrap())? == expected_parent,
            "parent path changed"
        );
    }
    // Validate first so a vanished directory leaves the existing snapshot intact.
    fs::symlink_metadata(path)?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(options.threads)
        .build()?;
    let replacement = pool.install(|| {
        visit(
            path.to_path_buf(),
            options,
            device(&root_metadata),
            progress,
        )
    });
    if path == root.path {
        *root = replacement;
    } else {
        replace_entry(root, path, Some(replacement));
    }
    aggregate(root, &mut HashSet::new(), options.count_links);
    Ok(())
}

pub fn update_after_delete(
    root: &mut Node,
    path: &Path,
    options: &Options,
    deleted: bool,
) -> anyhow::Result<()> {
    let relative = path.strip_prefix(&root.path)?;
    anyhow::ensure!(
        !relative.as_os_str().is_empty(),
        "cannot update deletion of scan root"
    );
    let root_metadata = fs::symlink_metadata(&root.path)?;
    anyhow::ensure!(
        file_identity(&root_metadata) == root.deletion_identity,
        "scan root changed"
    );
    let replacement = if deleted {
        None
    } else {
        let expected_parent = fs::canonicalize(&root.path)?.join(relative.parent().unwrap());
        anyhow::ensure!(
            fs::canonicalize(path.parent().unwrap())? == expected_parent,
            "parent path changed"
        );
        match fs::symlink_metadata(path) {
            Ok(_) => {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(options.threads)
                    .build()?;
                Some(pool.install(|| {
                    visit(
                        path.to_path_buf(),
                        options,
                        device(&root_metadata),
                        &Progress::default(),
                    )
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        }
    };
    replace_entry(root, path, replacement);
    // CPU-only accounting preserves deterministic hard-link ownership without filesystem I/O.
    aggregate(root, &mut HashSet::new(), options.count_links);
    Ok(())
}

fn replace_entry(node: &mut Node, path: &Path, replacement: Option<Node>) {
    let Some(index) = node
        .children
        .iter()
        .position(|child| path.starts_with(&child.path))
    else {
        return;
    };
    if node.children[index].path == path {
        if let Some(replacement) = replacement {
            node.children[index] = replacement;
        } else {
            node.children.remove(index);
        }
    } else {
        replace_entry(&mut node.children[index], path, replacement);
    }
    if let Ok(metadata) = fs::symlink_metadata(&node.path) {
        if file_identity(&metadata) == node.deletion_identity {
            node.own_apparent = metadata.len();
            node.own_allocated = allocated(&metadata);
        }
    }
}

#[cfg(unix)]
fn device(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

#[cfg(not(unix))]
fn device(_: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
pub fn file_identity(metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
pub fn file_identity(_: &Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
pub(crate) fn allocated(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
pub(crate) fn allocated(metadata: &Metadata) -> u64 {
    metadata.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(path: &Path, threads: usize, count_links: bool) -> Node {
        scan(
            path,
            &Options {
                threads,
                one_file_system: false,
                count_links,
            },
            &Progress::default(),
        )
        .unwrap()
    }

    #[test]
    fn nested_totals_are_identical_across_thread_counts() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        fs::write(temp.path().join("a"), b"abc").unwrap();
        fs::write(temp.path().join("nested/b"), b"12345").unwrap();
        let single = run(temp.path(), 1, false);
        let parallel = run(temp.path(), 4, false);
        let directories = fs::metadata(temp.path()).unwrap().len()
            + fs::metadata(temp.path().join("nested")).unwrap().len();
        assert_eq!(parallel.apparent, directories + 8);
        assert_eq!(parallel.items, 3);
        assert_eq!(
            serde_json::to_string(&single).unwrap(),
            serde_json::to_string(&parallel).unwrap()
        );
    }

    #[test]
    fn missing_root_is_an_error() {
        let temp = tempfile::tempdir().unwrap();
        assert!(scan(
            &temp.path().join("absent"),
            &Options {
                threads: 2,
                one_file_system: false,
                count_links: false
            },
            &Progress::default()
        )
        .is_err());
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_are_counted_once_unless_requested() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), b"12345").unwrap();
        fs::hard_link(temp.path().join("a"), temp.path().join("b")).unwrap();
        let dedup = run(temp.path(), 4, false);
        let all = run(temp.path(), 4, true);
        assert_eq!(all.apparent - dedup.apparent, 5);
        assert_eq!(dedup.children[0].apparent, 5);
        assert_eq!(dedup.children[1].apparent, 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_including_cycles_and_broken_links() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        symlink(temp.path(), temp.path().join("cycle")).unwrap();
        symlink("absent", temp.path().join("broken")).unwrap();
        let root = run(temp.path(), 4, false);
        assert_eq!(root.items, 2);
        assert_eq!(root.errors, 0);
        assert!(root
            .children
            .iter()
            .all(|child| child.is_symlink && child.children.is_empty()));
    }

    #[cfg(unix)]
    #[test]
    fn sparse_files_report_apparent_and_allocated_sizes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("sparse");
        fs::File::create(&path)
            .unwrap()
            .set_len(16 * 1024 * 1024)
            .unwrap();
        let node = run(&path, 2, false);
        assert_eq!(node.apparent, 16 * 1024 * 1024);
        assert!(node.allocated < node.apparent);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_can_be_exported() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let temp = tempfile::tempdir().unwrap();
        let mut node = run(temp.path(), 2, false);
        // APFS rejects invalid UTF-8 filenames, so test the export boundary directly.
        node.path = temp.path().join(OsString::from_vec(vec![0xff]));
        assert!(serde_json::to_string(&node).is_ok());
    }

    #[test]
    fn disappearing_entry_is_recorded_without_aborting() {
        let temp = tempfile::tempdir().unwrap();
        let progress = Progress::default();
        let node = visit(
            temp.path().join("gone"),
            &Options {
                threads: 2,
                one_file_system: false,
                count_links: false,
            },
            0,
            &progress,
        );
        assert_eq!(node.errors, 1);
        assert!(node.error.is_some());
        assert_eq!(progress.errors.load(Ordering::Relaxed), 1);
    }

    fn options(count_links: bool) -> Options {
        Options {
            threads: 2,
            one_file_system: false,
            count_links,
        }
    }

    #[test]
    fn deleting_updates_totals_without_rescanning_unrelated_files() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("delete"), b"abc").unwrap();
        fs::create_dir(temp.path().join("unrelated")).unwrap();
        fs::write(temp.path().join("unrelated/file"), b"original").unwrap();
        let mut root = run(temp.path(), 2, false);
        // Changes elsewhere must not be read by a deletion refresh.
        fs::write(
            temp.path().join("unrelated/file"),
            b"different and much longer",
        )
        .unwrap();
        let target = temp.path().join("delete");
        fs::remove_file(&target).unwrap();
        update_after_delete(&mut root, &target, &options(false), true).unwrap();
        assert_eq!(root.items, 2);
        assert_eq!(root.children[0].children[0].apparent, 8);
        let expected = fs::metadata(temp.path()).unwrap().len()
            + fs::metadata(temp.path().join("unrelated")).unwrap().len()
            + 8;
        assert_eq!(root.apparent, expected);
        // Recomputing repeatedly must not add totals a second time.
        aggregate(&mut root, &mut HashSet::new(), false);
        assert_eq!(root.apparent, expected);
    }

    #[test]
    fn deleted_subtree_reassigns_hard_link_ownership() {
        for count_links in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("a")).unwrap();
            fs::write(temp.path().join("a/file"), b"12345").unwrap();
            fs::hard_link(temp.path().join("a/file"), temp.path().join("b")).unwrap();
            let mut root = run(temp.path(), 2, count_links);
            let target = temp.path().join("a");
            fs::remove_dir_all(&target).unwrap();
            update_after_delete(&mut root, &target, &options(count_links), true).unwrap();
            assert_eq!(root.children[0].apparent, 5);
            assert_eq!(root.items, 1);
            assert_eq!(
                serde_json::to_string(&root).unwrap(),
                serde_json::to_string(&run(temp.path(), 2, count_links)).unwrap()
            );
        }
    }

    #[test]
    fn partial_deletion_refreshes_only_affected_subtree() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("gone"), b"abc").unwrap();
        fs::write(dir.join("keep"), b"12345").unwrap();
        fs::write(temp.path().join("unrelated"), b"original").unwrap();
        let mut root = run(temp.path(), 2, false);
        fs::remove_file(dir.join("gone")).unwrap();
        fs::write(temp.path().join("unrelated"), b"a changed length").unwrap();
        update_after_delete(&mut root, &dir, &options(false), false).unwrap();
        assert_eq!(root.items, 3);
        assert_eq!(root.children[0].items, 1);
        assert_eq!(root.children[0].children[0].apparent, 5);
        assert_eq!(root.children[1].apparent, 8);
    }

    #[test]
    fn missing_target_on_failure_is_removed_from_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, b"abc").unwrap();
        let mut root = run(temp.path(), 2, false);
        fs::remove_file(&path).unwrap();
        update_after_delete(&mut root, &path, &options(false), false).unwrap();
        assert_eq!(root.items, 0);
        assert!(root.children.is_empty());
    }

    #[test]
    fn rescan_updates_current_subtree_without_reading_siblings() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("old"), b"old").unwrap();
        fs::write(temp.path().join("sibling"), b"abc").unwrap();
        let mut root = run(temp.path(), 2, false);
        fs::remove_file(dir.join("old")).unwrap();
        fs::write(dir.join("new"), b"12345").unwrap();
        fs::write(temp.path().join("sibling"), b"changed length").unwrap();
        let progress = Progress::default();
        rescan(&mut root, &dir, &options(false), &progress).unwrap();
        assert_eq!(progress.entries.load(Ordering::Relaxed), 2);
        assert_eq!(root.children[0].children[0].name(), "new");
        assert_eq!(root.children[0].children[0].apparent, 5);
        assert_eq!(root.children[1].apparent, 3);
        assert_eq!(root.items, 3);
        rescan(
            &mut root,
            temp.path(),
            &options(false),
            &Progress::default(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string(&root).unwrap(),
            serde_json::to_string(&run(temp.path(), 2, false)).unwrap()
        );
    }

    #[test]
    fn rescan_failure_preserves_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("dir");
        fs::create_dir(&dir).unwrap();
        let mut root = run(temp.path(), 2, false);
        let before = serde_json::to_string(&root).unwrap();
        fs::remove_dir(&dir).unwrap();
        assert!(rescan(&mut root, &dir, &options(false), &Progress::default()).is_err());
        assert_eq!(serde_json::to_string(&root).unwrap(), before);
    }

    #[test]
    fn scan_progress_allocated_matches_totals_with_hard_links() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), vec![b'x'; 8192]).unwrap();
        fs::hard_link(temp.path().join("a"), temp.path().join("b")).unwrap();
        for count_links in [false, true] {
            let progress = Progress::default();
            let root = scan(temp.path(), &options(count_links), &progress).unwrap();
            assert_eq!(progress.allocated.load(Ordering::Relaxed), root.allocated);
        }
    }

    #[test]
    fn rescan_reaccounts_hard_links_across_subtrees() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("a");
        fs::create_dir(&dir).unwrap();
        fs::write(temp.path().join("b"), b"12345").unwrap();
        let mut root = run(temp.path(), 2, false);
        fs::hard_link(temp.path().join("b"), dir.join("link")).unwrap();
        rescan(&mut root, &dir, &options(false), &Progress::default()).unwrap();
        assert_eq!(root.children[0].children[0].apparent, 5);
        assert_eq!(root.children[1].apparent, 0);
    }
}
