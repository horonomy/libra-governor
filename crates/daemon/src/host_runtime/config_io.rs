//! Bounded observations for the concrete lifecycle's target and owned artifact.
//! The coordinator supplies serialization; these checks detect observed drift,
//! not a filesystem-wide compare-and-swap against noncooperating writers.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use super::config_record::FileFingerprint;
use super::parents::{ParentObservation, PreparedParent};
use super::RegistryFailure;

const MAX_BYTES: u64 = 1024 * 1024;

fn fail(reason: &'static str) -> RegistryFailure {
    RegistryFailure::new("configuration", reason)
}

fn flags() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}

fn identity(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    identity(a, b)
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.gid() == b.gid()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

fn safe_file(metadata: &Metadata, private: bool) -> bool {
    metadata.is_file()
        && metadata.uid() == rustix::process::getuid().as_raw()
        && metadata.nlink() == 1
        && metadata.mode() & if private { 0o077 } else { 0o022 } == 0
        && metadata.len() <= MAX_BYTES
}

fn metadata(path: &Path) -> Result<Option<Metadata>, RegistryFailure> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(fail("configuration_unavailable")),
    }
}

fn check_private_parent(path: &Path, private: bool) -> Result<(), RegistryFailure> {
    if private {
        if let Some(parent) = metadata(
            path.parent()
                .ok_or_else(|| fail("invalid_configuration_locator"))?,
        )? {
            if !parent.is_dir()
                || parent.uid() != rustix::process::getuid().as_raw()
                || parent.mode() & 0o077 != 0
            {
                return Err(fail("unsafe_artifact_directory"));
            }
        }
    }
    Ok(())
}

pub(super) struct FileObservation {
    path: PathBuf,
    locator: PathBuf,
    parent: ParentObservation,
    private: bool,
    metadata: Option<Metadata>,
    raw: Option<Vec<u8>>,
}

/// Always acquired after the lifecycle's root reservation. The original
/// preparation observation remains held through readback and catalog completion.
pub(super) struct LockedTarget {
    observation: FileObservation,
    parent: PreparedParent,
    guard: crate::write_lock::WriteLockGuard,
}

impl LockedTarget {
    pub(super) fn observation(&self) -> &FileObservation {
        &self.observation
    }

    pub(super) fn verify(&self) -> Result<(), RegistryFailure> {
        self.parent.check()?;
        self.guard
            .verify()
            .map_err(|_| fail("configuration_reservation_changed"))
    }

    pub(super) fn replace(
        &mut self,
        raw: &[u8],
        reservation: impl Fn() -> Result<(), RegistryFailure>,
    ) -> Result<(), RegistryFailure> {
        let after = self.observation.replace(raw, || {
            self.verify()?;
            reservation()
        })?;
        self.verify()
            .map_err(|failure| RegistryFailure::unconfirmed(failure.stage, failure.reason))?;
        after
            .check()
            .map_err(|failure| RegistryFailure::unconfirmed(failure.stage, failure.reason))?;
        self.observation = after;
        Ok(())
    }
}

impl FileObservation {
    pub(super) fn parent_missing(&self) -> bool {
        self.parent.needs_preparation()
    }

    pub(super) fn lock_target(self) -> Result<LockedTarget, RegistryFailure> {
        self.check()?;
        let prepared = self.parent.prepare()?;
        let result = (|| {
            self.check_prepared(&prepared)?;
            let guard = crate::write_lock::acquire_private(&self.path)
                .map_err(|_| fail("configuration_lock_unavailable"))?;
            self.check_prepared(&prepared)?;
            guard
                .verify()
                .map_err(|_| fail("configuration_reservation_changed"))?;
            let observation = Self::capture(&self.locator, self.private)?;
            self.check_prepared(&prepared)?;
            if observation.raw() != self.raw() {
                return Err(fail("configuration_changed"));
            }
            Ok(LockedTarget {
                observation,
                parent: prepared,
                guard,
            })
        })();
        result.map_err(|failure| {
            if self.parent.needs_preparation() {
                fail("preparation_artifacts_may_remain")
            } else {
                failure
            }
        })
    }

