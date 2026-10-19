//! Bounded, per-user activity history persisted independently of project files.
//!
//! The application supplies the storage path. Loading is deliberately limited
//! before JSON deserialization, and updates become visible in memory only after
//! the replacement file has been written and synced.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const FORMAT_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 256;
const MAX_CATEGORY_BYTES: usize = 48;
const MAX_MESSAGE_BYTES: usize = 768;
const MAX_FILE_BYTES: u64 = 256 * 1024;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityEntry {
    /// Milliseconds since the Unix epoch, saturating only if the clock exceeds
    /// the range representable by `u64`.
    pub timestamp_unix_ms: u64,
    pub level: ActivityLevel,
    pub category: String,
    pub message: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityFile {
    version: u32,
    entries: Vec<ActivityEntry>,
}

/// A bounded activity log backed by one user-local file.
pub struct ActivityLog {
    path: PathBuf,
    entries: Vec<ActivityEntry>, // oldest to newest
}

impl ActivityLog {
    /// Load a version-1 activity log. A missing file is an empty log.
    ///
    /// The file is size-checked from metadata and then read through a capped
    /// reader, so a file replaced or extended between those operations cannot
    /// cause an unbounded allocation or JSON parse.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    entries: Vec::new(),
                });
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
            bail!(
                "activity log {} exceeds the {MAX_FILE_BYTES}-byte limit",
                path.display()
            );
        }

        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {}", path.display()))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            bail!(
                "activity log {} exceeds the {MAX_FILE_BYTES}-byte limit",
                path.display()
            );
        }

        let decoded: ActivityFile = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid activity log {}", path.display()))?;
        if decoded.version != FORMAT_VERSION {
            bail!(
                "unsupported activity log version {} in {}",
                decoded.version,
                path.display()
            );
        }
        validate_entries(&decoded.entries)?;
        Ok(Self {
            path,
            entries: decoded.entries,
        })
    }

    /// Load the log when possible; on read or validation failure start with an
    /// empty in-memory log. The next successful mutation replaces the file.
    pub fn open_or_default(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        Self::load(&path).unwrap_or(Self {
            path,
            entries: Vec::new(),
        })
    }

    /// Iterate from newest to oldest without cloning message strings.
    pub fn entries_newest_first(&self) -> impl ExactSizeIterator<Item = &ActivityEntry> {
        self.entries.iter().rev()
    }

    /// Append an entry, evicting the oldest entry when the fixed cap is full.
    /// Category and message are truncated at UTF-8 boundaries before storage.
    /// If persistence fails, both the current file and this instance's entries
    /// remain unchanged.
    pub fn record(&mut self, level: ActivityLevel, category: &str, message: &str) -> Result<()> {
        let timestamp_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before the Unix epoch")?
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let entry = ActivityEntry {
            timestamp_unix_ms,
            level,
            category: truncate_utf8(category, MAX_CATEGORY_BYTES),
            message: truncate_utf8(message, MAX_MESSAGE_BYTES),
        };
        self.update_with(entry, persist_entries)
    }

    /// Remove all entries. A persistence error leaves the old entries intact.
    pub fn clear(&mut self) -> Result<()> {
        self.replace_entries_with(Vec::new(), persist_entries)
    }

    fn update_with<F>(&mut self, entry: ActivityEntry, persist: F) -> Result<()>
    where
        F: FnOnce(&Path, &[ActivityEntry]) -> Result<()>,
    {
        let mut next = self.entries.clone();
        if next.len() == MAX_ENTRIES {
            next.remove(0);
        }
        next.push(entry);
        self.replace_entries_with(next, persist)
    }

    fn replace_entries_with<F>(&mut self, next: Vec<ActivityEntry>, persist: F) -> Result<()>
    where
        F: FnOnce(&Path, &[ActivityEntry]) -> Result<()>,
    {
        validate_entries(&next)?;
        persist(&self.path, &next)?;
        self.entries = next;
        Ok(())
    }
}

