//! Bounded per-user history of recently opened or saved native projects.
//!
//! This data is stored outside project files. Mutations are persisted through
//! a synced same-directory temporary and atomic replacement before becoming
//! visible in memory.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const FORMAT_VERSION: u64 = 1;
const MAX_ENTRIES: usize = 12;
const MAX_PATH_BYTES: usize = 4096;
const MAX_FILE_BYTES: u64 = 32 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Recent project paths, newest first.
pub struct RecentProjects {
    path: PathBuf,
    entries: Vec<PathBuf>,
    entry_fields: Vec<Map<String, Value>>,
    root_fields: Map<String, Value>,
}

impl RecentProjects {
    /// Load the version-1 list. A missing file is an empty list.
    pub fn load(path: PathBuf) -> Result<Self> {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty(path))
            }
            Err(error) => {
                return Err(error).with_context(|| format!("cannot open {}", path.display()))
            }
        };

        let metadata_len = file
            .metadata()
            .with_context(|| format!("cannot inspect {}", path.display()))?
            .len();
        if metadata_len > MAX_FILE_BYTES {
            bail!("recent-project file exceeds the {MAX_FILE_BYTES}-byte limit");
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            bail!("recent-project file exceeds the {MAX_FILE_BYTES}-byte limit");
        }

        let mut root: Map<String, Value> = serde_json::from_slice::<Value>(&bytes)
            .with_context(|| format!("invalid recent-project file {}", path.display()))?
            .as_object()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("recent-project file root must be an object"))?;
        let version = root
            .remove("version")
            .and_then(|value| value.as_u64())
            .ok_or_else(|| anyhow::anyhow!("recent-project file has no valid version"))?;
        if version != FORMAT_VERSION {
            bail!("unsupported recent-project file version {version}");
        }
        let stored_entries = root
            .remove("entries")
            .and_then(|value| value.as_array().cloned())
            .ok_or_else(|| anyhow::anyhow!("recent-project entries must be an array"))?;
        if stored_entries.len() > MAX_ENTRIES {
            bail!("recent-project file has more than {MAX_ENTRIES} entries");
        }

        let mut entries = Vec::with_capacity(stored_entries.len());
        let mut entry_fields = Vec::with_capacity(stored_entries.len());
        for mut stored in stored_entries {
            let object = stored
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("recent-project entry must be an object"))?;
            let path_value = object
                .remove("path")
                .and_then(|value| value.as_str().map(str::to_owned))
                .ok_or_else(|| anyhow::anyhow!("recent-project entry has no string path"))?;
            let entry_path = PathBuf::from(&path_value);
            validate_path(&entry_path)?;
            if entries.contains(&entry_path) {
                bail!("recent-project file contains duplicate paths");
            }
            entries.push(entry_path);
            entry_fields.push(std::mem::take(object));
        }

        Ok(Self {
            path,
            entries,
            entry_fields,
            root_fields: root,
        })
    }

    /// Create an empty list associated with an explicit storage path.
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
            entry_fields: Vec::new(),
            root_fields: Map::new(),
        }
    }

    /// Return paths newest first. Missing files are retained for explicit UI removal.
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// Canonicalize an existing project file, move it to the front, and persist.
    pub fn record(&mut self, path: &Path) -> Result<()> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("cannot resolve recent project {}", path.display()))?;
        if !canonical.is_file() {
            bail!("recent project is not a file: {}", path.display());
        }
        validate_path(&canonical)?;

        let mut next_entries = Vec::with_capacity(self.entries.len() + 1);
        let mut next_fields = Vec::with_capacity(self.entry_fields.len() + 1);
        let mut retained_fields = None;
        for (entry, fields) in self.entries.iter().zip(&self.entry_fields) {
            let matches_recorded_file = entry == &canonical
                || entry
                    .canonicalize()
                    .is_ok_and(|resolved| resolved == canonical);
            if matches_recorded_file {
                if retained_fields.is_none() {
                    retained_fields = Some(fields.clone());
                }
            } else {
                next_entries.push(entry.clone());
                next_fields.push(fields.clone());
            }
        }
        next_entries.insert(0, canonical);
        next_fields.insert(0, retained_fields.unwrap_or_default());
        next_entries.truncate(MAX_ENTRIES);
        next_fields.truncate(MAX_ENTRIES);
        self.commit(next_entries, next_fields, persist_entries)
    }

    /// Remove a path, including a now-missing path, and persist before commit.
    pub fn remove(&mut self, path: &Path) -> Result<()> {
        let target = comparable_path(path)?;
        let mut next_entries = Vec::with_capacity(self.entries.len());
        let mut next_fields = Vec::with_capacity(self.entry_fields.len());
        for (entry, fields) in self.entries.iter().zip(&self.entry_fields) {
            if comparable_path(entry)? != target {
                next_entries.push(entry.clone());
                next_fields.push(fields.clone());
            }
        }
        self.commit(next_entries, next_fields, persist_entries)
    }

    /// Clear all paths while retaining unknown top-level extension fields.
    pub fn clear(&mut self) -> Result<()> {
        self.commit(Vec::new(), Vec::new(), persist_entries)
    }

    fn commit<F>(
        &mut self,
        entries: Vec<PathBuf>,
        entry_fields: Vec<Map<String, Value>>,
        persist: F,
    ) -> Result<()>
    where
        F: FnOnce(&Path, &[PathBuf], &[Map<String, Value>], &Map<String, Value>) -> Result<()>,
    {
        if entries.len() != entry_fields.len() || entries.len() > MAX_ENTRIES {
            bail!("invalid recent-project entry state");
        }
        for entry in &entries {
            validate_path(entry)?;
        }
        persist(&self.path, &entries, &entry_fields, &self.root_fields)?;
        self.entries = entries;
        self.entry_fields = entry_fields;
        Ok(())
    }
}

