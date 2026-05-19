//! Small path-display helpers shared across CLI, diagnostics and notifications.

use std::path::{Path, PathBuf};

/// Replace a leading `$HOME` component with `~` so paths are shorter for humans and safe
/// to paste in public issues. Falls back to the original path when `$HOME` is unset or
/// the prefix doesn't match.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".to_owned();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Convenience wrapper for callers that already hold a string-like path.
pub fn tilde_str(value: &str) -> String {
    tilde(Path::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_replaces_home_prefix() {
        // Force HOME so the test is hermetic.
        // SAFETY: tests are single-threaded under cargo nextest's per-process model;
        // no other thread reads HOME concurrently.
        unsafe {
            std::env::set_var("HOME", "/home/u");
        }
        assert_eq!(tilde(Path::new("/home/u/.config/x")), "~/.config/x");
        assert_eq!(tilde(Path::new("/home/u")), "~");
        assert_eq!(tilde(Path::new("/etc/passwd")), "/etc/passwd");
    }

    #[test]
    fn tilde_str_delegates() {
        unsafe {
            std::env::set_var("HOME", "/home/u");
        }
        assert_eq!(tilde_str("/home/u/pics/x.png"), "~/pics/x.png");
    }
}