    /// No directory, lock, file or parser state is created by this observation.
    pub(super) fn capture(path: &Path, private: bool) -> Result<Self, RegistryFailure> {
        let locator = path.to_owned();
        if !path.is_absolute()
            || !path
                .components()
                .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
            || path.components().collect::<PathBuf>().as_os_str() != path.as_os_str()
        {
            return Err(fail("invalid_configuration_locator"));
        }
        let name = path
            .file_name()
            .ok_or_else(|| fail("invalid_configuration_locator"))?;
        check_private_parent(path, private)?;
        let parent = ParentObservation::capture(
            path.parent()
                .ok_or_else(|| fail("invalid_configuration_locator"))?,
        )?;
        let path = parent.canonical_path().join(name);
        let observed = metadata(&path)?;
        let raw = if let Some(observed) = &observed {
            if !safe_file(observed, private) {
                return Err(fail("unsafe_configuration_file"));
            }
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(flags())
                .open(&path)
                .map_err(|_| fail("configuration_unavailable"))?;
            let held = file
                .metadata()
                .map_err(|_| fail("configuration_unavailable"))?;
            if !safe_file(&held, private) || !unchanged(observed, &held) {
                return Err(fail("configuration_changed"));
            }
            let mut raw = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_BYTES + 1)
                .read_to_end(&mut raw)
                .map_err(|_| fail("configuration_read_failed"))?;
            let after = file
                .metadata()
                .map_err(|_| fail("configuration_unavailable"))?;
            if raw.len() as u64 > MAX_BYTES
                || raw.len() as u64 != held.len()
                || !unchanged(&held, &after)
            {
                return Err(fail("configuration_changed"));
            }
            Some(raw)
        } else {
            None
        };
        let result = Self {
            path,
            locator,
            parent,
            private,
            metadata: observed,
            raw,
        };
        result.check()?;
        Ok(result)
    }

    pub(super) fn raw(&self) -> Option<&[u8]> {
        self.raw.as_deref()
    }

    pub(super) fn fingerprint(&self) -> FileFingerprint {
        FileFingerprint::from_bytes(self.raw())
    }

    pub(super) fn document(&self) -> Result<Value, RegistryFailure> {
        let Some(raw) = self.raw() else {
            return Ok(serde_json::json!({}));
        };
        let value = libra_governor_protocol::host_event::validate_bounded_json(
            raw,
            MAX_BYTES as usize,
            16,
            4096,
        )
        .map_err(|_| fail("invalid_configuration_json"))?;
        if !value.is_object() {
            return Err(fail("configuration_not_object"));
        }
        Ok(value)
    }

    pub(super) fn check(&self) -> Result<(), RegistryFailure> {
        check_private_parent(&self.locator, self.private)?;
        self.parent.check()?;
        check_private_parent(&self.locator, self.private)?;
        self.check_file()
    }

    fn check_file(&self) -> Result<(), RegistryFailure> {
        match (&self.metadata, metadata(&self.path)?) {
            (None, None) => Ok(()),
            (Some(before), Some(after))
                if safe_file(&after, self.private) && unchanged(before, &after) =>
            {
                Ok(())
            }
            _ => Err(fail("configuration_changed")),
        }
    }

    /// The supplied reservation check is run before publication and after it.
    /// A failure after publication reports an unconfirmed effect, never rollback.
    pub(super) fn replace(
        &self,
        raw: &[u8],
        reservation: impl Fn() -> Result<(), RegistryFailure>,
    ) -> Result<Self, RegistryFailure> {
        if raw.len() as u64 > MAX_BYTES {
            return Err(fail("configuration_limit"));
        }
        self.check()?;
        reservation()?;
        if self.raw() == Some(raw) {
            let readback = Self::capture(&self.path, self.private)?;
            self.check()?;
            reservation()?;
            self.check()?;
            readback.check()?;
            if readback.raw() != self.raw() {
                return Err(fail("configuration_changed"));
            }
            return Ok(readback);
        }
        let prepared = self.parent.prepare()?;
        self.replace_prepared(raw, &reservation, &prepared)
            .map_err(|failure| {
                if failure.effect == super::RegistryEffect::NoChange
                    && self.parent.needs_preparation()
                {
                    fail("preparation_artifacts_may_remain")
                } else {
                    failure
                }
            })
    }

    fn replace_prepared(
        &self,
        raw: &[u8],
        reservation: &impl Fn() -> Result<(), RegistryFailure>,
        prepared: &PreparedParent,
    ) -> Result<Self, RegistryFailure> {
        self.check_prepared(prepared)?;
        reservation()?;
        let directory = self
            .path
            .parent()
            .ok_or_else(|| fail("invalid_configuration_locator"))?;
        let temporary_path = directory.join(format!(
            ".libra-config-{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let mode = self
            .metadata
            .as_ref()
            .map_or(0o600, |metadata| metadata.mode() & 0o777);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(flags())
            .open(&temporary_path)
            .map_err(|_| fail("configuration_temporary_unavailable"))?;
        let mut temporary = Temporary {
            path: temporary_path,
            identity: file
                .metadata()
                .map_err(|_| fail("configuration_temporary_artifacts_may_remain"))?,
            held: file,
            parent: prepared.clone(),
            private: self.private,
            published: false,
        };
        let result = (|| {
            // Set only our held temporary FD; global umask and the original file
            // remain untouched. Permission/ACL metadata beyond these bits is not promised.
            temporary
                .held
                .set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|_| fail("configuration_temporary_mode_failed"))?;
            temporary
                .held
                .write_all(raw)
                .and_then(|_| temporary.held.flush())
                .and_then(|_| temporary.held.sync_all())
                .map_err(|_| fail("configuration_write_failed"))?;
            temporary.identity = temporary
                .held
                .metadata()
                .map_err(|_| fail("configuration_temporary_artifacts_may_remain"))?;
            temporary.check()?;
            self.check_prepared(prepared)?;
            reservation()?;
            self.check_prepared(prepared)?;
            temporary.check_frozen()?;
            if self.metadata.is_none() {
                // Unlike rename, this cannot replace a concurrently appeared entry.
                fs::hard_link(&temporary.path, &self.path)
                    .map_err(|_| fail("configuration_creation_failed"))?;
                temporary.published = true;
                reservation().map_err(|_| unconfirmed())?;
                temporary
                    .check_linked(&self.path)
                    .map_err(|_| unconfirmed())?;
                fs::remove_file(&temporary.path).map_err(|_| unconfirmed())?;
            } else {
                fs::rename(&temporary.path, &self.path)
                    .map_err(|_| fail("configuration_replacement_failed"))?;
                temporary.published = true;
            }
            let verify = || {
                prepared.check()?;
                reservation()?;
                sync_directory(directory)?;
                let readback = Self::capture(&self.path, self.private)?;
                if readback.raw() != Some(raw)
                    || !readback.metadata.as_ref().is_some_and(|metadata| {
                        identity(metadata, &temporary.identity)
                            && metadata.mode() == temporary.identity.mode()
                            && metadata.len() == temporary.identity.len()
                    })
                {
                    return Err(fail("configuration_readback_changed"));
                }
                prepared.check()?;
                reservation()?;
                prepared.check()?;
                readback.check()?;
                Ok(readback)
            };
            verify().map_err(|_: RegistryFailure| unconfirmed())
        })();
        if result.is_err() && !temporary.published && !temporary.discard() {
            return Err(fail("configuration_temporary_artifacts_may_remain"));
        }
        result
    }

    fn check_prepared(&self, prepared: &PreparedParent) -> Result<(), RegistryFailure> {
        prepared.check()?;
        check_private_parent(&self.locator, self.private)?;
        self.check_file()
    }

    /// Only an exact observed private artifact may be removed by this helper.
    pub(super) fn remove(
        &self,
        reservation: impl Fn() -> Result<(), RegistryFailure>,
    ) -> Result<Self, RegistryFailure> {
        if !self.private {
            return Err(fail("artifact_removal_required"));
        }
        self.check()?;
        reservation()?;
        if self.metadata.is_none() {
            let readback = Self::capture(&self.path, true)?;
            self.check()?;
            reservation()?;
            self.check()?;
            readback.check()?;
            if readback.raw().is_some() {
                return Err(fail("configuration_changed"));
            }
            return Ok(readback);
        }
        self.check()?;
        fs::remove_file(&self.path).map_err(|_| fail("artifact_removal_failed"))?;
        let verify = || {
            self.parent.check()?;
            reservation()?;
            sync_directory(
                self.path
                    .parent()
                    .ok_or_else(|| fail("invalid_configuration_locator"))?,
            )?;
            let readback = Self::capture(&self.path, true)?;
            if readback.raw().is_some() {
                return Err(fail("artifact_removal_changed"));
            }
            reservation()?;
            self.parent.check()?;
            readback.check()?;
            Ok(readback)
        };
        verify().map_err(|_: RegistryFailure| unconfirmed())
    }
}