fn validate_entries(entries: &[ActivityEntry]) -> Result<()> {
    if entries.len() > MAX_ENTRIES {
        bail!("activity log has more than {MAX_ENTRIES} entries");
    }
    for entry in entries {
        if entry.category.len() > MAX_CATEGORY_BYTES {
            bail!("activity category exceeds {MAX_CATEGORY_BYTES} UTF-8 bytes");
        }
        if entry.message.len() > MAX_MESSAGE_BYTES {
            bail!("activity message exceeds {MAX_MESSAGE_BYTES} UTF-8 bytes");
        }
    }
    Ok(())
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn persist_entries(path: &Path, entries: &[ActivityEntry]) -> Result<()> {
    persist_entries_with_replace(path, entries, replace_file)
}

fn persist_entries_with_replace<F>(path: &Path, entries: &[ActivityEntry], replace: F) -> Result<()>
where
    F: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    validate_entries(entries)?;
    let bytes = serde_json::to_vec(&ActivityFile {
        version: FORMAT_VERSION,
        entries: entries.to_vec(),
    })?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        bail!("serialized activity log exceeds the {MAX_FILE_BYTES}-byte limit");
    }

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent).with_context(|| {
            format!("cannot create activity log directory {}", parent.display())
        })?;
    }

    let (temp_path, mut file) = create_temp_file(path)?;
    let backup_path = match create_backup(path) {
        Ok(backup) => backup,
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            return Err(error);
        }
    };
    let write_result = (|| -> Result<()> {
        file.write_all(&bytes)
            .with_context(|| format!("cannot write {}", temp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot sync {}", temp_path.display()))?;
        drop(file);
        replace(&temp_path, path)
            .with_context(|| format!("cannot replace activity log {}", path.display()))?;
        if let Some(parent) = parent {
            if let Err(sync_error) = sync_parent(parent) {
                let rollback = match backup_path.as_deref() {
                    Some(backup) => replace_file(backup, path),
                    None => fs::remove_file(path),
                };
                let rollback_sync = sync_parent(parent);
                let rollback_detail = match (rollback, rollback_sync) {
                    (Ok(()), Ok(())) => String::new(),
                    (rollback, rollback_sync) => format!(
                        "; rollback: {}; directory sync: {}",
                        rollback
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| "ok".to_owned()),
                        rollback_sync
                            .err()
                            .map(|error| error.to_string())
                            .unwrap_or_else(|| "ok".to_owned())
                    ),
                };
                anyhow::bail!(
                    "cannot sync activity log directory {} after replacement: {sync_error}{rollback_detail}",
                    parent.display()
                );
            }
        }
        Ok(())
    })();
    if write_result.is_err() {
        let replacement_was_not_applied = temp_path.exists();
        let _ = fs::remove_file(&temp_path);
        if replacement_was_not_applied {
            if let Some(backup) = backup_path.as_deref() {
                let _ = fs::remove_file(backup);
            }
        }
        // If replacement happened and rollback failed, keep the backup inode
        // so the previous log remains recoverable beside the new file.
    } else if let Some(backup) = backup_path.as_deref() {
        // The replacement and its parent directory entry are synced. A stale
        // backup is harmless if cleanup itself is refused by the filesystem.
        let _ = fs::remove_file(backup);
    }
    write_result
}

fn create_backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("activity log path has no file name"))?
        .to_string_lossy();
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.{}.{}.bak",
            std::process::id(),
            sequence
        ));
        match fs::hard_link(path, &candidate) {
            Ok(()) => return Ok(Some(candidate)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot preserve prior activity log {}", path.display())
                });
            }
        }
    }
    bail!("could not allocate a unique activity log backup file")
}

