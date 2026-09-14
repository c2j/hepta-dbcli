// ─── Audit JSONL writer (issue #57 §3 / D2 / D5 / D7) ───────────────
//
// One line per event, daily rotation, private permissions. Appends are
// synchronous and flushed so a killed process still leaves the event on disk.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub(crate) const AUDIT_FILE_PREFIX: &str = "hepta-dbcli-audit";

#[derive(Debug, Clone)]
pub(crate) struct AuditConfig {
    /// Overrides the default directory (`<data>/hepta-dbcli/audit`).
    pub dir: Option<PathBuf>,
    /// Whether a writer is opened at all. Production always passes `true`
    /// (the ledger is not optional); tests use it to simulate an unavailable
    /// audit directory.
    pub enabled: bool,
    /// Extra `fsync` after each event. Off by default: `flush` already
    /// survives process kill, and fsync only matters for power loss.
    pub fsync: bool,
    /// `--audit-meta`: also record high-noise meta tools (list_tables, ...).
    pub meta: bool,
    /// Keep audit files for this many days (0 = keep forever).
    pub retention_days: u32,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            dir: None,
            enabled: true,
            fsync: false,
            meta: false,
            retention_days: 30,
        }
    }
}

impl AuditConfig {
    pub(crate) fn resolved_dir(&self) -> PathBuf {
        match &self.dir {
            Some(d) => d.clone(),
            None => crate::logger::log_dir().join("audit"),
        }
    }
}

pub(crate) struct AuditWriter {
    dir: PathBuf,
    fsync: bool,
    current: Option<(String, File)>,
}

impl AuditWriter {
    /// Create `dir` (0700) and open today's file (0600) so an unusable audit
    /// path fails loudly at startup rather than on the first query.
    pub(crate) fn open(dir: &Path, fsync: bool) -> std::io::Result<Self> {
        create_dir_all_private(dir)?;
        let mut writer = Self {
            dir: dir.to_path_buf(),
            fsync,
            current: None,
        };
        writer.current_file()?;
        Ok(writer)
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn append(&mut self, line: &str) -> std::io::Result<()> {
        let fsync = self.fsync;
        let file = self.current_file()?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        if fsync {
            file.sync_all()?;
        }
        Ok(())
    }

    /// Reopen when the UTC date rolls over, so events never land in a stale
    /// day file.
    fn current_file(&mut self) -> std::io::Result<&mut File> {
        let date = today_utc();
        let stale = match &self.current {
            Some((open_date, _)) => open_date != &date,
            None => true,
        };
        if stale {
            let path = self.dir.join(file_name(&date));
            let file = open_private(&path)?;
            self.current = Some((date, file));
        }
        match self.current.as_mut() {
            Some((_, file)) => Ok(file),
            None => Err(std::io::Error::other("audit file was not opened")),
        }
    }
}

fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

pub(crate) fn file_name(date: &str) -> String {
    format!("{AUDIT_FILE_PREFIX}.{date}.jsonl")
}

fn open_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn create_dir_all_private(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Delete audit files whose date is older than `retention_days` days.
/// Returns the number of files removed. `retention_days == 0` keeps everything.
pub(crate) fn apply_retention(dir: &Path, retention_days: u32) -> std::io::Result<usize> {
    if retention_days == 0 {
        return Ok(0);
    }
    let today = today_utc();
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(date) = date_from_file_name(&name) {
            if is_expired(&date, &today, retention_days) {
                std::fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Extract the `YYYY-MM-DD` date from a `hepta-dbcli-audit.<date>.jsonl` name.
fn date_from_file_name(name: &str) -> Option<String> {
    let stem = name.strip_prefix(AUDIT_FILE_PREFIX)?.strip_prefix('.')?;
    let stem = stem.strip_suffix(".jsonl")?;
    let bytes = stem.as_bytes();
    if stem.len() == 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        Some(stem.to_string())
    } else {
        None
    }
}

/// A file is expired when its age in days reaches `retention_days`.
fn is_expired(file_date: &str, today: &str, retention_days: u32) -> bool {
    if retention_days == 0 {
        return false;
    }
    let Ok(file) = chrono::NaiveDate::parse_from_str(file_date, "%Y-%m-%d") else {
        return false;
    };
    let Ok(today) = chrono::NaiveDate::parse_from_str(today, "%Y-%m-%d") else {
        return false;
    };
    (today - file).num_days() >= retention_days as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::event::{
        ActionClass, AuditEvent, Channel, ConnectionInfo, Decision, DraftEvent,
    };
    use std::io::Read;

    fn event(seq: u64) -> AuditEvent {
        AuditEvent::stamp(
            DraftEvent::new(
                Channel::Mcp,
                ConnectionInfo::from_url("d", "mysql://h/db", false),
                "execute_query",
                ActionClass::Dql,
                Decision::Allow,
            ),
            "sess",
            seq,
            format!("id{seq}"),
            "2026-09-14T08:12:03.441Z".to_string(),
        )
    }

    fn read_all(dir: &Path) -> String {
        let mut out = String::new();
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let mut f = File::open(path).expect("open");
            f.read_to_string(&mut out).expect("read");
        }
        out
    }

    #[test]
    fn should_name_file_with_current_date_and_not_legacy_prefix() {
        let name = file_name("2026-09-14");
        assert_eq!(name, "hepta-dbcli-audit.2026-09-14.jsonl");
        assert!(!name.contains("polar-mysql"));
    }

    #[test]
    fn should_write_one_json_object_per_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut writer = AuditWriter::open(dir.path(), false).expect("open");
        writer
            .append(&event(1).to_json_line().expect("json"))
            .expect("append");
        writer
            .append(&event(2).to_json_line().expect("json"))
            .expect("append");

        let contents = read_all(dir.path());
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid json line");
            assert_eq!(v["seq"], i as u64 + 1);
        }
    }

    #[test]
    fn should_create_directory_if_missing() {
        let base = tempfile::tempdir().expect("tempdir");
        let nested = base.path().join("a").join("b");
        AuditWriter::open(&nested, false).expect("open creates dir");
        assert!(nested.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn should_use_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let audit_dir = dir.path().join("audit");
        let mut writer = AuditWriter::open(&audit_dir, false).expect("open");
        writer
            .append(&event(1).to_json_line().expect("json"))
            .expect("append");

        let dir_mode = std::fs::metadata(&audit_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "audit dir must be 0700");

        let file = std::fs::read_dir(&audit_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let file_mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "audit file must be 0600");
    }

    #[test]
    fn should_append_without_truncating_existing_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut w = AuditWriter::open(dir.path(), false).expect("open");
            w.append("first").expect("append");
        }
        {
            let mut w = AuditWriter::open(dir.path(), false).expect("open");
            w.append("second").expect("append");
        }
        let contents = read_all(dir.path());
        assert_eq!(contents, "first\nsecond\n");
    }