fn sync_directory(path: &Path) -> Result<(), RegistryFailure> {
    OpenOptions::new()
        .read(true)
        .custom_flags(flags())
        .open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| fail("configuration_directory_sync_failed"))
}

fn unconfirmed() -> RegistryFailure {
    RegistryFailure::unconfirmed("configuration", "configuration_effect_unconfirmed")
}

struct Temporary {
    path: PathBuf,
    identity: Metadata,
    held: File,
    parent: PreparedParent,
    private: bool,
    published: bool,
}

impl Temporary {
    fn check_frozen(&self) -> Result<(), RegistryFailure> {
        self.check()?;
        let current = self
            .held
            .metadata()
            .map_err(|_| fail("configuration_temporary_changed"))?;
        if !unchanged(&current, &self.identity) {
            return Err(fail("configuration_temporary_changed"));
        }
        Ok(())
    }
    fn discard(&self) -> bool {
        self.check().is_ok()
            && fs::remove_file(&self.path).is_ok()
            && self.parent.check().is_ok()
            && matches!(metadata(&self.path), Ok(None))
    }
    fn check_linked(&self, target: &Path) -> Result<(), RegistryFailure> {
        self.parent.check()?;
        let held = self
            .held
            .metadata()
            .map_err(|_| fail("configuration_temporary_changed"))?;
        let current =
            metadata(&self.path)?.ok_or_else(|| fail("configuration_temporary_changed"))?;
        let target = metadata(target)?.ok_or_else(|| fail("configuration_temporary_changed"))?;
        if !current.is_file()
            || current.uid() != rustix::process::getuid().as_raw()
            || current.nlink() != 2
            || !identity(&current, &self.identity)
            || current.mode() != self.identity.mode()
            || current.len() != self.identity.len()
            || !unchanged(&current, &held)
            || !unchanged(&current, &target)
        {
            return Err(fail("configuration_temporary_changed"));
        }
        Ok(())
    }

