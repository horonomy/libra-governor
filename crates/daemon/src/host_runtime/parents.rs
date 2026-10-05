//! Passive parent observations and explicit, descriptor-relative preparation.
//! Preparing ancestors never initializes the registry or acquires its reservation.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{
    fstat, mkdirat, open, openat, readlinkat_raw, statat, AtFlags, FileType, Mode, OFlags, Stat,
};
use rustix::io::Errno;

use super::RegistryFailure;

const MAX_COMPONENTS: usize = 256;
const MAX_LINKS: usize = 40;

fn failure(reason: &'static str) -> RegistryFailure {
    RegistryFailure::new("registry", reason)
}

fn directory_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC
}

fn open_root() -> Result<Arc<File>, RegistryFailure> {
    open("/", directory_flags(), Mode::empty())
        .map(File::from)
        .map(Arc::new)
        .map_err(|_| failure("state_parent_unavailable"))
}

// Creating a child legitimately changes directory times/link counts. Retain
// identity and safety fields, rather than mistaking that for parent replacement.
fn directory_stamp(stat: &Stat) -> String {
    format!(
        "{}:{}:{}:{}:{}",
        stat.st_dev, stat.st_ino, stat.st_mode, stat.st_uid, stat.st_gid
    )
}

fn link_stamp(stat: &Stat) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}:{}",
        directory_stamp(stat),
        stat.st_nlink,
        stat.st_size,
        stat.st_mtime,
        stat.st_mtime_nsec,
        stat.st_ctime,
        stat.st_ctime_nsec
    )
}

fn safe_anchor(stat: &Stat) -> bool {
    FileType::from_raw_mode(stat.st_mode) == FileType::Directory
        && stat.st_uid == rustix::process::getuid().as_raw()
        && stat.st_mode & 0o022 == 0
}

fn read_link(parent: &File, name: &std::ffi::OsStr) -> Result<PathBuf, RegistryFailure> {
    let mut bytes = [0_u8; 4096];
    let count = readlinkat_raw(parent, name, &mut bytes[..])
        .map_err(|_| failure("state_parent_unavailable"))?;
    if count == bytes.len() {
        return Err(failure("state_parent_limit"));
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..count])))
}

#[derive(Clone, Debug)]
enum EdgeKind {
    Directory { opened: Arc<File>, stamp: String },
    Link { target: PathBuf, stamp: String },
}

#[derive(Clone, Debug)]
struct Edge {
    parent: Arc<File>,
    name: OsString,
    kind: EdgeKind,
}

impl Edge {
    fn check(&self) -> Result<(), RegistryFailure> {
        let current = statat(self.parent.as_ref(), &self.name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| failure("state_parent_changed"))?;
        let same = match &self.kind {
            EdgeKind::Directory { opened, stamp } => {
                let held = fstat(opened.as_ref()).map_err(|_| failure("state_parent_changed"))?;
                FileType::from_raw_mode(current.st_mode) == FileType::Directory
                    && directory_stamp(&current) == *stamp
                    && directory_stamp(&held) == *stamp
            }
            EdgeKind::Link { target, stamp } => {
                FileType::from_raw_mode(current.st_mode) == FileType::Symlink
                    && link_stamp(&current) == *stamp
                    && read_link(self.parent.as_ref(), &self.name)? == *target
            }
        };
        if same {
            Ok(())
        } else {
            Err(failure("state_parent_changed"))
        }
    }
}

fn directory_edge(
    parent: Arc<File>,
    name: OsString,
    before: &Stat,
    owned: bool,
) -> Result<(Arc<File>, Edge), RegistryFailure> {
    if FileType::from_raw_mode(before.st_mode) != FileType::Directory {
        return Err(failure("unsafe_state_parent"));
    }
    let opened = Arc::new(File::from(
        openat(parent.as_ref(), &name, directory_flags(), Mode::empty())
            .map_err(|_| failure("state_parent_unavailable"))?,
    ));
    let held = fstat(opened.as_ref()).map_err(|_| failure("state_parent_unavailable"))?;
    if directory_stamp(before) != directory_stamp(&held) {
        return Err(failure("state_parent_changed"));
    }
    if owned && !safe_anchor(&held) {
        return Err(failure("unsafe_state_parent"));
    }
    let edge = Edge {
        parent,
        name,
        kind: EdgeKind::Directory {
            opened: opened.clone(),
            stamp: directory_stamp(&held),
        },
    };
    edge.check()?;
    Ok((opened, edge))
}

enum Part {
    Root,
    Parent,
    Name { name: OsString, from_link: bool },
}