fn validate_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        bail!("recent-project paths must be absolute");
    }
    let text = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("recent-project path is not valid UTF-8"))?;
    if text.contains('\0') {
        bail!("recent-project paths cannot contain NUL bytes");
    }
    if text.len() > MAX_PATH_BYTES {
        bail!("recent-project path exceeds {MAX_PATH_BYTES} UTF-8 bytes");
    }
    Ok(())
}

fn comparable_path(path: &Path) -> Result<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        validate_path(&canonical)?;
        return Ok(canonical);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    validate_path(&normalized)?;
    Ok(normalized)
}

fn encode_file(
    entries: &[PathBuf],
    entry_fields: &[Map<String, Value>],
    root_fields: &Map<String, Value>,
) -> Result<Vec<u8>> {
    if entries.len() != entry_fields.len() || entries.len() > MAX_ENTRIES {
        bail!("invalid recent-project entry state");
    }
    let mut root = root_fields.clone();
    root.insert("version".into(), Value::from(FORMAT_VERSION));
    let mut encoded_entries = Vec::with_capacity(entries.len());
    for (path, extra) in entries.iter().zip(entry_fields) {
        validate_path(path)?;
        let mut object = extra.clone();
        object.insert(
            "path".into(),
            Value::String(
                path.to_str()
                    .ok_or_else(|| anyhow::anyhow!("recent-project path is not valid UTF-8"))?
                    .to_owned(),
            ),
        );
        encoded_entries.push(Value::Object(object));
    }
    root.insert("entries".into(), Value::Array(encoded_entries));
    let bytes = serde_json::to_vec(&Value::Object(root))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("serialized recent-project file exceeds the {MAX_FILE_BYTES}-byte limit");
    }
    Ok(bytes)
}

fn persist_entries(
    path: &Path,
    entries: &[PathBuf],
    entry_fields: &[Map<String, Value>],
    root_fields: &Map<String, Value>,
) -> Result<()> {
    let bytes = encode_file(entries, entry_fields, root_fields)?;
    persist_bytes_with_replace(path, &bytes, replace_file)
}

