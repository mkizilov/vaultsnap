//! Include and exclude patterns.
//!
//! Patterns are globs matched against the path *relative to the scan root*, with `/` as the
//! separator on every platform:
//!
//! * `*` and `?` match within one path component (never `/`); `**` matches across components;
//!   `[abc]`, `[a-z]`, `[!x]`, and `{one,two}` work as in shell globs. A leading dot is not
//!   special: `*` matches `.hidden`. Matching is case sensitive.
//! * A pattern with no `/` matches a name at any depth (`*.log` matches `a.log` and `d/a.log`).
//! * A pattern with a `/` in the middle or a leading `/` is anchored at the root (`src/*.o`
//!   matches `src/x.o` only, `/build` matches only the top-level `build`). The leading `/` is
//!   not part of the match.
//! * A trailing `/` restricts an exclude pattern to directories (`cache/`). Include patterns
//!   apply to non-directories only and cannot end with `/`.
//! * On Unix a backslash escapes the next character (`\[x\]` matches the literal name `[x]`).

use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};

use crate::error::{Error, Result};

#[derive(Debug)]
struct Pattern {
    matcher: GlobMatcher,
    directories_only: bool,
}

impl Pattern {
    fn compile(pattern: &str, allow_directories_only: bool) -> Result<Self> {
        let invalid = |reason: &str| Error::InvalidPattern {
            pattern: pattern.to_owned(),
            reason: reason.to_owned(),
        };
        let mut text = pattern;
        let directories_only = text.ends_with('/');
        if directories_only {
            if !allow_directories_only {
                return Err(invalid("include patterns cannot end with `/`; use `dir/**`"));
            }
            text = &text[..text.len() - 1];
        }
        let leading_slash = text.starts_with('/');
        if leading_slash {
            text = &text[1..];
        }
        if text.is_empty() {
            return Err(invalid("the pattern is empty"));
        }
        let anchored = leading_slash || text.contains('/');
        let glob = if anchored {
            text.to_owned()
        } else {
            format!("**/{text}")
        };
        let matcher = GlobBuilder::new(&glob)
            .literal_separator(true)
            .build()
            .map_err(|err| invalid(&err.to_string()))?
            .compile_matcher();
        Ok(Self {
            matcher,
            directories_only,
        })
    }

    fn matches(&self, path: &Path, is_directory: bool) -> bool {
        (!self.directories_only || is_directory) && self.matcher.is_match(path)
    }
}

/// Compiled include and exclude patterns.
#[derive(Debug)]
pub(super) struct Filter {
    includes: Vec<Pattern>,
    excludes: Vec<Pattern>,
}

impl Filter {
    pub(super) fn new(includes: &[String], excludes: &[String]) -> Result<Self> {
        let compile_all = |patterns: &[String], allow_directories_only: bool| {
            patterns
                .iter()
                .map(|pattern| Pattern::compile(pattern, allow_directories_only))
                .collect::<Result<Vec<_>>>()
        };
        Ok(Self {
            includes: compile_all(includes, false)?,
            excludes: compile_all(excludes, true)?,
        })
    }

    /// Whether `path` (relative to the root) is excluded.
    pub(super) fn is_excluded(&self, path: &Path, is_directory: bool) -> bool {
        self.excludes
            .iter()
            .any(|pattern| pattern.matches(path, is_directory))
    }

    /// Whether a non-directory at `path` passes the include patterns (always true if there are
    /// none).
    pub(super) fn is_included(&self, path: &Path) -> bool {
        self.includes.is_empty()
            || self
                .includes
                .iter()
                .any(|pattern| pattern.matches(path, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exclude(pattern: &str) -> Filter {
        Filter::new(&[], &[pattern.to_owned()]).expect("valid pattern")
    }

    fn excluded(pattern: &str, path: &str, is_directory: bool) -> bool {
        exclude(pattern).is_excluded(Path::new(path), is_directory)
    }

    #[test]
    fn a_pattern_without_a_slash_matches_names_at_any_depth() {
        assert!(excluded("*.log", "a.log", false));
        assert!(excluded("*.log", "d/e/a.log", false));
        assert!(excluded("*.log", ".hidden.log", false));
        assert!(!excluded("*.log", "a.logx", false));
        assert!(!excluded("*.log", "log", false));
        assert!(excluded("node_modules", "node_modules", true));
        assert!(excluded("node_modules", "web/node_modules", true));
        assert!(!excluded("node_modules", "web/node_modules_extra", true));
    }

    #[test]
    fn a_leading_slash_anchors_at_the_root() {
        assert!(excluded("/build", "build", true));
        assert!(!excluded("/build", "src/build", true));
        assert!(excluded("/notes.txt", "notes.txt", false));
        assert!(!excluded("/notes.txt", "d/notes.txt", false));
    }

    #[test]
    fn a_slash_in_the_middle_anchors_too_and_stars_do_not_cross_directories() {
        assert!(excluded("src/*.o", "src/x.o", false));
        assert!(!excluded("src/*.o", "src/a/x.o", false));
        assert!(!excluded("src/*.o", "x.o", false));
        assert!(!excluded("src/*.o", "lib/src/x.o", false));
        assert!(excluded("src/**", "src/a/b/x.o", false));
        assert!(excluded("**/gen/*.rs", "a/b/gen/x.rs", false));
        assert!(excluded("**/gen/*.rs", "gen/x.rs", false));
    }

    #[test]
    fn a_trailing_slash_matches_directories_only() {
        assert!(excluded("cache/", "cache", true));
        assert!(excluded("cache/", "a/cache", true));
        assert!(!excluded("cache/", "cache", false));
        assert!(!excluded("cache/", "a/cache", false));
    }

    #[test]
    fn character_classes_wildcards_and_alternation_work() {
        assert!(excluded("file?.txt", "file1.txt", false));
        assert!(!excluded("file?.txt", "file12.txt", false));
        assert!(excluded("[ab].txt", "b.txt", false));
        assert!(!excluded("[ab].txt", "c.txt", false));
        assert!(excluded("*.{jpg,png}", "x.png", false));
        assert!(!excluded("*.{jpg,png}", "x.gif", false));
    }

    #[test]
    fn matching_is_case_sensitive() {
        assert!(!excluded("*.LOG", "a.log", false));
    }

    #[test]
    fn includes_apply_to_files_and_default_to_everything() {
        let none = Filter::new(&[], &[]).expect("valid");
        assert!(none.is_included(Path::new("anything/at/all")));

        let some = Filter::new(&["*.rs".to_owned(), "docs/**".to_owned()], &[]).expect("valid");
        assert!(some.is_included(Path::new("src/main.rs")));
        assert!(some.is_included(Path::new("docs/guide/a.md")));
        assert!(!some.is_included(Path::new("notes.txt")));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        for pattern in ["", "/", "//", "[", "a[", "[z-a]"] {
            let result = Filter::new(&[], &[pattern.to_owned()]);
            assert!(matches!(result, Err(Error::InvalidPattern { .. })), "{pattern:?}");
        }
        let include_dir = Filter::new(&["src/".to_owned()], &[]);
        assert!(matches!(include_dir, Err(Error::InvalidPattern { .. })));
    }

    #[cfg(unix)]
    #[test]
    fn a_backslash_escapes_special_characters_on_unix() {
        assert!(excluded(r"\[x\]", "[x]", false));
        assert!(!excluded(r"\[x\]", "x", false));
        assert!(excluded(r"a\*b", "a*b", false));
        assert!(!excluded(r"a\*b", "aXb", false));
    }
}