fn parts(path: &Path, from_link: bool) -> VecDeque<Part> {
    path.components()
        .filter_map(|part| match part {
            Component::RootDir => Some(Part::Root),
            Component::ParentDir => Some(Part::Parent),
            Component::Normal(name) => Some(Part::Name {
                name: name.to_owned(),
                from_link,
            }),
            _ => None,
        })
        .collect()
}

/// Retained descriptors prevent preparation from being redirected through a
/// replaced pathname. `check` still detects namespace replacement before use.
#[derive(Clone, Debug)]
pub(super) struct ParentObservation {
    root_stamp: String,
    edges: Vec<Edge>,
    anchor: Arc<File>,
    anchor_stamp: String,
    missing: Vec<OsString>,
    canonical: PathBuf,
}

impl ParentObservation {
    pub(super) fn capture(parent: &Path) -> Result<Self, RegistryFailure> {
        if !parent.is_absolute()
            || parent.components().any(|part| {
                !matches!(
                    part,
                    Component::RootDir | Component::Normal(_) | Component::ParentDir
                )
            })
            || parent.components().collect::<PathBuf>().as_os_str() != parent.as_os_str()
        {
            return Err(failure("invalid_state_parent"));
        }
        let root = open_root()?;
        let root_stamp = directory_stamp(
            &fstat(root.as_ref()).map_err(|_| failure("state_parent_unavailable"))?,
        );
        let mut directories = vec![root];
        let mut canonical = PathBuf::from("/");
        let mut queue = parts(parent, false);
        let mut edges = Vec::new();
        let mut links = 0;
        let mut visited = 0;
        let mut missing = Vec::new();
        while let Some(part) = queue.pop_front() {
            visited += 1;
            if visited + queue.len() > MAX_COMPONENTS {
                return Err(failure("state_parent_limit"));
            }
            let (name, from_link) = match part {
                Part::Root => {
                    directories.truncate(1);
                    canonical = PathBuf::from("/");
                    continue;
                }
                Part::Parent => {
                    if directories.len() > 1 {
                        directories.pop();
                        canonical.pop();
                    }
                    continue;
                }
                Part::Name { name, from_link } => (name, from_link),
            };
            let anchor = directories
                .last()
                .ok_or_else(|| failure("state_parent_unavailable"))?
                .clone();
            let stat = match statat(anchor.as_ref(), &name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => stat,
                Err(Errno::NOENT) if !from_link => {
                    missing.push(name);
                    for rest in queue {
                        match rest {
                            Part::Name {
                                name,
                                from_link: false,
                            } => missing.push(name),
                            _ => return Err(failure("state_parent_unavailable")),
                        }
                    }
                    break;
                }
                Err(_) => return Err(failure("state_parent_unavailable")),
            };
            if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
                links += 1;
                if links > MAX_LINKS {
                    return Err(failure("state_parent_limit"));
                }
                let target = read_link(anchor.as_ref(), &name)?;
                let edge = Edge {
                    parent: anchor,
                    name,
                    kind: EdgeKind::Link {
                        target: target.clone(),
                        stamp: link_stamp(&stat),
                    },
                };
                edge.check()?;
                edges.push(edge);
                let mut expanded = parts(&target, true);
                expanded.append(&mut queue);
                queue = expanded;
            } else {
                let (opened, edge) = directory_edge(anchor, name.clone(), &stat, false)?;
                edges.push(edge);
                directories.push(opened);
                canonical.push(name);
            }
        }
        let anchor = directories
            .pop()
            .ok_or_else(|| failure("state_parent_unavailable"))?;
        let stat = fstat(anchor.as_ref()).map_err(|_| failure("state_parent_unavailable"))?;
        if !safe_anchor(&stat) {
            return Err(failure("unsafe_state_parent"));
        }
        for name in &missing {
            canonical.push(name);
        }
        let observed = Self {
            root_stamp,
            edges,
            anchor,
            anchor_stamp: directory_stamp(&stat),
            missing,
            canonical,
        };
        observed.check()?;
        Ok(observed)
    }

    fn check_existing(&self) -> Result<(), RegistryFailure> {
        let current_root = open_root()?;
        let root = fstat(current_root.as_ref()).map_err(|_| failure("state_parent_changed"))?;
        if directory_stamp(&root) != self.root_stamp {
            return Err(failure("state_parent_changed"));
        }
        for edge in &self.edges {
            edge.check()?;
        }
        let anchor = fstat(self.anchor.as_ref()).map_err(|_| failure("state_parent_changed"))?;
        if !safe_anchor(&anchor) || directory_stamp(&anchor) != self.anchor_stamp {
            return Err(failure("state_parent_changed"));
        }
        Ok(())
    }

    pub(super) fn check(&self) -> Result<(), RegistryFailure> {
        self.check_existing()?;
        if let Some(name) = self.missing.first() {
            match statat(self.anchor.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW) {
                Err(Errno::NOENT) => {}
                _ => return Err(failure("state_parent_changed")),
            }
        }
        Ok(())
    }

    pub(super) fn canonical_path(&self) -> &Path {
        &self.canonical
    }

    pub(super) fn needs_preparation(&self) -> bool {
        !self.missing.is_empty()
    }

    pub(super) fn prepare(&self) -> Result<PreparedParent, RegistryFailure> {
        // Missing components can now exist because another initial registrant
        // prepared them. Existing anchor observations may never be refreshed.
        self.check_existing()?;
        let mut attempted = false;
        self.prepare_inner(&mut attempted).map_err(|error| {
            if attempted {
                failure("preparation_artifacts_may_remain")
            } else {
                error
            }
        })
    }

    fn prepare_inner(&self, attempted: &mut bool) -> Result<PreparedParent, RegistryFailure> {
        let mut parent = self.anchor.clone();
        let mut edges = Vec::new();
        for (index, name) in self.missing.iter().enumerate() {
            preparation_boundary(PreparationPoint::BeforeMkdir, index);
            self.check_existing()?;
            for edge in &edges {
                Edge::check(edge)?;
            }
            *attempted = true;
            match mkdirat(parent.as_ref(), name, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(_) => return Err(failure("state_parent_unavailable")),
            }
            preparation_boundary(PreparationPoint::AfterMkdir, index);
            let stat = statat(parent.as_ref(), name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| failure("state_parent_unavailable"))?;
            let (opened, edge) = directory_edge(parent, name.clone(), &stat, true)?;
            edges.push(edge);
            parent = opened;
        }
        preparation_boundary(PreparationPoint::BeforeFinish, self.missing.len());
        let prepared = PreparedParent {
            original: self.clone(),
            edges,
        };
        prepared.check()?;
        Ok(prepared)
    }
}

