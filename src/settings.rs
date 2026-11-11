//! `settings.ini`: `key=value` lines next to the index cache.
//!
//! Small and written rarely (hotkey changes, window hide/quit), so every call
//! just reads or rewrites the whole file.

use std::path::PathBuf;

fn path() -> PathBuf {
    crate::persist::cache_path().with_file_name("settings.ini")
}

pub fn get(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path()).ok()?;
    text.lines()
        .filter_map(|l| l.split_once('='))
        .find(|(k, _)| k.trim() == key)
        .map(|(_, v)| v.trim().to_string())
}

/// Sets one key, keeping the others. Does nothing if the value is unchanged.
pub fn set(key: &str, value: &str) -> std::io::Result<()> {
    let path = path();
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    for line in old.lines() {
        match line.split_once('=') {
            Some((k, v)) if k.trim() == key => {
                if v.trim() == value {
                    return Ok(());
                }
                if !found {
                    lines.push(format!("{key}={value}"));
                    found = true;
                }
            }
            _ if line.trim().is_empty() => {}
            _ => lines.push(line.to_string()),
        }
    }
    if !found {
        lines.push(format!("{key}={value}"));
    }
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("ini.tmp");
    std::fs::write(&tmp, lines.join("\n") + "\n")?;
    std::fs::rename(&tmp, &path)
}
