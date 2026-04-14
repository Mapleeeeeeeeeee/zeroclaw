use std::path::Path;

/// Read a single key from the workspace `.env` file.
///
/// Matches the parsing used by `src/tools/pushover.rs`: ignores `#` comments
/// and blank lines, strips optional `export ` prefix, trims surrounding
/// single/double quotes.
pub(super) fn load_env_value(workspace_dir: &Path, key: &str) -> anyhow::Result<String> {
    let env_path = workspace_dir.join(".env");
    let content = std::fs::read_to_string(&env_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {e}", env_path.display()))?;

    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue; };
        if k.trim().eq_ignore_ascii_case(key) {
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            return Ok(v.trim().to_string());
        }
    }
    Err(anyhow::anyhow!("{key} not found in {}", env_path.display()))
}

#[cfg(test)]
mod tests {
    use super::load_env_value;
    use std::io::Write;

    /// Write `content` to a `.env` file inside `dir` and return the dir path.
    fn write_env(dir: &tempfile::TempDir, content: &str) -> std::path::PathBuf {
        let env_path = dir.path().join(".env");
        let mut f = std::fs::File::create(&env_path).expect("create .env");
        f.write_all(content.as_bytes()).expect("write .env");
        dir.path().to_path_buf()
    }

    #[test]
    fn env_loader_reads_plain_key_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = write_env(&dir, "FOO=bar\n");
        let result = load_env_value(&workspace, "FOO").unwrap();
        assert_eq!(result, "bar");
    }

    #[test]
    fn env_loader_reads_double_quoted_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = write_env(&dir, "FOO=\"bar baz\"\n");
        let result = load_env_value(&workspace, "FOO").unwrap();
        assert_eq!(result, "bar baz");
    }

    #[test]
    fn env_loader_reads_single_quoted_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = write_env(&dir, "FOO='bar baz'\n");
        let result = load_env_value(&workspace, "FOO").unwrap();
        assert_eq!(result, "bar baz");
    }

    #[test]
    fn env_loader_reads_export_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = write_env(&dir, "export FOO=bar\n");
        let result = load_env_value(&workspace, "FOO").unwrap();
        assert_eq!(result, "bar");
    }

    #[test]
    fn env_loader_skips_comment_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = write_env(&dir, "# this is a comment\nFOO=bar\n");
        let result = load_env_value(&workspace, "FOO").unwrap();
        assert_eq!(result, "bar");
    }

    #[test]
    fn env_loader_skips_blank_lines() {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = write_env(&dir, "\n\nFOO=bar\n");
        let result = load_env_value(&workspace, "FOO").unwrap();
        assert_eq!(result, "bar");
    }

    #[test]
    fn env_loader_case_insensitive_key_match() {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = write_env(&dir, "FOO=bar\n");
        // Query with lowercase — implementation uses eq_ignore_ascii_case
        let lower = load_env_value(&workspace, "foo").unwrap();
        assert_eq!(lower, "bar");
        // Query with mixed case
        let mixed = load_env_value(&workspace, "Foo").unwrap();
        assert_eq!(mixed, "bar");
    }

    #[test]
    fn env_loader_returns_err_on_missing_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let workspace = write_env(&dir, "OTHER=1\n");
        let err = load_env_value(&workspace, "FOO").unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "error message should mention 'not found', got: {err}"
        );
    }

    #[test]
    fn env_loader_returns_err_on_missing_file() {
        let dir = tempfile::TempDir::new().unwrap();
        // Do NOT create a .env file — just use the bare temp dir
        let err = load_env_value(dir.path(), "FOO").unwrap_err();
        assert!(
            err.to_string().contains("Failed to read"),
            "error message should mention 'Failed to read', got: {err}"
        );
    }

    #[test]
    fn env_loader_handles_mixed_content() {
        let dir = tempfile::TempDir::new().unwrap();
        let content = "# leading comment\n\nexport SKIP=ignored\nFOO=\"hello world\"\n# trailing comment\n";
        let workspace = write_env(&dir, content);
        let result = load_env_value(&workspace, "FOO").unwrap();
        assert_eq!(result, "hello world");
    }
}
