use crate::scan::{file_identity, Node};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::{
    fs,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
};

#[derive(Default)]
pub struct Progress {
    pub checked: AtomicU64,
    pub removed: AtomicU64,
    pub removed_allocated: AtomicU64,
    pub phase: AtomicU8,
    pub cancel: AtomicBool,
}

impl Progress {
    fn check_cancel(&self) -> Result<()> {
        anyhow::ensure!(
            !self.cancel.load(Ordering::Relaxed),
            "deletion cancelled (already removed entries cannot be restored)"
        );
        Ok(())
    }
}

struct Entry {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    is_dir: bool,
    is_symlink: bool,
    children: Vec<Entry>,
}

#[cfg(test)]
pub fn remove(root: &Node, target: &Node) -> Result<()> {
    remove_parallel(root, target, 2, &Progress::default())
}

pub fn remove_parallel(
    root: &Node,
    target: &Node,
    threads: usize,
    progress: &Progress,
) -> Result<()> {
    validate_target(root, target)?;
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()?;
    pool.install(|| {
        let plan = plan(
            &target.path,
            root.deletion_identity.map(|id| id.0),
            progress,
        )?;
        anyhow::ensure!(
            plan.identity == target.deletion_identity
                && plan.is_dir == target.is_dir
                && plan.is_symlink == target.is_symlink,
            "selected entry changed during preflight"
        );
        progress.phase.store(1, Ordering::Relaxed);
        remove_entry(&plan, progress)
    })
}

fn validate_target(root: &Node, target: &Node) -> Result<()> {
    let relative = target
        .path
        .strip_prefix(&root.path)
        .context("entry is outside the scan root")?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("refusing to delete the scan root or an unsafe path");
    }
    let root_metadata = fs::symlink_metadata(&root.path)?;
    if !root_metadata.is_dir() || file_identity(&root_metadata) != root.deletion_identity {
        bail!("scan root changed; restart whatdu before deleting");
    }
    let canonical_root = fs::canonicalize(&root.path)?;
    let expected_parent = canonical_root.join(relative.parent().unwrap());
    let actual_parent = fs::canonicalize(target.path.parent().unwrap())?;
    if actual_parent != expected_parent {
        bail!("parent path changed or traverses a symlink; restart whatdu before deleting");
    }
    let metadata = fs::symlink_metadata(&target.path)?;
    if file_identity(&metadata) != target.deletion_identity
        || metadata.is_dir() != target.is_dir
        || metadata.file_type().is_symlink() != target.is_symlink
    {
        bail!("selected entry changed since scanning; restart whatdu before deleting");
    }
    Ok(())
}

