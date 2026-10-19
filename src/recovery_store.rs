//! Crash-safe generations for bounded recovery snapshots and journals.
//!
//! A manifest points at an immutable checkpoint archive and the journal whose
//! header is bound to that archive's digest. Publishing a snapshot installs
//! both files before atomically switching the manifest, so startup can always
//! select a complete generation.

use crate::{project, recovery};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

const MANIFEST_NAME: &str = "manifest.json";
const MANIFEST_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024;

/// A validated checkpoint and its matching journal generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryGeneration {
    pub generation_id: Uuid,
    pub project_id: Uuid,
    pub schema_version: u32,
    /// Highest logical sequence covered by this checkpoint. Journal-local
    /// record numbering starts at one for each generation.
    pub sequence_high_water: u64,
    pub baseline_digest: [u8; 32],
    pub baseline_path: PathBuf,
    pub journal_path: PathBuf,
    pub used_fallback: bool,
    /// Non-fatal garbage-collection warning after successful publication.
    pub cleanup_warning: Option<String>,
}

/// Filesystem-backed generation selector. Its files live beside the journal
/// path in a dedicated, same-volume directory.
#[derive(Clone, Debug)]
pub struct RecoveryGenerationStore {
    directory: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    project_id: Uuid,
    schema_version: u32,
    active: ManifestGeneration,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    previous: Option<ManifestGeneration>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ManifestGeneration {
    id: Uuid,
    sequence_high_water: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublishPhase {
    BaselineInstalled,
    JournalInstalled,
    BeforeManifestSwitch,
    AfterManifestSwitch,
}

impl RecoveryGenerationStore {
    /// Derive the sidecar store directory from a project's journal path.
    pub fn from_journal_path(journal_path: &Path) -> Self {
        let mut name = journal_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("recovery"))
            .to_os_string();
        name.push(".store");
        Self {
            directory: journal_path.with_file_name(name),
        }
    }

    /// Remove this store's generations after the owning recovery session has
    /// been stopped and its lock is still held.
    pub fn remove_all(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Read and validate the manifest's active generation, falling back to
    /// its previous generation if the active pair is incomplete or corrupt.
    /// Orphaned snapshot/journal files are deliberately never considered.
    pub fn read_active(
        &self,
        project_id: Uuid,
        schema_version: u32,
    ) -> io::Result<Option<RecoveryGeneration>> {
        let manifest_path = self.directory.join(MANIFEST_NAME);
        let manifest = match read_manifest(&manifest_path)? {
            Some(manifest) => manifest,
            None => return Ok(None),
        };
        validate_manifest_identity(&manifest, project_id, schema_version)?;

        match self.validate_generation(project_id, schema_version, &manifest.active) {
            Ok(generation) => Ok(Some(generation)),
            Err(active_error) => {
                let Some(previous) = manifest.previous.as_ref() else {
                    return Err(invalid_data(format!(
                        "active recovery generation is invalid and no previous generation exists: {active_error}"
                    )));
                };
                let mut generation = self
                    .validate_generation(project_id, schema_version, previous)
                    .map_err(|previous_error| {
                        invalid_data(format!(
                            "active recovery generation is invalid ({active_error}); previous generation is invalid ({previous_error})"
                        ))
                    })?;
                generation.used_fallback = true;
                Ok(Some(generation))
            }
        }
    }

    /// Persist an immutable checkpoint and its empty matching journal, then
    /// atomically make that pair active by publishing the manifest last.
    pub fn publish_snapshot(
        &self,
        project_id: Uuid,
        schema_version: u32,
        sequence_high_water: u64,
        write_snapshot: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<RecoveryGeneration> {
        self.publish_snapshot_with_previous(
            project_id,
            schema_version,
            sequence_high_water,
            true,
            write_snapshot,
        )
    }

    /// Publish a new baseline without retaining the prior generation as a
    /// recovery fallback. Use only after an explicit Discard choice, so stale
    /// unsaved edits cannot be restored by a later fallback.
    pub fn publish_clean_snapshot(
        &self,
        project_id: Uuid,
        schema_version: u32,
        sequence_high_water: u64,
        write_snapshot: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<RecoveryGeneration> {
        self.publish_snapshot_with_previous(
            project_id,
            schema_version,
            sequence_high_water,
            false,
            write_snapshot,
        )
    }

    fn publish_snapshot_with_previous(
        &self,
        project_id: Uuid,
        schema_version: u32,
        sequence_high_water: u64,
        retain_previous: bool,
        write_snapshot: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<RecoveryGeneration> {
        self.publish_snapshot_with_hook(
            project_id,
            schema_version,
            sequence_high_water,
            retain_previous,
            write_snapshot,
            |_| Ok(()),
        )
    }

    fn publish_snapshot_with_hook(
        &self,
        project_id: Uuid,
        schema_version: u32,
        sequence_high_water: u64,
        retain_previous: bool,
        write_snapshot: impl FnOnce(&Path) -> Result<()>,
        phase_hook: impl FnMut(PublishPhase) -> Result<()>,
    ) -> Result<RecoveryGeneration> {
        self.publish_snapshot_with_hooks(
            project_id,
            schema_version,
            sequence_high_water,
            retain_previous,
            write_snapshot,
            phase_hook,
            |path| fs::remove_file(path),
        )
    }

    #[allow(clippy::too_many_arguments)] // Separate callbacks inject publication and cleanup faults.
    fn publish_snapshot_with_hooks(
        &self,
        project_id: Uuid,
        schema_version: u32,
        sequence_high_water: u64,
        retain_previous: bool,
        write_snapshot: impl FnOnce(&Path) -> Result<()>,
        mut phase_hook: impl FnMut(PublishPhase) -> Result<()>,
        remove_generation_file: impl FnMut(&Path) -> io::Result<()>,
    ) -> Result<RecoveryGeneration> {
        fs::create_dir_all(&self.directory)
            .with_context(|| format!("create recovery store {}", self.directory.display()))?;
        project::sync_parent_directory(&self.directory).context("sync parent of recovery store")?;

        let old_active = if retain_previous {
            self.valid_existing_active(project_id, schema_version)?
        } else {
            None
        };
        let generation_id = Uuid::new_v4();
        let staging_path = self.directory.join(format!("snapshot-{generation_id}.tmp"));
        let baseline_path = self.baseline_path(generation_id);
        let journal_path = self.journal_path(generation_id);
        let _staging_cleanup = RemoveOnDrop(staging_path.clone());

        write_snapshot(&staging_path).context("write recovery checkpoint snapshot")?;
        sync_file(&staging_path).context("sync recovery checkpoint snapshot")?;
        let snapshot = project::load_checkpoint(&staging_path)
            .context("validate recovery checkpoint snapshot")?;
        if snapshot.project_id != project_id {
            bail!("recovery checkpoint project identity does not match the requested project");
        }
        let baseline_digest = project::file_digest(&staging_path)
            .context("fingerprint recovery checkpoint snapshot")?;

        fs::rename(&staging_path, &baseline_path)
            .context("install immutable recovery checkpoint")?;
        project::sync_parent_directory(&baseline_path)
            .context("sync recovery checkpoint directory entry")?;
        phase_hook(PublishPhase::BaselineInstalled)?;

        let journal =
            recovery::Journal::create(&journal_path, project_id, baseline_digest, schema_version)
                .context("create recovery journal for checkpoint")?;
        drop(journal);
        project::sync_parent_directory(&journal_path)
            .context("sync recovery journal directory entry")?;
        phase_hook(PublishPhase::JournalInstalled)?;

        let manifest = Manifest {
            format: "kalium-recovery-generations".to_owned(),
            version: MANIFEST_VERSION,
            project_id,
            schema_version,
            active: ManifestGeneration {
                id: generation_id,
                sequence_high_water,
            },
            previous: retain_previous
                .then_some(old_active)
                .flatten()
                .map(|generation| ManifestGeneration {
                    id: generation.generation_id,
                    sequence_high_water: generation.sequence_high_water,
                }),
        };
        let manifest_bytes = serde_json::to_vec(&manifest).context("encode recovery manifest")?;
        if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
            bail!("recovery manifest exceeds 4 KiB");
        }
        phase_hook(PublishPhase::BeforeManifestSwitch)?;
        self.publish_manifest(&manifest_bytes)?;
        phase_hook(PublishPhase::AfterManifestSwitch)?;
        let cleanup_warning =
            self.cleanup_unreferenced_generations_with(&manifest, remove_generation_file);

        Ok(RecoveryGeneration {
            generation_id,
            project_id,
            schema_version,
            sequence_high_water,
            baseline_digest,
            baseline_path,
            journal_path,
            used_fallback: false,
            cleanup_warning,
        })
    }

    fn valid_existing_active(
        &self,
        project_id: Uuid,
        schema_version: u32,
    ) -> Result<Option<RecoveryGeneration>> {
        let manifest_path = self.directory.join(MANIFEST_NAME);
        let Some(manifest) = read_manifest(&manifest_path).context("read existing manifest")?
        else {
            return Ok(None);
        };
        validate_manifest_identity(&manifest, project_id, schema_version)
            .context("existing recovery manifest has a different project identity")?;
        Ok(self
            .validate_generation(project_id, schema_version, &manifest.active)
            .ok())
    }

    fn validate_generation(
        &self,
        project_id: Uuid,
        schema_version: u32,
        entry: &ManifestGeneration,
    ) -> io::Result<RecoveryGeneration> {
        let baseline_path = self.baseline_path(entry.id);
        let journal_path = self.journal_path(entry.id);
        let baseline = project::load_checkpoint(&baseline_path)
            .map_err(|error| invalid_data(format!("load recovery baseline: {error:#}")))?;
        if baseline.project_id != project_id {
            return Err(invalid_data("recovery baseline project identity mismatch"));
        }
        let baseline_digest = project::file_digest(&baseline_path)
            .map_err(|error| invalid_data(format!("digest recovery baseline: {error:#}")))?;
        recovery::scan(&journal_path, project_id, baseline_digest, schema_version)?;
        Ok(RecoveryGeneration {
            generation_id: entry.id,
            project_id,
            schema_version,
            sequence_high_water: entry.sequence_high_water,
            baseline_digest,
            baseline_path,
            journal_path,
            used_fallback: false,
            cleanup_warning: None,
        })
    }

    fn baseline_path(&self, generation_id: Uuid) -> PathBuf {
        self.directory
            .join(format!("baseline-{generation_id}.kalium"))
    }

    fn journal_path(&self, generation_id: Uuid) -> PathBuf {
        self.directory
            .join(format!("journal-{generation_id}.journal"))
    }

    fn publish_manifest(&self, bytes: &[u8]) -> Result<()> {
        let manifest_path = self.directory.join(MANIFEST_NAME);
        let temp_path = self
            .directory
            .join(format!("manifest-{}.tmp", Uuid::new_v4()));
        let _temp_cleanup = RemoveOnDrop(temp_path.clone());
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .context("create temporary recovery manifest")?;
        file.write_all(bytes)
            .context("write temporary recovery manifest")?;
        file.sync_all()
            .context("sync temporary recovery manifest")?;
        drop(file);
        project::replace_project_file(&temp_path, &manifest_path)
            .context("atomically publish recovery manifest")?;
        project::sync_parent_directory(&manifest_path)
            .context("sync recovery manifest directory entry")?;
        Ok(())
    }

    /// Best-effort cleanup after the new manifest is durably published. Keep
    /// both manifest-referenced generations; all other complete pairs are
    /// either superseded or orphaned by an interrupted pre-switch publish.
    /// Cleanup failure does not invalidate the active generation and can be
    /// retried by a later successful publication.
    fn cleanup_unreferenced_generations_with(
        &self,
        manifest: &Manifest,
        mut remove_file: impl FnMut(&Path) -> io::Result<()>,
    ) -> Option<String> {
        let mut retained = vec![manifest.active.id];
        if let Some(previous) = &manifest.previous {
            retained.push(previous.id);
        }
        let entries = match fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) => {
                return Some(format!(
                    "could not list recovery generations for cleanup: {error}"
                ));
            }
        };
        let mut warning = None;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    warning.get_or_insert_with(|| {
                        format!("could not inspect recovery generation during cleanup: {error}")
                    });
                    continue;
                }
            };
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_file() => {}
                Ok(_) => continue,
                Err(error) => {
                    warning.get_or_insert_with(|| {
                        format!(
                            "could not inspect {} during cleanup: {error}",
                            path.display()
                        )
                    });
                    continue;
                }
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let staging = name
                .strip_prefix("snapshot-")
                .and_then(|name| name.strip_suffix(".tmp"))
                .or_else(|| {
                    name.strip_prefix("manifest-")
                        .and_then(|name| name.strip_suffix(".tmp"))
                })
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_some();
            let id = name
                .strip_prefix("baseline-")
                .and_then(|name| name.strip_suffix(".kalium"))
                .or_else(|| {
                    name.strip_prefix("journal-")
                        .and_then(|name| name.strip_suffix(".journal"))
                })
                .and_then(|id| Uuid::parse_str(id).ok());
            if staging {
                if let Err(error) = remove_file(&path) {
                    if error.kind() != io::ErrorKind::NotFound {
                        warning.get_or_insert_with(|| {
                            format!(
                                "could not remove {} during cleanup: {error}",
                                path.display()
                            )
                        });
                    }
                }
                continue;
            }
            let Some(id) = id else {
                continue;
            };
            if !retained.contains(&id) {
                if let Err(error) = remove_file(&path) {
                    if error.kind() != io::ErrorKind::NotFound {
                        warning.get_or_insert_with(|| {
                            format!(
                                "could not remove {} during cleanup: {error}",
                                path.display()
                            )
                        });
                    }
                }
            }
        }
        warning
    }
}

