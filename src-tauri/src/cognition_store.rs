//! Crash-safe persistence for the WorkUnit/Insight cognition ledger.
//!
//! The two collections are stored in one snapshot so a process crash can
//! never expose a new WorkUnit list with an old Insight list (or vice versa).

use std::fmt;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::insight::Insight;
use crate::workunit::WorkUnit;

pub const COGNITION_FILE: &str = "cognition.json";
pub const COGNITION_SCHEMA_VERSION: u32 = 1;
pub const LEGACY_WORK_UNITS_FILE: &str = "work-units.json";
pub const LEGACY_INSIGHTS_FILE: &str = "insights.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CognitionSnapshot {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub project_key: String,
    pub saved_at: i64,
    pub work_units: Vec<WorkUnit>,
    pub insights: Vec<Insight>,
}

impl CognitionSnapshot {
    pub fn new(
        project_key: impl Into<String>,
        work_units: Vec<WorkUnit>,
        insights: Vec<Insight>,
    ) -> Self {
        Self {
            schema_version: COGNITION_SCHEMA_VERSION,
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            project_key: project_key.into(),
            saved_at: chrono::Utc::now().timestamp_millis(),
            work_units,
            insights,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LegacyArtifacts {
    pub work_units: Vec<WorkUnit>,
    pub insights: Vec<Insight>,
    pub work_units_present: bool,
    pub insights_present: bool,
}

impl LegacyArtifacts {
    pub fn is_complete(&self) -> bool {
        self.work_units_present && self.insights_present
    }

    pub fn into_snapshot(self, project_key: impl Into<String>) -> CognitionSnapshot {
        CognitionSnapshot::new(project_key, self.work_units, self.insights)
    }
}

#[derive(Debug)]
pub enum CognitionStoreError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json {
        operation: &'static str,
        path: PathBuf,
        source: serde_json::Error,
    },
    UnsupportedSchema {
        path: PathBuf,
        found: u32,
        supported: u32,
    },
    ProjectKeyMismatch {
        path: PathBuf,
        expected: String,
        found: String,
    },
    /// 原子替换已经完成，但父目录同步失败。文件内容已是新快照，只是
    /// 断电后的目录项耐久性没有得到确认。
    Durability { path: PathBuf, source: io::Error },
}

impl CognitionStoreError {
    fn io(operation: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }

    fn json(operation: &'static str, path: &Path, source: serde_json::Error) -> Self {
        Self::Json {
            operation,
            path: path.to_path_buf(),
            source,
        }
    }
}

impl fmt::Display for CognitionStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{operation} cognition file {} failed: {source}",
                path.display()
            ),
            Self::Json {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "{operation} cognition JSON {} failed: {source}",
                path.display()
            ),
            Self::UnsupportedSchema {
                path,
                found,
                supported,
            } => write!(
                formatter,
                "unsupported cognition schema {found} in {} (supported: {supported})",
                path.display()
            ),
            Self::ProjectKeyMismatch {
                path,
                expected,
                found,
            } => write!(
                formatter,
                "cognition project key mismatch in {} (expected {expected}, found {found})",
                path.display()
            ),
            Self::Durability { path, source } => write!(
                formatter,
                "cognition snapshot {} was replaced, but parent directory sync failed: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CognitionStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            Self::Durability { source, .. } => Some(source),
            Self::UnsupportedSchema { .. } | Self::ProjectKeyMismatch { .. } => None,
        }
    }
}

/// Read and validate the canonical snapshot.
///
/// A missing file is the only condition reported as `Ok(None)`. Malformed
/// JSON, unsupported schemas and I/O failures remain distinguishable errors;
/// callers must not silently turn them into an empty ledger.
pub fn load(
    project_dir: &Path,
    expected_project_key: &str,
) -> Result<Option<CognitionSnapshot>, CognitionStoreError> {
    let path = project_dir.join(COGNITION_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CognitionStoreError::io("read", &path, error)),
    };
    let snapshot: CognitionSnapshot = serde_json::from_slice(&bytes)
        .map_err(|error| CognitionStoreError::json("decode", &path, error))?;
    validate_snapshot(&path, expected_project_key, &snapshot)?;
    Ok(Some(snapshot))
}