    fn check(&self) -> Result<(), RegistryFailure> {
        self.parent.check()?;
        let held = self
            .held
            .metadata()
            .map_err(|_| fail("configuration_temporary_changed"))?;
        let current =
            metadata(&self.path)?.ok_or_else(|| fail("configuration_temporary_changed"))?;
        if !safe_file(&current, self.private)
            || !identity(&current, &self.identity)
            || !unchanged(&current, &held)
        {
            return Err(fail("configuration_temporary_changed"));
        }
        Ok(())
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.published && self.check().is_ok() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_runtime::RegistryEffect;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn private_home() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        directory
    }

    #[test]
    #[ignore = "isolated restrictive-umask helper; invoked by its parent test"]
    fn restrictive_umask_helper() {
        let target = std::env::var_os("LIBRA_CONFIG_IO_UMASK_TARGET").unwrap();
        let target = PathBuf::from(target);
        // SAFETY: this test runs only in its dedicated child process.
        unsafe {
            libc::umask(0o077);
        }
        let observed = FileObservation::capture(&target, false).unwrap();
        observed.replace(b"{\"future\":true}", || Ok(())).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
    }

    #[test]
    fn restrictive_umask_does_not_change_existing_target_permissions() {
        let home = private_home();
        let target = home.path().join("settings.json");
        fs::write(&target, b"{}").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "host_runtime::config_io::tests::restrictive_umask_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("LIBRA_CONFIG_IO_UMASK_TARGET", &target)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let output = super::super::exec::spawn_test_child(&mut command)
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::read(&target).unwrap(), b"{\"future\":true}");
    }

    #[test]
    fn passive_absence_never_prepares_a_parent_and_explicit_replace_reads_back() {
        let home = private_home();
        let target = home.path().join("missing/settings.json");
        let observed = FileObservation::capture(&target, false).unwrap();
        assert!(observed.raw().is_none());
        assert!(observed.document().unwrap().as_object().unwrap().is_empty());
        assert!(!target.parent().unwrap().exists());
        let bytes = br#"{"future":{"value":[1,true]},"mcpServers":{"x":{}}}"#;
        let after = observed.replace(bytes, || Ok(())).unwrap();
        assert!(after.raw() == Some(bytes.as_slice()));
        assert!(after.fingerprint() == FileFingerprint::from_bytes(Some(bytes)));
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn replacement_preserves_existing_mode_and_noop_does_not_rewrite() {
        let home = private_home();
        let target = home.path().join("settings.json");
        fs::write(&target, b"{}").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        let observed = FileObservation::capture(&target, false).unwrap();
        let after = observed.replace(b"{\"future\":true}", || Ok(())).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().mode() & 0o777, 0o640);
        let before = fs::metadata(&target).unwrap();
        after.replace(after.raw().unwrap(), || Ok(())).unwrap();
        assert!(unchanged(&before, &fs::metadata(&target).unwrap()));
    }

    #[test]
    fn changed_or_appeared_target_is_preserved_without_temporary_files() {
        let home = private_home();
        let target = home.path().join("settings.json");
        let absent = FileObservation::capture(&target, false).unwrap();
        fs::write(&target, b"foreign").unwrap();
        assert!(absent.replace(b"{}", || Ok(())).is_err());
        let observed = FileObservation::capture(&target, false).unwrap();
        fs::write(&target, b"later user edit").unwrap();
        assert!(observed.replace(b"{}", || Ok(())).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"later user edit");
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[test]
    fn unsafe_entries_and_invalid_bounded_json_refuse() {
        let home = private_home();
        let target = home.path().join("settings.json");
        symlink("missing", &target).unwrap();
        assert!(FileObservation::capture(&target, false).is_err());
        fs::remove_file(&target).unwrap();
        use std::os::unix::ffi::OsStrExt;
        let fifo = std::ffi::CString::new(target.as_os_str().as_bytes()).unwrap();
        // SAFETY: the CString remains live and contains one terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(FileObservation::capture(&target, false).is_err());
        fs::remove_file(&target).unwrap();
        for raw in [br#"{"x":1,"x":2}"#.as_slice(), b"", b"[]"] {
            fs::write(&target, raw).unwrap();
            assert!(FileObservation::capture(&target, false)
                .unwrap()
                .document()
                .is_err());
        }
        fs::write(&target, vec![b' '; MAX_BYTES as usize + 1]).unwrap();
        assert!(FileObservation::capture(&target, false).is_err());
    }

    #[test]
    fn hardlinks_and_nonprivate_artifacts_refuse() {
        let home = private_home();
        let target = home.path().join("artifact.json");
        fs::write(&target, b"{}").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(FileObservation::capture(&target, true).is_err());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&target, home.path().join("alias")).unwrap();
        assert!(FileObservation::capture(&target, true).is_err());
        assert!(FileObservation::capture(&target, false).is_err());
    }

    #[test]
    fn parent_replacement_refuses_and_private_removal_preserves_later_edits() {
        let home = private_home();
        let parent = home.path().join("private");
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let target = parent.join("artifact.json");
        let before = FileObservation::capture(&target, true).unwrap();
        let artifact = before.replace(b"owned", || Ok(())).unwrap();
        fs::rename(&parent, home.path().join("old")).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(artifact.remove(|| Ok(())).is_err());
        fs::write(&target, b"later edit").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let current = FileObservation::capture(&target, true).unwrap();
        fs::write(&target, b"new user edit").unwrap();
        assert!(current.remove(|| Ok(())).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"new user edit");
        let current = FileObservation::capture(&target, true).unwrap();
        assert!(current.remove(|| Ok(())).unwrap().raw().is_none());
    }

    #[test]
    fn failed_postpublication_reservation_reports_unconfirmed_without_rollback() {
        use std::cell::Cell;
        let home = private_home();
        let target = home.path().join("settings.json");
        let before = FileObservation::capture(&target, false).unwrap();
        let calls = Cell::new(0);
        let result = before.replace(b"{\"future\":true}", || {
            calls.set(calls.get() + 1);
            if calls.get() == 5 {
                Err(fail("reservation_changed"))
            } else {
                Ok(())
            }
        });
        assert!(result.is_err());
        assert_eq!(
            result.err().unwrap().effect,
            RegistryEffect::EffectUnconfirmed
        );
        assert_eq!(fs::read(&target).unwrap(), b"{\"future\":true}");
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[test]
    fn noop_rejects_same_byte_inode_replacement_and_absent_remove_appearance() {
        use std::cell::Cell;
        let home = private_home();
        let target = home.path().join("settings.json");
        fs::write(&target, b"{}").unwrap();
        let observed = FileObservation::capture(&target, false).unwrap();
        let calls = Cell::new(0);
        let result = observed.replace(b"{}", || {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                let foreign = home.path().join("replacement");
                fs::write(&foreign, b"{}").unwrap();
                fs::rename(foreign, &target).unwrap();
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"{}");
        let absent_path = home.path().join("artifact.json");
        let absent = FileObservation::capture(&absent_path, true).unwrap();
        assert!(absent
            .remove(|| {
                fs::write(&absent_path, b"foreign").unwrap();
                fs::set_permissions(&absent_path, fs::Permissions::from_mode(0o600)).unwrap();
                Ok(())
            })
            .is_err());
        assert_eq!(fs::read(&absent_path).unwrap(), b"foreign");
    }

    #[test]
    fn replaced_temporary_name_after_publication_is_never_deleted() {
        use std::cell::Cell;
        let home = private_home();
        let target = home.path().join("artifact.json");
        let before = FileObservation::capture(&target, true).unwrap();
        let calls = Cell::new(0);
        let foreign_temporary = std::cell::RefCell::new(None);
        let result = before.replace(b"owned", || {
            calls.set(calls.get() + 1);
            if calls.get() == 4 {
                let temporary = fs::read_dir(home.path())
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| {
                        path.file_name()
                            .unwrap()
                            .to_string_lossy()
                            .starts_with(".libra-config-")
                    })
                    .unwrap();
                fs::remove_file(&temporary).unwrap();
                fs::write(&temporary, b"foreign replacement").unwrap();
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).unwrap();
                *foreign_temporary.borrow_mut() = Some(temporary);
            }
            Ok(())
        });
        assert_eq!(
            result.err().unwrap().effect,
            RegistryEffect::EffectUnconfirmed
        );
        assert_eq!(
            fs::read(foreign_temporary.borrow().as_ref().unwrap()).unwrap(),
            b"foreign replacement"
        );
        assert_eq!(fs::read(&target).unwrap(), b"owned");
        assert_eq!(fs::metadata(&target).unwrap().nlink(), 1);
    }

    #[test]
    fn final_readback_refuses_same_bytes_on_a_foreign_inode() {
        use std::cell::Cell;
        let home = private_home();
        let target = home.path().join("settings.json");
        let before = FileObservation::capture(&target, false).unwrap();
        let calls = Cell::new(0);
        let result = before.replace(b"{}", || {
            calls.set(calls.get() + 1);
            if calls.get() == 6 {
                let foreign = home.path().join("replacement");
                fs::write(&foreign, b"{}").unwrap();
                fs::rename(foreign, &target).unwrap();
            }
            Ok(())
        });
        assert_eq!(
            result.err().unwrap().effect,
            RegistryEffect::EffectUnconfirmed
        );
        assert_eq!(fs::read(&target).unwrap(), b"{}");
    }

    #[test]
    fn preparation_residue_is_explicit_after_a_prepublication_failure() {
        use std::cell::Cell;
        let home = private_home();
        let target = home.path().join("missing/artifact.json");
        let before = FileObservation::capture(&target, true).unwrap();
        let calls = Cell::new(0);
        let result = before.replace(b"owned", || {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                Err(fail("reservation_changed"))
            } else {
                Ok(())
            }
        });
        assert_eq!(
            result.err().unwrap().reason,
            "preparation_artifacts_may_remain"
        );
        assert!(target.parent().unwrap().is_dir());
        assert!(!target.exists());
        assert_eq!(fs::read_dir(target.parent().unwrap()).unwrap().count(), 0);
    }

    #[test]
    fn removal_rechecks_after_reservation_and_leaves_a_foreign_replacement() {
        let home = private_home();
        let target = home.path().join("artifact.json");
        fs::write(&target, b"owned").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let observed = FileObservation::capture(&target, true).unwrap();
        let result = observed.remove(|| {
            let foreign = home.path().join("replacement");
            fs::write(&foreign, b"foreign").unwrap();
            fs::set_permissions(&foreign, fs::Permissions::from_mode(0o600)).unwrap();
            fs::rename(foreign, &target).unwrap();
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"foreign");
    }

    #[test]
    fn private_artifact_directory_must_be_owned_private_and_not_an_alias() {
        let home = private_home();
        let public = home.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::set_permissions(&public, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(FileObservation::capture(&public.join("artifact.json"), true).is_err());
        fs::set_permissions(&public, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = home.path().join("alias");
        symlink(&public, &alias).unwrap();
        assert!(FileObservation::capture(&alias.join("artifact.json"), true).is_err());
        assert!(FileObservation::capture(&public.join("artifact.json"), true).is_ok());
    }

    #[test]
    fn private_parent_is_rechecked_even_if_generic_capture_observed_a_safe_public_mode() {
        let home = private_home();
        let target = home.path().join("artifact.json");
        let mut observation = FileObservation::capture(&target, true).unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o755)).unwrap();
        // Model the specific check/capture interleaving: generic parent safety
        // accepts 0755, whereas the concrete artifact predicate must refuse it.
        observation.parent = ParentObservation::capture(home.path()).unwrap();
        assert_eq!(
            observation.check().unwrap_err().reason,
            "unsafe_artifact_directory"
        );
    }
}