fn persist_bytes_with_replace<F>(path: &Path, bytes: &[u8], replace: F) -> Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("serialized recent-project file exceeds the {MAX_FILE_BYTES}-byte limit");
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "cannot create recent-project directory {}",
            parent.display()
        )
    })?;

    let (temp_path, mut temp_file) = create_unique_file(parent, "tmp")?;
    let backup_path = match create_backup(path, parent) {
        Ok(backup) => backup,
        Err(error) => {
            drop(temp_file);
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
    };

    let result = (|| -> Result<()> {
        temp_file
            .write_all(bytes)
            .with_context(|| format!("cannot write {}", temp_path.display()))?;
        temp_file
            .sync_all()
            .with_context(|| format!("cannot sync {}", temp_path.display()))?;
        drop(temp_file);
        replace(&temp_path, path)
            .with_context(|| format!("cannot replace recent-project file {}", path.display()))?;
        if let Err(sync_error) = sync_parent(parent) {
            let rollback = match backup_path.as_deref() {
                Some(backup) => replace_file(backup, path),
                None => fs::remove_file(path),
            };
            let rollback_sync = sync_parent(parent);
            let details = match (rollback, rollback_sync) {
                (Ok(()), Ok(())) => String::new(),
                (rollback, rollback_sync) => format!(
                    "; rollback: {}; directory sync: {}",
                    rollback
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "ok".into()),
                    rollback_sync
                        .err()
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "ok".into())
                ),
            };
            bail!(
                "cannot sync recent-project directory {} after replacement: {sync_error}{details}",
                parent.display()
            );
        }
        Ok(())
    })();

    if result.is_err() {
        let replacement_was_not_applied = temp_path.exists();
        let _ = fs::remove_file(&temp_path);
        if replacement_was_not_applied {
            if let Some(backup) = backup_path.as_deref() {
                let _ = fs::remove_file(backup);
            }
        }
    } else if let Some(backup) = backup_path.as_deref() {
        let _ = fs::remove_file(backup);
    }
    result
}

fn create_unique_file(parent: &Path, suffix: &str) -> Result<(PathBuf, File)> {
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".kalium-recent-projects-{}-{sequence}.{suffix}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("cannot create {}", candidate.display()))
            }
        }
    }
    bail!("could not allocate a unique recent-project temporary file")
}

fn create_backup(path: &Path, parent: &Path) -> Result<Option<PathBuf>> {
    let mut old_file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot preserve {}", path.display()))
        }
    };
    if old_file.metadata()?.len() > MAX_FILE_BYTES {
        bail!("cannot replace an oversized recent-project file safely");
    }
    let (backup_path, mut backup_file) = create_unique_file(parent, "bak")?;
    let result = (|| -> std::io::Result<()> {
        copy_capped(&mut old_file, &mut backup_file, MAX_FILE_BYTES)?;
        backup_file.sync_all()
    })();
    if let Err(error) = result {
        drop(backup_file);
        let _ = fs::remove_file(&backup_path);
        return Err(error).with_context(|| {
            format!(
                "cannot preserve prior recent-project file {}",
                path.display()
            )
        });
    }
    Ok(Some(backup_path))
}

fn copy_capped(reader: &mut impl Read, writer: &mut impl Write, limit: u64) -> std::io::Result<()> {
    let copied = std::io::copy(&mut reader.take(limit + 1), writer)?;
    if copied > limit {
        return Err(std::io::Error::other(
            "source grew beyond the bounded backup limit",
        ));
    }
    Ok(())
}

