//! Directory resolution for entrypoint files and their local dependencies.

use std::path::Path;

/// Directory containing a source file, relative to the current directory.
pub fn source_file_directory(file: &str) -> &Path {
    Path::new(file)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_filename_uses_current_directory() {
        assert_eq!(source_file_directory("counter.mkt"), Path::new("."));
    }

    #[test]
    fn explicit_parent_is_preserved() {
        assert_eq!(
            source_file_directory("smoke/counter.mkt"),
            Path::new("smoke")
        );
        assert_eq!(source_file_directory("./counter.mkt"), Path::new("."));
    }
}
