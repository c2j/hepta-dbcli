// ─── delta-diff progress: 断点续传（v2.1 §13.2，JSONL append-only）───────
//
// 每个完成分片追加一行 {"shard":"…","status":"Match|Diff"}；恢复时按行回放
// 成 HashSet，损坏行跳过并告警。完成时由调用方做原子 rename（.done）。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::backend::DbError;

/// 已完成分片记录：(left_count, right_count, diff_count)——恢复时还原统计。
pub(crate) struct CheckpointManager {
    path: PathBuf,
    completed: HashMap<String, (u64, u64, u64)>,
    writer: std::io::BufWriter<std::fs::File>,
    pub(crate) corrupted_lines: usize,
}

impl CheckpointManager {
    /// 打开（或创建）断点文件并回放已完成分片。
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref().to_path_buf();
        let mut completed = HashMap::new();
        let mut corrupted_lines = 0;
        if path.exists() {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| DbError::query(format!("checkpoint read: {e}")))?;
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(v) => {
                        if let Some(id) = v.get("shard").and_then(|s| s.as_str()) {
                            let get = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
                            completed.insert(id.to_string(), (get("lc"), get("rc"), get("dc")));
                        } else {
                            corrupted_lines += 1;
                        }
                    }
                    Err(_) => corrupted_lines += 1,
                }
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| DbError::query(format!("checkpoint open: {e}")))?;
        Ok(Self {
            path,
            completed,
            writer: std::io::BufWriter::new(file),
            corrupted_lines,
        })
    }

    /// 已完成分片及其统计（供恢复时还原报告计数）。
    pub(crate) fn completed(&self, shard_id: &str) -> Option<(u64, u64, u64)> {
        self.completed.get(shard_id).copied()
    }

    /// 追加一条完成记录（同步落盘，保证崩溃后可恢复）。
    pub(crate) fn record(
        &mut self,
        shard_id: &str,
        status: &str,
        left_count: u64,
        right_count: u64,
        diff_count: u64,
    ) -> Result<(), DbError> {
        let line = serde_json::json!({
            "shard": shard_id,
            "status": status,
            "lc": left_count,
            "rc": right_count,
            "dc": diff_count,
        });
        writeln!(self.writer, "{line}")
            .map_err(|e| DbError::query(format!("checkpoint write: {e}")))?;
        self.writer
            .flush()
            .map_err(|e| DbError::query(format!("checkpoint flush: {e}")))?;
        self.completed
            .insert(shard_id.to_string(), (left_count, right_count, diff_count));
        Ok(())
    }

    /// 全部完成后原子 rename 为 <path>.done。
    pub(crate) fn finalize(self) -> Result<(), DbError> {
        drop(self.writer);
        finalize_path(&self.path)
    }
}

/// 完成后将断点文件原子 rename 为 <path>.done（POSIX 允许 rename 打开中的文件）。
pub(crate) fn finalize_path(path: impl AsRef<Path>) -> Result<(), DbError> {
    let path = path.as_ref();
    let mut done = path.to_path_buf();
    done.set_extension("done");
    if done.exists() {
        std::fs::remove_file(&done)
            .map_err(|e| DbError::query(format!("checkpoint finalize: {e}")))?;
    }
    std::fs::rename(path, &done).map_err(|e| DbError::query(format!("checkpoint rename: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_corrupt_tolerance() {
        let dir = std::env::temp_dir().join(format!("ddcp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cp.jsonl");
        std::fs::write(
            &path,
            "{\"shard\":\"0-100\",\"status\":\"Match\"}\nNOT-JSON\n{\"shard\":\"100-200\"}\n",
        )
        .unwrap();

        let mut cp = CheckpointManager::open(&path).unwrap();
        assert!(cp.completed("0-100").is_some());
        assert!(cp.completed("100-200").is_some());
        assert!(cp.completed("200-300").is_none());
        assert_eq!(cp.corrupted_lines, 1);

        cp.record("200-300", "Diff", 10, 20, 3).unwrap();
        assert_eq!(cp.completed("200-300"), Some((10, 20, 3)));

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.lines().count() == 4);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finalize_renames() {
        let dir = std::env::temp_dir().join(format!("ddcpf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cp.jsonl");
        let cp = CheckpointManager::open(&path).unwrap();
        cp.finalize().unwrap();
        assert!(dir.join("cp.done").exists());
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