fn plan(path: &Path, device: Option<u64>, progress: &Progress) -> Result<Entry> {
    progress.check_cancel()?;
    let metadata = fs::symlink_metadata(path)?;
    if file_identity(&metadata).map(|id| id.0) != device {
        bail!(
            "refusing deletion across a filesystem boundary: {}",
            path.display()
        );
    }
    progress.checked.fetch_add(1, Ordering::Relaxed);
    let children = if metadata.is_dir() {
        let paths = fs::read_dir(path)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        paths
            .into_par_iter()
            .map(|path| plan(&path, device, progress))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok(Entry {
        path: path.to_path_buf(),
        identity: file_identity(&metadata),
        is_dir: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        children,
    })
}

fn remove_entry(entry: &Entry, progress: &Progress) -> Result<()> {
    progress.check_cancel()?;
    let metadata = fs::symlink_metadata(&entry.path)?;
    anyhow::ensure!(
        file_identity(&metadata) == entry.identity
            && metadata.is_dir() == entry.is_dir
            && metadata.file_type().is_symlink() == entry.is_symlink,
        "entry changed during deletion: {}",
        entry.path.display()
    );
    if entry.is_dir {
        entry
            .children
            .par_iter()
            .try_for_each(|child| remove_entry(child, progress))?;
        progress.check_cancel()?;
        fs::remove_dir(&entry.path)
            .with_context(|| format!("cannot remove directory {}", entry.path.display()))?;
    } else {
        fs::remove_file(&entry.path)
            .with_context(|| format!("cannot remove file {}", entry.path.display()))?;
    }
    // Sum pre-removal entry sizes, not reclaimed filesystem capacity.
    progress
        .removed_allocated
        .fetch_add(crate::scan::allocated(&metadata), Ordering::Relaxed);
    progress.removed.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan, Options, Progress as ScanProgress};

    fn snapshot(path: &std::path::Path) -> Node {
        scan(
            path,
            &Options {
                threads: 2,
                one_file_system: false,
                count_links: false,
            },
            &ScanProgress::default(),
        )
        .unwrap()
    }

    #[test]
    fn deletes_nested_directory_and_refreshes_hard_link_totals() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("a")).unwrap();
        fs::write(temp.path().join("a/file"), b"12345").unwrap();
        fs::hard_link(temp.path().join("a/file"), temp.path().join("b")).unwrap();
        let root = snapshot(temp.path());
        assert_eq!(root.children[1].apparent, 0);
        remove(&root, &root.children[0]).unwrap();
        assert!(!temp.path().join("a").exists());
        let refreshed = snapshot(temp.path());
        assert_eq!(refreshed.items, 1);
        assert_eq!(refreshed.children[0].apparent, 5);
        remove(&refreshed, &refreshed.children[0]).unwrap();
        assert!(!temp.path().join("b").exists());
    }

    #[test]
    fn refuses_root_and_outside_entries() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = snapshot(temp.path());
        assert!(remove(&root, &root).is_err());
        assert!(remove(&root, &snapshot(outside.path())).is_err());
        assert!(temp.path().exists());
        assert!(outside.path().exists());
    }

    #[test]
    fn missing_entry_and_device_mismatch_do_not_delete_anything() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, b"safe").unwrap();
        let root = snapshot(temp.path());
        let actual_device = file_identity(&fs::symlink_metadata(&path).unwrap()).map(|id| id.0);
        let other_device = Some(actual_device.unwrap_or(0).wrapping_add(1));
        assert!(plan(&path, other_device, &Progress::default()).is_err());
        assert!(path.exists());
        fs::rename(&path, temp.path().join("keep")).unwrap();
        assert!(remove(&root, &root.children[0]).is_err());
        assert!(temp.path().join("keep").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unlinks_symlinks_without_touching_targets() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("keep"), b"safe").unwrap();
        fs::create_dir(temp.path().join("dir")).unwrap();
        symlink(outside.path(), temp.path().join("dir/link")).unwrap();
        symlink(outside.path(), temp.path().join("link")).unwrap();
        symlink("missing", temp.path().join("broken")).unwrap();
        let root = snapshot(temp.path());
        for child in &root.children {
            remove(&root, child).unwrap();
        }
        assert!(outside.path().join("keep").exists());
        assert_eq!(snapshot(temp.path()).items, 0);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_replaced_entry_and_symlinked_parent() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("dir")).unwrap();
        fs::write(temp.path().join("dir/file"), b"safe").unwrap();
        let root = snapshot(temp.path());
        let target = &root.children[0].children[0];
        fs::rename(
            temp.path().join("dir/file"),
            temp.path().join("dir/original"),
        )
        .unwrap();
        fs::write(temp.path().join("dir/file"), b"replacement").unwrap();
        assert!(remove(&root, target).is_err());
        assert!(temp.path().join("dir/file").exists());
        fs::rename(temp.path().join("dir"), temp.path().join("moved")).unwrap();
        symlink(temp.path().join("moved"), temp.path().join("dir")).unwrap();
        assert!(remove(&root, target).is_err());
        assert!(temp.path().join("moved/file").exists());
    }

    #[test]
    fn parallel_removal_counts_every_entry_once() {
        for threads in [1, 4] {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("delete");
            fs::create_dir(&dir).unwrap();
            for i in 0..8 {
                let child = dir.join(i.to_string());
                fs::create_dir(&child).unwrap();
                for j in 0..16 {
                    fs::write(child.join(j.to_string()), b"test").unwrap();
                }
            }
            let root = snapshot(temp.path());
            let progress = Progress::default();
            remove_parallel(&root, &root.children[0], threads, &progress).unwrap();
            assert_eq!(progress.checked.load(Ordering::Relaxed), 137);
            assert_eq!(progress.removed.load(Ordering::Relaxed), 137);
            assert!(!dir.exists());
        }
    }

    #[test]
    fn cancellation_before_preflight_removes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, b"test").unwrap();
        let root = snapshot(temp.path());
        let progress = Progress::default();
        progress.cancel.store(true, Ordering::Relaxed);
        assert!(remove_parallel(&root, &root.children[0], 4, &progress).is_err());
        assert_eq!(progress.removed.load(Ordering::Relaxed), 0);
        assert_eq!(progress.removed_allocated.load(Ordering::Relaxed), 0);
        assert!(path.exists());
    }

    #[test]
    fn deleted_bytes_count_only_successful_entries_not_space_freed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file");
        fs::write(&path, vec![b'x'; 8192]).unwrap();
        fs::hard_link(&path, temp.path().join("keep")).unwrap();
        let root = snapshot(temp.path());
        let bytes = crate::scan::allocated(&fs::symlink_metadata(&path).unwrap());
        let progress = Progress::default();
        remove_parallel(&root, &root.children[0], 4, &progress).unwrap();
        assert_eq!(progress.removed_allocated.load(Ordering::Relaxed), bytes);
        assert_eq!(progress.removed.load(Ordering::Relaxed), 1);
        assert!(temp.path().join("keep").exists());
        assert!(remove_parallel(&root, &root.children[0], 4, &progress).is_err());
        assert_eq!(progress.removed_allocated.load(Ordering::Relaxed), bytes);
    }

    #[test]
    fn partial_cancel_and_replaced_plan_entries_are_safe() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), b"test").unwrap();
        fs::write(temp.path().join("b"), b"test").unwrap();
        let progress = Progress::default();
        let device = file_identity(&fs::symlink_metadata(temp.path()).unwrap()).map(|id| id.0);
        let entry = plan(temp.path(), device, &progress).unwrap();
        remove_entry(&entry.children[0], &progress).unwrap();
        progress.cancel.store(true, Ordering::Relaxed);
        assert!(remove_entry(&entry, &progress).is_err());
        assert_eq!(progress.removed.load(Ordering::Relaxed), 1);
        assert!(entry.children[1].path.exists());
        assert!(temp.path().exists());
        #[cfg(unix)]
        {
            progress.cancel.store(false, Ordering::Relaxed);
            let original = &entry.children[1].path;
            fs::rename(original, temp.path().join("saved")).unwrap();
            fs::write(original, b"replacement").unwrap();
            assert!(remove_entry(&entry.children[1], &progress).is_err());
            assert!(original.exists());
            assert_eq!(progress.removed.load(Ordering::Relaxed), 1);
        }
    }
}