fn read_manifest(path: &Path) -> io::Result<Option<Manifest>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let size = file.metadata()?.len();
    if size > MAX_MANIFEST_BYTES {
        return Err(invalid_data("recovery manifest exceeds 4 KiB"));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size as usize)
        .map_err(|_| io::Error::other("could not reserve recovery manifest memory"))?;
    file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(invalid_data("recovery manifest exceeds 4 KiB"));
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|error| invalid_data(format!("invalid recovery manifest: {error}")))?;
    if manifest.format != "kalium-recovery-generations" || manifest.version != MANIFEST_VERSION {
        return Err(invalid_data(
            "unsupported recovery manifest format or version",
        ));
    }
    if manifest.active.id.is_nil()
        || manifest
            .previous
            .as_ref()
            .is_some_and(|previous| previous.id.is_nil() || previous.id == manifest.active.id)
    {
        return Err(invalid_data(
            "recovery manifest contains invalid generation IDs",
        ));
    }
    Ok(Some(manifest))
}

fn validate_manifest_identity(
    manifest: &Manifest,
    project_id: Uuid,
    schema_version: u32,
) -> io::Result<()> {
    if manifest.project_id != project_id {
        return Err(invalid_data("recovery manifest project identity mismatch"));
    }
    if manifest.schema_version != schema_version {
        return Err(invalid_data("recovery manifest schema version mismatch"));
    }
    Ok(())
}