#[derive(Clone, Debug)]
pub(super) struct PreparedParent {
    original: ParentObservation,
    edges: Vec<Edge>,
}

impl PreparedParent {
    pub(super) fn check(&self) -> Result<(), RegistryFailure> {
        self.check_inner().map_err(|error| {
            if self.original.needs_preparation() {
                failure("preparation_artifacts_may_remain")
            } else {
                error
            }
        })
    }

    fn check_inner(&self) -> Result<(), RegistryFailure> {
        self.original.check_existing()?;
        for edge in &self.edges {
            edge.check()?;
        }
        Ok(())
    }

    pub(super) fn canonical_path(&self) -> &Path {
        self.original.canonical_path()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PreparationPoint {
    BeforeMkdir,
    AfterMkdir,
    BeforeFinish,
}

fn preparation_boundary(point: PreparationPoint, index: usize) {
    #[cfg(test)]
    tests::boundary(point, index);
    #[cfg(not(test))]
    let _ = (point, index);
}

#[cfg(test)]
mod tests {
    use super::super::RegistryEffect;
    use super::*;
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    struct Injection {
        point: PreparationPoint,
        index: usize,
        action: Box<dyn FnOnce()>,
    }
    thread_local! { static INJECTION: RefCell<Option<Injection>> = const { RefCell::new(None) }; }

    pub(super) fn boundary(point: PreparationPoint, index: usize) {
        let action = INJECTION.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot
                .as_ref()
                .is_some_and(|hook| hook.point == point && hook.index == index)
            {
                slot.take().map(|hook| hook.action)
            } else {
                None
            }
        });
        if let Some(action) = action {
            action();
        }
    }

