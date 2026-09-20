use anyhow::Result;
use serde::Serialize;
use std::{fs, path::Path};

/// Single-quote `value` for a POSIX shell.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Write `value` as pretty JSON, replacing `path` only once it is complete.
///
/// The temporary name carries this process's pid: several tmux hooks can save
/// concurrently, and a shared name would let them race on the same file.
pub fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_embedded_single_quotes() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r#"'it'\''s'"#);
    }

    #[test]
    fn atomic_json_leaves_no_temporary_behind() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("state.json");
        atomic_json(&path, &serde_json::json!({"a": 1})).unwrap();
        assert_eq!(
            fs::read_to_string(&path).unwrap().trim(),
            "{\n  \"a\": 1\n}"
        );
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 1);
    }
}