/// Read the legacy two-file representation without modifying or deleting it.
///
/// This helper is intended only for migration when `cognition.json` is absent.
/// It recovers one valid side when the other file does not exist and exposes
/// presence flags so the caller can report an incomplete migration.
pub fn load_legacy(project_dir: &Path) -> Result<Option<LegacyArtifacts>, CognitionStoreError> {
    let work_units_path = project_dir.join(LEGACY_WORK_UNITS_FILE);
    let insights_path = project_dir.join(LEGACY_INSIGHTS_FILE);
    let work_units = read_legacy_array::<WorkUnit>(&work_units_path)?;
    let insights = read_legacy_array::<Insight>(&insights_path)?;

    if work_units.is_none() && insights.is_none() {
        return Ok(None);
    }

    Ok(Some(LegacyArtifacts {
        work_units_present: work_units.is_some(),
        insights_present: insights.is_some(),
        work_units: work_units.unwrap_or_default(),
        insights: insights.unwrap_or_default(),
    }))
}

/// Persist one complete snapshot using a same-directory atomic replacement.
pub fn save(
    project_dir: &Path,
    expected_project_key: &str,
    snapshot: &CognitionSnapshot,
) -> Result<PathBuf, CognitionStoreError> {
    fs::create_dir_all(project_dir)
        .map_err(|error| CognitionStoreError::io("create directory for", project_dir, error))?;
    let target_path = project_dir.join(COGNITION_FILE);
    validate_snapshot(&target_path, expected_project_key, snapshot)?;

    // Serialize fully before touching the filesystem. A serialization failure
    // therefore cannot truncate or otherwise affect the last good snapshot.
    let mut payload = serde_json::to_vec_pretty(snapshot)
        .map_err(|error| CognitionStoreError::json("encode", &target_path, error))?;
    payload.push(b'\n');

    let temp_path = project_dir.join(format!(".cognition.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = write_and_replace(&temp_path, &target_path, &payload);
    if result.is_err() {
        // Cleanup is best-effort. The unique temp name is never considered by
        // the loader, so a cleanup failure cannot supersede the real error.
        let _ = fs::remove_file(&temp_path);
    }
    result.map(|()| target_path)
}

fn validate_snapshot(
    path: &Path,
    expected_project_key: &str,
    snapshot: &CognitionSnapshot,
) -> Result<(), CognitionStoreError> {
    if snapshot.schema_version != COGNITION_SCHEMA_VERSION {
        return Err(CognitionStoreError::UnsupportedSchema {
            path: path.to_path_buf(),
            found: snapshot.schema_version,
            supported: COGNITION_SCHEMA_VERSION,
        });
    }
    if snapshot.project_key != expected_project_key {
        return Err(CognitionStoreError::ProjectKeyMismatch {
            path: path.to_path_buf(),
            expected: expected_project_key.to_string(),
            found: snapshot.project_key.clone(),
        });
    }
    Ok(())
}

fn read_legacy_array<T>(path: &Path) -> Result<Option<Vec<T>>, CognitionStoreError>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CognitionStoreError::io("read legacy", path, error)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| CognitionStoreError::json("decode legacy", path, error))
}

fn write_and_replace(
    temp_path: &Path,
    target_path: &Path,
    payload: &[u8],
) -> Result<(), CognitionStoreError> {
    write_and_replace_using(temp_path, target_path, payload, replace_file)
}

