// ─── delta-diff: export/checkpoint path confinement ───────────────────
//
// MCP `delta_diff` lets an LLM client pick `export`/`checkpoint` paths.
// `resolve_output_path` confines those writes to a configured root so a
// caller cannot clobber arbitrary files (`out.sh`, `~/.bashrc`, ...).

use std::path::{Path, PathBuf};

/// Resolve a caller-supplied export/checkpoint path, confining it to `root`.
///
/// `root` is created when missing and canonicalized. Relative `requested`
/// paths are joined to the canonical root; absolute paths are used as-is. The
/// nearest existing ancestor of the result is canonicalized and must live
/// inside the canonical root, otherwise the path is rejected (this catches
/// `..` traversal and symlinks pointing outside the root). The final parent
/// directory is created so the subsequent write can succeed.
pub(crate) fn resolve_output_path(requested: &Path, root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root)
        .map_err(|e| format!("failed to create export root {}: {}", root.display(), e))?;
    let canonical_root = std::fs::canonicalize(root).map_err(|e| {
        format!(
            "failed to canonicalize export root {}: {}",
            root.display(),
            e
        )
    })?;

    let resolved = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        canonical_root.join(requested)
    };

    // Walk up to the nearest existing ancestor so a not-yet-created leaf (or
    // parent) can still be validated after symlink resolution.
    let mut ancestor = resolved.as_path();
    let existing = loop {
        if ancestor.exists() {
            break ancestor.to_path_buf();
        }
        match ancestor.parent() {
            Some(parent) => ancestor = parent,
            None => break resolved.clone(),
        }
    };
    let canonical_ancestor = std::fs::canonicalize(&existing)
        .map_err(|e| format!("failed to canonicalize {}: {}", existing.display(), e))?;

    if !canonical_ancestor.starts_with(&canonical_root) {
        return Err(format!(
            "export path escapes the allowed root {}: {}",
            canonical_root.display(),
            resolved.display()
        ));
    }

    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create directory {}: {}", parent.display(), e))?;
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::resolve_output_path;
    use std::path::{Path, PathBuf};

    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn relative_path_resolves_under_root_and_creates_parent() {
        let root = temp_root();
        let resolved = resolve_output_path(Path::new("nested/out.csv"), root.path()).unwrap();

        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        assert!(
            resolved.starts_with(&canonical_root),
            "resolved {resolved:?} should be under {canonical_root:?}"
        );
        assert_eq!(resolved, canonical_root.join("nested/out.csv"));
        assert!(
            resolved.parent().unwrap().is_dir(),
            "parent directory should be created for the later write"
        );
    }

    #[test]
    fn absolute_path_inside_root_is_accepted() {
        let root = temp_root();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let requested = canonical_root.join("direct.jsonl");

        let resolved = resolve_output_path(&requested, root.path()).unwrap();
        assert_eq!(resolved, requested);
    }

    #[test]
    fn absolute_path_outside_root_is_rejected() {
        let root = temp_root();
        let outside = temp_root();
        let requested = outside.path().join("evil.sh");

        let err = resolve_output_path(&requested, root.path()).unwrap_err();
        assert!(
            err.contains("export path escapes the allowed root"),
            "error should name the confinement rule, got: {err}"
        );
        assert!(
            !outside.path().join("evil.sh").exists(),
            "nothing may be created outside the root"
        );
    }

    #[test]
    fn dot_dot_traversal_escaping_root_is_rejected() {
        let root = temp_root();
        let requested = root.path().join("../escape.csv");

        let err = resolve_output_path(&requested, root.path()).unwrap_err();
        assert!(
            err.contains("export path escapes the allowed root"),
            "error should name the confinement rule, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_root_pointing_outside_is_rejected() {
        let root = temp_root();
        let outside = temp_root();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();

        let requested: PathBuf = link.join("escaped.csv");
        let err = resolve_output_path(&requested, root.path()).unwrap_err();
        assert!(
            err.contains("export path escapes the allowed root"),
            "symlink escape should be rejected, got: {err}"
        );
    }

    #[test]
    fn missing_root_is_created() {
        let base = temp_root();
        let root = base.path().join("deep/export-root");
        assert!(!root.exists());

        let resolved = resolve_output_path(Path::new("x.csv"), &root).unwrap();
        assert!(root.is_dir(), "root should be created on demand");
        assert!(resolved.starts_with(std::fs::canonicalize(&root).unwrap()));
    }
}