fn replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(source, destination)
    }
    #[cfg(windows)]
    {
        use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
        const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
        fn wide(path: &OsStr) -> Vec<u16> {
            path.encode_wide().chain(std::iter::once(0)).collect()
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn ReplaceFileW(
                replaced_file_name: *const u16,
                replacement_file_name: *const u16,
                backup_file_name: *const u16,
                replace_flags: u32,
                exclude: *mut std::ffi::c_void,
                reserved: *mut std::ffi::c_void,
            ) -> i32;
            fn MoveFileExW(
                existing_file_name: *const u16,
                new_file_name: *const u16,
                flags: u32,
            ) -> i32;
        }
        let source_wide = wide(source.as_os_str());
        let destination_wide = wide(destination.as_os_str());
        // SAFETY: both pointers refer to NUL-terminated buffers for this call.
        let succeeded = unsafe {
            if destination.exists() {
                ReplaceFileW(
                    destination_wide.as_ptr(),
                    source_wide.as_ptr(),
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            } else {
                MoveFileExW(
                    source_wide.as_ptr(),
                    destination_wide.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            }
        };
        if succeeded == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        fs::rename(source, destination)
    }
}

fn sync_parent(parent: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(parent)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "kalium-recent-projects-{}-{stamp}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn storage(&self) -> PathBuf {
            self.0.join("recent-projects.json")
        }

        fn project(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::write(&path, b"native project placeholder").unwrap();
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn missing_file_loads_empty() {
        let temp = TestDir::new();
        let projects = RecentProjects::load(temp.storage()).unwrap();
        assert!(projects.entries().is_empty());
    }

    #[test]
    fn read_is_capped_even_when_metadata_is_oversized() {
        let temp = TestDir::new();
        let path = temp.storage();
        let file = File::create(&path).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        assert!(RecentProjects::load(path)
            .err()
            .unwrap()
            .to_string()
            .contains("limit"));
    }

    #[test]
    fn malformed_versions_and_path_limits_are_rejected() {
        let temp = TestDir::new();
        let path = temp.storage();
        for bytes in [
            br"{".to_vec(),
            br#"{"version":2,"entries":[]}"#.to_vec(),
            br#"{"version":1,"entries":[{"path":"relative.kalium"}]}"#.to_vec(),
            br#"{"version":1,"entries":[{"path":"/bad\u0000path.kalium"}]}"#.to_vec(),
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(RecentProjects::load(path.clone()).is_err());
        }
        let too_long = format!("/{}", "p".repeat(MAX_PATH_BYTES));
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "entries": [{"path": too_long}]
        }))
        .unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(RecentProjects::load(path).is_err());
    }

    #[test]
    fn record_orders_deduplicates_and_caps_entries() {
        let temp = TestDir::new();
        let mut projects = RecentProjects::empty(temp.storage());
        let files = (0..15)
            .map(|index| temp.project(&format!("project-{index}.kalium")))
            .collect::<Vec<_>>();
        for file in &files {
            projects.record(file).unwrap();
        }
        assert_eq!(projects.entries().len(), MAX_ENTRIES);
        assert_eq!(projects.entries()[0], files[14].canonicalize().unwrap());
        assert!(!projects
            .entries()
            .contains(&files[0].canonicalize().unwrap()));

        projects.record(&files[10]).unwrap();
        assert_eq!(projects.entries()[0], files[10].canonicalize().unwrap());
        assert_eq!(projects.entries().len(), MAX_ENTRIES);
        assert_eq!(
            projects
                .entries()
                .iter()
                .filter(|entry| **entry == files[10].canonicalize().unwrap())
                .count(),
            1
        );
    }

    #[test]
    fn unknown_root_and_entry_fields_survive_rewrites() {
        let temp = TestDir::new();
        let old = temp.project("old.kalium").canonicalize().unwrap();
        let new = temp.project("new.kalium");
        let storage = temp.storage();
        fs::write(
            &storage,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "future_root": {"keep": true},
                "entries": [{"path": old, "future_entry": [3, 4]}]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut projects = RecentProjects::load(storage.clone()).unwrap();
        projects.record(&new).unwrap();
        let rewritten: Value = serde_json::from_slice(&fs::read(storage).unwrap()).unwrap();
        assert_eq!(rewritten["future_root"]["keep"], true);
        assert_eq!(
            rewritten["entries"][1]["future_entry"],
            serde_json::json!([3, 4])
        );
    }

    #[cfg(unix)]
    #[test]
    fn record_collapses_all_canonical_aliases_and_keeps_newest_fields() {
        use std::os::unix::fs::symlink;

        let temp = TestDir::new();
        let real = temp.project("real.kalium").canonicalize().unwrap();
        let alias = temp.0.join("alias.kalium");
        symlink(&real, &alias).unwrap();
        let storage = temp.storage();
        fs::write(
            &storage,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "entries": [
                    {"path": alias, "origin": "newest"},
                    {"path": real, "origin": "older"}
                ]
            }))
            .unwrap(),
        )
        .unwrap();

        let mut projects = RecentProjects::load(storage.clone()).unwrap();
        projects.record(&real).unwrap();
        assert_eq!(projects.entries(), &[real]);
        let rewritten: Value = serde_json::from_slice(&fs::read(storage).unwrap()).unwrap();
        assert_eq!(rewritten["entries"].as_array().unwrap().len(), 1);
        assert_eq!(rewritten["entries"][0]["origin"], "newest");
    }

    #[test]
    fn failed_replacement_preserves_durable_and_live_state() {
        let temp = TestDir::new();
        let storage = temp.storage();
        let first = temp.project("first.kalium").canonicalize().unwrap();
        let second = temp.project("second.kalium").canonicalize().unwrap();
        let mut projects = RecentProjects::empty(storage.clone());
        projects.record(&first).unwrap();
        let previous_entries = projects.entries().to_vec();
        let previous_bytes = fs::read(&storage).unwrap();
        let mut next_entries = vec![second];
        next_entries.extend(previous_entries.iter().cloned());
        next_entries.truncate(MAX_ENTRIES);
        let next_fields = vec![Map::new(); next_entries.len()];

        let result = projects.commit(next_entries, next_fields, |path, entries, fields, root| {
            let bytes = encode_file(entries, fields, root)?;
            persist_bytes_with_replace(path, &bytes, |_, _| {
                Err(std::io::Error::other("injected replacement failure"))
            })
        });
        assert!(result.is_err());
        assert_eq!(projects.entries(), previous_entries);
        assert_eq!(fs::read(&storage).unwrap(), previous_bytes);
    }

    #[test]
    fn backup_copy_reads_at_most_limit_plus_one_and_rejects_growth() {
        let source = vec![0x5a; MAX_FILE_BYTES as usize + 32];
        let mut reader = std::io::Cursor::new(source);
        let mut backup = Vec::new();
        let result = copy_capped(&mut reader, &mut backup, MAX_FILE_BYTES);
        assert!(result.is_err());
        assert_eq!(backup.len(), MAX_FILE_BYTES as usize + 1);
        assert_eq!(reader.position(), MAX_FILE_BYTES + 1);
    }

    #[test]
    fn rejected_oversized_backup_closes_and_removes_staging_file() {
        let temp = TestDir::new();
        let storage = temp.storage();
        let file = File::create(&storage).unwrap();
        file.set_len(MAX_FILE_BYTES + 1).unwrap();
        drop(file);
        let first = temp.project("first.kalium");
        let mut recent = RecentProjects::empty(storage.clone());
        assert!(recent.record(&first).is_err());
        assert!(recent.entries().is_empty());
        assert_eq!(fs::metadata(storage).unwrap().len(), MAX_FILE_BYTES + 1);
        assert!(fs::read_dir(&temp.0).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".kalium-recent-projects-")));
    }

    #[test]
    fn remove_and_clear_persist_before_updating_entries() {
        let temp = TestDir::new();
        let first = temp.project("first.kalium");
        let second = temp.project("second.kalium");
        let storage = temp.storage();
        let mut projects = RecentProjects::empty(storage.clone());
        projects.record(&first).unwrap();
        projects.record(&second).unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        fs::remove_file(&first).unwrap();
        projects.remove(&first).unwrap();
        assert_eq!(projects.entries(), &[second]);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&storage).unwrap()).unwrap()["entries"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        projects.clear().unwrap();
        assert!(projects.entries().is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(storage).unwrap()).unwrap()["entries"],
            serde_json::json!([])
        );
    }

    #[cfg(unix)]
    #[test]
    fn record_rejects_non_utf8_path_without_mutating_state() {
        use std::os::unix::ffi::OsStringExt;
        let temp = TestDir::new();
        let invalid = temp
            .0
            .join(std::ffi::OsString::from_vec(b"bad-\xff.kalium".to_vec()));
        fs::write(&invalid, b"project").unwrap();
        let storage = temp.storage();
        let mut projects = RecentProjects::empty(storage.clone());
        assert!(projects.record(&invalid).is_err());
        assert!(projects.entries().is_empty());
        assert!(!storage.exists());
    }
}