fn sync_file(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{document::Document, recovery::Journal};
    use anyhow::anyhow;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    const JOURNAL_SCHEMA: u32 = 1;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kalium-recovery-store-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(label: &str) -> (TempDir, RecoveryGenerationStore, Document) {
        let temp = TempDir::new(label);
        let journal_path = temp.0.join("project.kalium.journal");
        let store = RecoveryGenerationStore::from_journal_path(&journal_path);
        let doc = Document::new(64);
        (temp, store, doc)
    }

    fn publish(
        store: &RecoveryGenerationStore,
        doc: &Document,
        high_water: u64,
    ) -> RecoveryGeneration {
        store
            .publish_snapshot(doc.project_id, JOURNAL_SCHEMA, high_water, |path| {
                project::save_checkpoint(doc, path)
            })
            .unwrap()
    }

    #[test]
    fn active_generation_reads_and_reports_sequence_high_water() {
        let (_temp, store, doc) = fixture("active");
        let generation = publish(&store, &doc, 73);
        let active = store
            .read_active(doc.project_id, JOURNAL_SCHEMA)
            .unwrap()
            .unwrap();
        assert_eq!(active, generation);
        assert_eq!(active.sequence_high_water, 73);
        assert!(!active.used_fallback);
    }

    #[test]
    fn corrupt_active_generation_falls_back_to_previous() {
        let (_temp, store, doc) = fixture("fallback");
        let previous = publish(&store, &doc, 4);
        let active = publish(&store, &doc, 9);
        fs::write(&active.baseline_path, b"corrupt checkpoint").unwrap();
        let selected = store
            .read_active(doc.project_id, JOURNAL_SCHEMA)
            .unwrap()
            .unwrap();
        assert_eq!(selected.generation_id, previous.generation_id);
        assert_eq!(selected.sequence_high_water, 4);
        assert!(selected.used_fallback);
    }

    #[test]
    fn failed_snapshot_closure_preserves_the_active_manifest() {
        let (_temp, store, doc) = fixture("failed-snapshot");
        let previous = publish(&store, &doc, 11);
        let manifest_before = fs::read(store.directory.join(MANIFEST_NAME)).unwrap();
        let result = store.publish_snapshot(doc.project_id, JOURNAL_SCHEMA, 12, |_path| {
            Err(anyhow!("injected snapshot failure"))
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read(store.directory.join(MANIFEST_NAME)).unwrap(),
            manifest_before
        );
        assert_eq!(
            store
                .read_active(doc.project_id, JOURNAL_SCHEMA)
                .unwrap()
                .unwrap()
                .generation_id,
            previous.generation_id
        );
    }

    #[test]
    fn failures_before_manifest_switch_keep_previous_generation_active() {
        for (label, failing_phase) in [
            ("after-baseline", PublishPhase::BaselineInstalled),
            ("after-journal", PublishPhase::JournalInstalled),
            ("before-switch", PublishPhase::BeforeManifestSwitch),
        ] {
            let (_temp, store, doc) = fixture(label);
            let previous = publish(&store, &doc, 21);
            let manifest_before = fs::read(store.directory.join(MANIFEST_NAME)).unwrap();
            let result = store.publish_snapshot_with_hook(
                doc.project_id,
                JOURNAL_SCHEMA,
                22,
                true,
                |path| project::save_checkpoint(&doc, path),
                |phase| {
                    if phase == failing_phase {
                        Err(anyhow!("injected publication failure at {phase:?}"))
                    } else {
                        Ok(())
                    }
                },
            );

            assert!(result.is_err(), "phase {failing_phase:?} should fail");
            assert_eq!(
                fs::read(store.directory.join(MANIFEST_NAME)).unwrap(),
                manifest_before,
                "phase {failing_phase:?} must not replace the active pointer"
            );
            let selected = store
                .read_active(doc.project_id, JOURNAL_SCHEMA)
                .unwrap()
                .unwrap();
            assert_eq!(selected.generation_id, previous.generation_id);
            assert!(!selected.used_fallback);
        }
    }

    #[test]
    fn failure_after_manifest_switch_leaves_new_generation_active() {
        let (_temp, store, doc) = fixture("after-switch");
        let previous = publish(&store, &doc, 31);
        let result = store.publish_snapshot_with_hook(
            doc.project_id,
            JOURNAL_SCHEMA,
            32,
            true,
            |path| project::save_checkpoint(&doc, path),
            |phase| {
                if phase == PublishPhase::AfterManifestSwitch {
                    Err(anyhow!("injected post-switch failure"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(result.is_err());
        let active = store
            .read_active(doc.project_id, JOURNAL_SCHEMA)
            .unwrap()
            .unwrap();
        assert_ne!(active.generation_id, previous.generation_id);
        assert_eq!(active.sequence_high_water, 32);
        assert!(!active.used_fallback);
    }

    #[test]
    fn clean_generation_does_not_fall_back_to_discarded_edits() {
        let (_temp, store, doc) = fixture("clean-no-fallback");
        let old = publish(&store, &doc, 41);
        let clean = store
            .publish_clean_snapshot(doc.project_id, JOURNAL_SCHEMA, 0, |path| {
                project::save_checkpoint(&doc, path)
            })
            .unwrap();
        assert_ne!(clean.generation_id, old.generation_id);
        fs::write(&clean.baseline_path, b"corrupt clean baseline").unwrap();
        assert!(store.read_active(doc.project_id, JOURNAL_SCHEMA).is_err());
    }

    #[test]
    fn successful_publication_retains_active_and_previous_and_reclaims_older_pairs() {
        let (_temp, store, doc) = fixture("generation-retention");
        let oldest = publish(&store, &doc, 1);
        let previous = publish(&store, &doc, 2);
        assert!(oldest.baseline_path.exists());
        assert!(oldest.journal_path.exists());

        let active = publish(&store, &doc, 3);
        assert!(!oldest.baseline_path.exists());
        assert!(!oldest.journal_path.exists());
        assert!(previous.baseline_path.exists());
        assert!(previous.journal_path.exists());
        assert!(active.baseline_path.exists());
        assert!(active.journal_path.exists());

        let orphan_path = store
            .directory
            .join(format!("baseline-{}.kalium", Uuid::new_v4()));
        let orphan_snapshot = store
            .directory
            .join(format!("snapshot-{}.tmp", Uuid::new_v4()));
        let orphan_manifest = store
            .directory
            .join(format!("manifest-{}.tmp", Uuid::new_v4()));
        let unrelated = store.directory.join("unrelated.tmp");
        fs::write(&orphan_path, b"orphan from an interrupted publication").unwrap();
        fs::write(&orphan_snapshot, b"partial snapshot from a killed process").unwrap();
        fs::write(&orphan_manifest, b"partial manifest from a killed process").unwrap();
        fs::write(&unrelated, b"preserve unrelated files").unwrap();
        let next = publish(&store, &doc, 4);
        let selected = store
            .read_active(doc.project_id, JOURNAL_SCHEMA)
            .unwrap()
            .unwrap();
        assert_eq!(selected.generation_id, next.generation_id);
        assert!(!previous.baseline_path.exists());
        assert!(!previous.journal_path.exists());
        assert!(active.baseline_path.exists());
        assert!(next.baseline_path.exists());
        assert!(next.journal_path.exists());
        assert!(!orphan_path.exists());
        assert!(!orphan_snapshot.exists());
        assert!(!orphan_manifest.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"preserve unrelated files");
    }

    #[test]
    fn cleanup_failure_is_reported_without_invalidating_published_generation() {
        let (_temp, store, doc) = fixture("cleanup-warning");
        let older = publish(&store, &doc, 1);
        let active = publish(&store, &doc, 2);
        let next = store
            .publish_snapshot_with_hooks(
                doc.project_id,
                JOURNAL_SCHEMA,
                3,
                true,
                |path| project::save_checkpoint(&doc, path),
                |_| Ok(()),
                |_path| {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "injected cleanup failure",
                    ))
                },
            )
            .unwrap();
        assert!(next
            .cleanup_warning
            .as_deref()
            .is_some_and(|message| message.contains("injected cleanup failure")));
        assert!(older.baseline_path.exists());
        assert!(older.journal_path.exists());
        assert!(active.baseline_path.exists());
        assert!(active.journal_path.exists());
        assert_eq!(
            store
                .read_active(doc.project_id, JOURNAL_SCHEMA)
                .unwrap()
                .unwrap()
                .generation_id,
            next.generation_id
        );
        assert_eq!(
            store
                .read_active(doc.project_id, JOURNAL_SCHEMA)
                .unwrap()
                .unwrap()
                .sequence_high_water,
            3
        );
    }

    #[test]
    fn new_journal_is_empty_bound_to_snapshot_and_can_be_reopened_and_appended() {
        let (_temp, store, doc) = fixture("empty-journal");
        let generation = publish(&store, &doc, 0);
        assert!(recovery::scan(
            &generation.journal_path,
            doc.project_id,
            generation.baseline_digest,
            JOURNAL_SCHEMA
        )
        .unwrap()
        .is_empty());
        let (mut journal, records) = Journal::open(
            &generation.journal_path,
            doc.project_id,
            generation.baseline_digest,
            JOURNAL_SCHEMA,
        )
        .unwrap();
        assert!(records.is_empty());
        assert_eq!(journal.append(b"next generation event").unwrap(), 1);
        drop(journal);
        assert_eq!(
            recovery::scan(
                &generation.journal_path,
                doc.project_id,
                generation.baseline_digest,
                JOURNAL_SCHEMA
            )
            .unwrap()[0]
                .sequence,
            1
        );
    }
}