    fn inject(point: PreparationPoint, index: usize, action: impl FnOnce() + 'static) {
        INJECTION.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Injection {
                point,
                index,
                action: Box::new(action),
            });
        });
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn absent_default_parent_observation_is_passive_and_cloneable() {
        let home = tempfile::tempdir().unwrap();
        let parent = home.path().join(".local/state");
        let observed = ParentObservation::capture(&parent).unwrap();
        assert!(observed.needs_preparation());
        assert_eq!(
            observed.canonical_path(),
            home.path().canonicalize().unwrap().join(".local/state")
        );
        observed.clone().check().unwrap();
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
        assert!(!parent.exists());
    }

    #[test]
    fn prepare_creates_only_missing_parent_components_and_preserves_legacy_mode() {
        let home = tempfile::tempdir().unwrap();
        chmod(home.path(), 0o755);
        let before = fs::metadata(home.path()).unwrap();
        let parent = home.path().join(".local/state");
        let observed = ParentObservation::capture(&parent).unwrap();
        let prepared = observed.prepare().unwrap();
        prepared.clone().check().unwrap();
        assert_eq!(prepared.canonical_path(), parent.canonicalize().unwrap());
        for path in [home.path().join(".local"), parent.clone()] {
            assert_eq!(fs::metadata(path).unwrap().mode() & 0o777, 0o700);
        }
        assert_eq!(before.ino(), fs::metadata(home.path()).unwrap().ino());
        assert_eq!(fs::metadata(home.path()).unwrap().mode() & 0o777, 0o755);
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        assert!(observed.check().is_err());
        let present = ParentObservation::capture(&parent).unwrap();
        assert!(!present.needs_preparation());
        present.prepare().unwrap().check().unwrap();
    }

    #[test]
    fn safe_already_exists_race_is_accepted_without_chmod() {
        let home = tempfile::tempdir().unwrap();
        let parent = home.path().join(".local/state");
        let observed = ParentObservation::capture(&parent).unwrap();
        let local = home.path().join(".local");
        let raced = local.clone();
        inject(PreparationPoint::BeforeMkdir, 0, move || {
            fs::create_dir(&raced).unwrap();
            chmod(&raced, 0o755);
        });
        observed.prepare().unwrap().check().unwrap();
        assert_eq!(fs::metadata(local).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn competing_initializers_share_the_prepared_parent_without_refreshing_anchor() {
        let home = tempfile::tempdir().unwrap();
        let parent = home.path().join(".local/state");
        let first = ParentObservation::capture(&parent).unwrap();
        let second = ParentObservation::capture(&parent).unwrap();
        let prepared_first = first.prepare().unwrap();
        assert!(second.check().is_err());
        let prepared_second = second.prepare().unwrap();
        assert_eq!(
            prepared_first.canonical_path(),
            prepared_second.canonical_path()
        );
        prepared_first.check().unwrap();
        prepared_second.check().unwrap();
    }

    #[test]
    fn malicious_already_exists_symlink_is_not_followed() {
        let home = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let observed = ParentObservation::capture(&home.path().join(".local/state")).unwrap();
        let local = home.path().join(".local");
        let target = foreign.path().to_owned();
        inject(PreparationPoint::BeforeMkdir, 0, move || {
            symlink(target, local).unwrap();
        });
        let failure = observed.prepare().unwrap_err();
        assert_eq!(failure.reason, "preparation_artifacts_may_remain");
        assert_eq!(failure.effect, RegistryEffect::NoChange);
        assert_eq!(fs::read_dir(foreign.path()).unwrap().count(), 0);
    }

    #[test]
    fn symlink_inserted_after_mkdir_is_not_opened_or_used_for_descendants() {
        let home = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let observed = ParentObservation::capture(&home.path().join(".local/state")).unwrap();
        let local = home.path().join(".local");
        let moved = home.path().join("retained-local");
        let moved_assert = moved.clone();
        let target = foreign.path().to_owned();
        inject(PreparationPoint::AfterMkdir, 0, move || {
            fs::rename(&local, &moved).unwrap();
            symlink(target, local).unwrap();
        });
        assert_eq!(
            observed.prepare().unwrap_err().reason,
            "preparation_artifacts_may_remain"
        );
        assert!(moved_assert.is_dir());
        assert_eq!(fs::read_dir(foreign.path()).unwrap().count(), 0);
        assert!(!moved_assert.join("state").exists());
    }

    #[test]
    fn completion_recheck_does_not_return_a_replaced_parent_as_prepared() {
        let home = tempfile::tempdir().unwrap();
        let parent = home.path().join(".local/state");
        let observed = ParentObservation::capture(&parent).unwrap();
        let moved = home.path().join("retained-state");
        inject(PreparationPoint::BeforeFinish, 2, move || {
            fs::rename(&parent, moved).unwrap();
            fs::create_dir(&parent).unwrap();
            chmod(&parent, 0o700);
        });
        assert_eq!(
            observed.prepare().unwrap_err().reason,
            "preparation_artifacts_may_remain"
        );
        assert!(home.path().join("retained-state").is_dir());
    }

    #[test]
    fn unsafe_raced_directory_and_regular_file_are_refused() {
        for directory in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let observed = ParentObservation::capture(&home.path().join(".local/state")).unwrap();
            let local = home.path().join(".local");
            inject(PreparationPoint::BeforeMkdir, 0, move || {
                if directory {
                    fs::create_dir(&local).unwrap();
                    chmod(&local, 0o777);
                } else {
                    fs::write(&local, b"foreign").unwrap();
                }
            });
            assert_eq!(
                observed.prepare().unwrap_err().reason,
                "preparation_artifacts_may_remain"
            );
            assert!(!home.path().join(".local/state").exists());
        }
    }

    #[test]
    fn anchor_alias_retarget_is_refused_before_any_creation() {
        let workspace = tempfile::tempdir().unwrap();
        let first = workspace.path().join("first");
        let second = workspace.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        let alias = workspace.path().join("alias");
        symlink(&first, &alias).unwrap();
        let observed = ParentObservation::capture(&alias.join(".local/state")).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&second, &alias).unwrap();
        assert!(observed.check().is_err());
        assert_eq!(
            observed.prepare().unwrap_err().reason,
            "state_parent_changed"
        );
        assert_eq!(fs::read_dir(first).unwrap().count(), 0);
        assert_eq!(fs::read_dir(second).unwrap().count(), 0);
    }

    #[test]
    fn parent_inode_replacement_is_not_accepted_as_a_new_baseline() {
        let workspace = tempfile::tempdir().unwrap();
        let anchor = workspace.path().join("home");
        fs::create_dir(&anchor).unwrap();
        let observed = ParentObservation::capture(&anchor.join(".local/state")).unwrap();
        fs::rename(&anchor, workspace.path().join("old-home")).unwrap();
        fs::create_dir(&anchor).unwrap();
        assert_eq!(
            observed.prepare().unwrap_err().reason,
            "state_parent_changed"
        );
        assert_eq!(fs::read_dir(anchor).unwrap().count(), 0);
    }

    #[test]
    fn prepared_component_replacement_stops_further_creation_and_retains_artifacts() {
        let home = tempfile::tempdir().unwrap();
        let observed = ParentObservation::capture(&home.path().join(".local/state")).unwrap();
        let local = home.path().join(".local");
        let moved = home.path().join("moved");
        let moved_assert = moved.clone();
        let local_assert = local.clone();
        inject(PreparationPoint::BeforeMkdir, 1, move || {
            fs::rename(&local, &moved).unwrap();
            fs::create_dir(&local).unwrap();
        });
        assert_eq!(
            observed.prepare().unwrap_err().reason,
            "preparation_artifacts_may_remain"
        );
        assert!(moved_assert.is_dir());
        assert!(!moved_assert.join("state").exists());
        assert!(!local_assert.join("state").exists());
    }

    #[test]
    fn final_prepared_check_detects_leaf_replacement_and_permission_drift() {
        let home = tempfile::tempdir().unwrap();
        let parent = home.path().join(".local/state");
        let prepared = ParentObservation::capture(&parent)
            .unwrap()
            .prepare()
            .unwrap();
        chmod(&parent, 0o777);
        assert!(prepared.check().is_err());
        chmod(&parent, 0o700);
        fs::rename(&parent, home.path().join("old-state")).unwrap();
        fs::create_dir(&parent).unwrap();
        chmod(&parent, 0o700);
        assert!(prepared.check().is_err());
    }

    #[test]
    fn dangling_alias_non_directory_and_writable_anchor_are_not_absence() {
        let home = tempfile::tempdir().unwrap();
        let alias = home.path().join("alias");
        symlink(home.path().join("not-created"), &alias).unwrap();
        assert!(ParentObservation::capture(&alias.join("state")).is_err());
        assert!(!home.path().join("not-created").exists());
        let file = home.path().join("file");
        fs::write(&file, b"foreign").unwrap();
        assert!(ParentObservation::capture(&file.join("state")).is_err());
        chmod(home.path(), 0o777);
        assert!(ParentObservation::capture(&home.path().join("missing/state")).is_err());
    }

    #[test]
    fn component_and_link_limits_refuse_without_creating_anything() {
        let home = tempfile::tempdir().unwrap();
        let mut path = home.path().to_owned();
        for _ in 0..257 {
            path.push("missing");
        }
        assert_eq!(
            ParentObservation::capture(&path).unwrap_err().reason,
            "state_parent_limit"
        );
        let alias = home.path().join("loop");
        symlink("loop", &alias).unwrap();
        assert_eq!(
            ParentObservation::capture(&alias).unwrap_err().reason,
            "state_parent_limit"
        );
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }
}