fn write_and_replace_using<F>(
    temp_path: &Path,
    target_path: &Path,
    payload: &[u8],
    replace: F,
) -> Result<(), CognitionStoreError>
where
    F: FnOnce(&Path, &Path) -> io::Result<()>,
{
    let mut temp_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .map_err(|error| CognitionStoreError::io("create temporary", temp_path, error))?;
    temp_file
        .write_all(payload)
        .map_err(|error| CognitionStoreError::io("write temporary", temp_path, error))?;
    temp_file
        .flush()
        .map_err(|error| CognitionStoreError::io("flush temporary", temp_path, error))?;
    temp_file
        .sync_all()
        .map_err(|error| CognitionStoreError::io("sync temporary", temp_path, error))?;
    drop(temp_file);

    replace(temp_path, target_path)
        .map_err(|error| CognitionStoreError::io("replace", target_path, error))?;
    sync_parent_directory(target_path).map_err(|source| CognitionStoreError::Durability {
        path: target_path.to_path_buf(),
        source,
    })?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(temp_path: &Path, target_path: &Path) -> io::Result<()> {
    fs::rename(temp_path, target_path)
}

#[cfg(windows)]
fn replace_file(temp_path: &Path, target_path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::thread;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION};
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    let temp_wide = wide(temp_path);
    let target_wide = wide(target_path);
    let mut last_error = None;
    for attempt in 0..4 {
        // 同目录 MoveFileExW 对首次创建与覆盖都是一次 rename。这里故意
        // 不用 ReplaceFileW：它有已记录的失败态会先移走旧目标再报错，
        // 随后的临时文件清理会把仅存的新副本也删掉。
        let succeeded = unsafe {
            MoveFileExW(
                temp_wide.as_ptr(),
                target_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if succeeded != 0 {
            return Ok(());
        }

        let error = io::Error::last_os_error();
        let retryable = matches!(
            error.raw_os_error(),
            Some(code)
                if code == ERROR_SHARING_VIOLATION as i32 || code == ERROR_ACCESS_DENIED as i32
        );
        last_error = Some(error);
        if !retryable || attempt == 3 {
            break;
        }
        thread::sleep(Duration::from_millis(10 * (1 << attempt)));
    }
    Err(last_error.unwrap_or_else(io::Error::last_os_error))
}

#[cfg(unix)]
fn sync_parent_directory(target_path: &Path) -> io::Result<()> {
    let parent = target_path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "target has no parent"))?;
    File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_target_path: &Path) -> io::Result<()> {
    // MoveFileExW(..., MOVEFILE_WRITE_THROUGH) flushes the move on Windows.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insight::{Evidence, EvidenceKind, InsightType, RiskLevel};
    use crate::workunit::{ChangeSet, WorkUnitMode};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "kvba-cognition-store-test-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn work_unit(id: &str) -> WorkUnit {
        WorkUnit {
            id: id.into(),
            project_id: "proj_test".into(),
            session_id: "session".into(),
            goal: "test atomic persistence".into(),
            mode: WorkUnitMode::Control,
            start_at: 1,
            end_at: 2,
            events: vec!["event-1".into()],
            change_set: ChangeSet::default(),
            tests: Vec::new(),
            executions: Vec::new(),
        }
    }

    fn insight(id: &str, work_unit_id: &str) -> Insight {
        Insight {
            id: id.into(),
            work_unit_id: work_unit_id.into(),
            kind: InsightType::Explain,
            conclusion: "snapshot saved".into(),
            evidence: vec![Evidence {
                kind: EvidenceKind::Event,
                reference: "event-1".into(),
                excerpt: "done".into(),
            }],
            importance: "durability".into(),
            suggested_action: "none".into(),
            risk_level: RiskLevel::Info,
            confidence: 1.0,
        }
    }

    fn snapshot(project_key: &str, suffix: &str) -> CognitionSnapshot {
        let work_unit_id = format!("wu_{suffix}");
        CognitionSnapshot::new(
            project_key,
            vec![work_unit(&work_unit_id)],
            vec![insight(&format!("ins_{suffix}"), &work_unit_id)],
        )
    }

    #[test]
    fn missing_snapshot_is_not_an_error() {
        let dir = TestDir::new();
        assert_eq!(load(dir.path(), "project-a").unwrap(), None);
    }

    #[test]
    fn saves_and_loads_one_complete_snapshot() {
        let dir = TestDir::new();
        let expected = snapshot("project-a", "first");
        let path = save(dir.path(), "project-a", &expected).unwrap();
        assert_eq!(path, dir.path().join(COGNITION_FILE));
        assert_eq!(load(dir.path(), "project-a").unwrap(), Some(expected));
    }

    #[test]
    fn overwrites_existing_snapshot_without_mixing_collections() {
        let dir = TestDir::new();
        save(dir.path(), "project-a", &snapshot("project-a", "old")).unwrap();
        let expected = snapshot("project-a", "new");
        save(dir.path(), "project-a", &expected).unwrap();

        let actual = load(dir.path(), "project-a").unwrap().unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual.work_units[0].id, "wu_new");
        assert_eq!(actual.insights[0].work_unit_id, "wu_new");
    }

    #[test]
    fn replacement_failure_keeps_the_previous_snapshot() {
        let dir = TestDir::new();
        let target = dir.path().join(COGNITION_FILE);
        let temp = dir.path().join(".cognition.injected.tmp");
        fs::write(&target, b"previous snapshot").unwrap();

        let error = write_and_replace_using(&temp, &target, b"new snapshot", |_, _| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected replacement failure",
            ))
        })
        .unwrap_err();

        assert!(matches!(error, CognitionStoreError::Io { .. }));
        assert_eq!(fs::read(&target).unwrap(), b"previous snapshot");
        assert_eq!(fs::read(&temp).unwrap(), b"new snapshot");
    }

    #[test]
    fn malformed_json_is_not_treated_as_missing() {
        let dir = TestDir::new();
        fs::write(dir.path().join(COGNITION_FILE), b"{not-json").unwrap();
        assert!(matches!(
            load(dir.path(), "project-a"),
            Err(CognitionStoreError::Json { .. })
        ));
    }

    #[test]
    fn unsupported_schema_is_reported() {
        let dir = TestDir::new();
        let mut future = snapshot("project-a", "future");
        future.schema_version = COGNITION_SCHEMA_VERSION + 1;
        fs::write(
            dir.path().join(COGNITION_FILE),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load(dir.path(), "project-a"),
            Err(CognitionStoreError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn project_key_mismatch_is_reported() {
        let dir = TestDir::new();
        let wrong = snapshot("project-b", "wrong");
        fs::write(
            dir.path().join(COGNITION_FILE),
            serde_json::to_vec(&wrong).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            load(dir.path(), "project-a"),
            Err(CognitionStoreError::ProjectKeyMismatch { .. })
        ));
    }

    #[test]
    fn save_rejects_snapshot_for_a_different_project() {
        let dir = TestDir::new();
        let wrong = snapshot("project-b", "wrong");
        assert!(matches!(
            save(dir.path(), "project-a", &wrong),
            Err(CognitionStoreError::ProjectKeyMismatch { .. })
        ));
        assert!(!dir.path().join(COGNITION_FILE).exists());
    }

    #[test]
    fn reads_complete_legacy_pair_without_deleting_it() {
        let dir = TestDir::new();
        let unit = work_unit("wu_legacy");
        let item = insight("ins_legacy", "wu_legacy");
        let work_units_path = dir.path().join(LEGACY_WORK_UNITS_FILE);
        let insights_path = dir.path().join(LEGACY_INSIGHTS_FILE);
        fs::write(
            &work_units_path,
            serde_json::to_vec(std::slice::from_ref(&unit)).unwrap(),
        )
        .unwrap();
        fs::write(
            &insights_path,
            serde_json::to_vec(std::slice::from_ref(&item)).unwrap(),
        )
        .unwrap();

        let legacy = load_legacy(dir.path()).unwrap().unwrap();
        assert!(legacy.is_complete());
        assert_eq!(legacy.work_units, vec![unit]);
        assert_eq!(legacy.insights, vec![item]);
        assert!(work_units_path.exists());
        assert!(insights_path.exists());
    }

    #[test]
    fn incomplete_legacy_pair_is_explicit() {
        let dir = TestDir::new();
        fs::write(
            dir.path().join(LEGACY_WORK_UNITS_FILE),
            serde_json::to_vec(&[work_unit("wu_only")]).unwrap(),
        )
        .unwrap();

        let legacy = load_legacy(dir.path()).unwrap().unwrap();
        assert!(!legacy.is_complete());
        assert!(legacy.work_units_present);
        assert!(!legacy.insights_present);
        assert!(legacy.insights.is_empty());
    }
}
