#![allow(dead_code)]

pub mod backend;
pub mod config;

#[cfg(test)]
mod release_compat_tests {
    use std::path::PathBuf;
    use std::process::Command;

    fn check_script() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scripts/check_max_glibc.py")
    }

    #[test]
    fn check_max_glibc_script_enforces_2_28_policy() {
        let script = check_script();
        assert!(
            script.is_file(),
            "missing glibc policy checker {}",
            script.display()
        );
        let output = Command::new("python3")
            .arg(&script)
            .arg("--self-test")
            .output()
            .unwrap_or_else(|e| panic!("failed to run {}: {e}", script.display()));
        assert!(
            output.status.success(),
            "glibc 2.28 policy self-test failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