    fn touch(dir: &Path, date: &str) {
        std::fs::write(dir.join(file_name(date)), "{}").expect("write fixture");
    }

    fn has_file(dir: &Path, date: &str) -> bool {
        dir.join(file_name(date)).exists()
    }

    #[test]
    fn retention_deletes_old_files_and_keeps_recent() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "2020-01-01"); // far in the past
        touch(dir.path(), "2020-06-15"); // far in the past
        let today = today_utc();
        touch(dir.path(), &today);

        let removed = apply_retention(dir.path(), 30).expect("retention");
        assert_eq!(removed, 2);
        assert!(!has_file(dir.path(), "2020-01-01"));
        assert!(!has_file(dir.path(), "2020-06-15"));
        assert!(has_file(dir.path(), &today));
    }

    #[test]
    fn retention_respects_zero_keep_forever() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "2020-01-01");

        let removed = apply_retention(dir.path(), 0).expect("retention");
        assert_eq!(removed, 0);
        assert!(has_file(dir.path(), "2020-01-01"));
    }

    #[test]
    fn retention_ignores_non_audit_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("unrelated.txt"), "x").expect("write");
        touch(dir.path(), "2020-01-01");

        let removed = apply_retention(dir.path(), 30).expect("retention");
        assert_eq!(removed, 1);
        assert!(dir.path().join("unrelated.txt").exists());
    }

    #[test]
    fn is_expired_uses_age_threshold() {
        assert!(!is_expired("2026-09-01", "2026-09-14", 30)); // 13 days old
        assert!(is_expired("2026-08-15", "2026-09-14", 30)); // 30 days old
        assert!(!is_expired("2026-09-14", "2026-09-14", 30)); // today
        assert!(!is_expired("2020-01-01", "2026-09-14", 0)); // keep forever
    }

    #[test]
    fn date_from_file_name_parses_and_rejects_garbage() {
        assert_eq!(
            date_from_file_name("hepta-dbcli-audit.2026-09-14.jsonl").as_deref(),
            Some("2026-09-14")
        );
        assert_eq!(date_from_file_name("hepta-dbcli-audit.garbage.jsonl"), None);
        assert_eq!(date_from_file_name("other.txt"), None);
    }
}