fn create_temp_file(path: &Path) -> Result<(PathBuf, File)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("activity log path has no file name"))?
        .to_string_lossy();
    for _ in 0..32 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            sequence
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot create {}", candidate.display()));
            }
        }
    }
    bail!("could not allocate a unique activity log temporary file")
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
            fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
        }

        let source = wide(source.as_os_str());
        let destination = wide(destination.as_os_str());
        // SAFETY: both pointers reference NUL-terminated buffers for the call.
        let result = unsafe {
            MoveFileExW(
                source.as_ptr(),
                destination.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
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
        path::Path,
        sync::atomic::{AtomicU64, Ordering},
    };

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "kalium-activity-log-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("activity.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn round_trip_reopens_newest_first() {
        let dir = TestDir::new();
        let path = dir.file();
        let mut log = ActivityLog::load(&path).unwrap();
        log.record(ActivityLevel::Info, "Project", "Created")
            .unwrap();
        log.record(ActivityLevel::Warning, "Recovery", "Restore available")
            .unwrap();

        let reopened = ActivityLog::load(&path).unwrap();
        let entries: Vec<_> = reopened.entries_newest_first().collect();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].category, "Recovery");
        assert_eq!(entries[1].category, "Project");
    }

    #[test]
    fn entry_cap_evicts_oldest() {
        let dir = TestDir::new();
        let path = dir.file();
        let mut log = ActivityLog::load(&path).unwrap();
        for index in 0..=MAX_ENTRIES {
            log.record(ActivityLevel::Info, "item", &index.to_string())
                .unwrap();
        }
        assert_eq!(log.entries.len(), MAX_ENTRIES);
        let entries: Vec<_> = log.entries_newest_first().collect();
        assert_eq!(entries[0].message, MAX_ENTRIES.to_string());
        assert_eq!(entries[MAX_ENTRIES - 1].message, "1");
        assert_eq!(ActivityLog::load(path).unwrap().entries.len(), MAX_ENTRIES);
    }

    #[test]
    fn record_truncates_strings_on_utf8_boundaries() {
        let dir = TestDir::new();
        let mut log = ActivityLog::load(dir.file()).unwrap();
        let category = format!("{}é", "a".repeat(MAX_CATEGORY_BYTES - 1));
        let message = format!("{}🖌", "b".repeat(MAX_MESSAGE_BYTES - 1));
        log.record(ActivityLevel::Info, &category, &message)
            .unwrap();
        let entry = log.entries_newest_first().next().unwrap();
        assert_eq!(entry.category.len(), MAX_CATEGORY_BYTES - 1);
        assert_eq!(entry.message.len(), MAX_MESSAGE_BYTES - 1);
        assert!(entry.category.is_char_boundary(entry.category.len()));
        assert!(entry.message.is_char_boundary(entry.message.len()));
    }

    #[test]
    fn rejects_malformed_oversized_and_unsupported_version_files() {
        let dir = TestDir::new();
        let path = dir.file();

        fs::write(&path, b"{").unwrap();
        assert!(ActivityLog::load(&path).is_err());

        fs::write(&path, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert!(ActivityLog::load(&path).is_err());

        fs::write(&path, br#"{"version":99,"entries":[]}"#).unwrap();
        assert!(ActivityLog::load(&path).is_err());

        let invalid_entry = serde_json::json!({
            "version": 1,
            "entries": [{
                "timestamp_unix_ms": 1,
                "level": "info",
                "category": "x".repeat(MAX_CATEGORY_BYTES + 1),
                "message": "message"
            }]
        });
        fs::write(&path, serde_json::to_vec(&invalid_entry).unwrap()).unwrap();
        assert!(ActivityLog::load(&path).is_err());

        let too_many_entries = serde_json::json!({
            "version": 1,
            "entries": (0..=MAX_ENTRIES)
                .map(|index| serde_json::json!({
                    "timestamp_unix_ms": index,
                    "level": "info",
                    "category": "test",
                    "message": "entry"
                }))
                .collect::<Vec<_>>()
        });
        fs::write(&path, serde_json::to_vec(&too_many_entries).unwrap()).unwrap();
        assert!(ActivityLog::load(&path).is_err());
    }

    #[test]
    fn failed_record_or_clear_preserves_file_and_memory() {
        let dir = TestDir::new();
        let path = dir.file();
        let mut log = ActivityLog::load(&path).unwrap();
        log.record(ActivityLevel::Info, "initial", "kept").unwrap();
        let prior_bytes = fs::read(&path).unwrap();
        let prior_entries = log.entries.clone();

        let fail_replace =
            |_: &Path, _: &Path| Err(std::io::Error::other("injected replacement failure"));
        let result = persist_entries_with_replace(&path, &[], fail_replace);
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), prior_bytes);
        assert_eq!(log.entries, prior_entries);

        let result = log.update_with(
            ActivityEntry {
                timestamp_unix_ms: 2,
                level: ActivityLevel::Error,
                category: "failed".into(),
                message: "not committed".into(),
            },
            |_, _| Err(anyhow::anyhow!("injected persistence failure")),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), prior_bytes);
        assert_eq!(log.entries, prior_entries);

        let result = log.replace_entries_with(Vec::new(), |_, _| {
            Err(anyhow::anyhow!("injected clear failure"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), prior_bytes);
        assert_eq!(log.entries, prior_entries);
    }

    #[test]
    fn clear_persists_an_empty_log() {
        let dir = TestDir::new();
        let path = dir.file();
        let mut log = ActivityLog::load(&path).unwrap();
        log.record(ActivityLevel::Info, "edit", "painted").unwrap();
        log.clear().unwrap();
        assert_eq!(log.entries.len(), 0);
        assert_eq!(ActivityLog::load(path).unwrap().entries.len(), 0);
    }
}
