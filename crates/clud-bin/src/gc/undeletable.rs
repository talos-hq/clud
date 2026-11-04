//! One summary line for paths a gc pass could not delete (#2397).
//!
//! Container builds leave `root:root` trees (47 GB of CEF under one `*-extern`)
//! that the owning user cannot unlink, and a recursive delete over them used to
//! print an error per file. Callers feed every failure into an [`Undeletable`]
//! and print [`Undeletable::summary_line`] once at the end instead.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};

/// Aggregated deletion failures.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Undeletable {
    count: usize,
    denied: usize,
    first: Option<PathBuf>,
    /// Owner uid of the first denied path, where the host reports one.
    first_owner: Option<u32>,
    first_other_error: Option<String>,
}

impl Undeletable {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// Record one failed removal.
    pub fn record(&mut self, path: &Path, error: &io::Error) {
        self.count += 1;
        if error.kind() == io::ErrorKind::PermissionDenied {
            self.denied += 1;
            if self.first_owner.is_none() {
                self.first_owner = std::fs::symlink_metadata(path)
                    .ok()
                    .and_then(|meta| crate::platform::rm_tool::file_owner(&meta));
            }
        } else if self.first_other_error.is_none() {
            self.first_other_error = Some(format!("{}: {error}", path.display()));
        }
        if self.first.is_none() {
            self.first = Some(path.to_path_buf());
        }
    }

    /// Fold `other` into `self` (the first example stays the earliest).
    pub fn merge(&mut self, other: &Undeletable) {
        if self.first.is_none() {
            self.first = other.first.clone();
            self.first_owner = other.first_owner;
        }
        if self.first_other_error.is_none() {
            self.first_other_error = other.first_other_error.clone();
        }
        self.count += other.count;
        self.denied += other.denied;
    }

    /// `N paths could not be removed (permission denied), e.g. <path>; owned by
    /// root — restore ownership, then retry clud gc`, or `None` when nothing failed.
    pub fn summary_line(&self) -> Option<String> {
        let first = self.first.as_ref()?;
        let noun = if self.count == 1 { "path" } else { "paths" };
        let mut line = if self.denied == self.count {
            format!(
                "{} {noun} could not be removed (permission denied), e.g. {}",
                self.count,
                first.display()
            )
        } else {
            let others = self.count - self.denied;
            let example = self
                .first_other_error
                .clone()
                .unwrap_or_else(|| first.display().to_string());
            format!(
                "{} {noun} could not be removed ({} permission denied, {others} other), e.g. {example}",
                self.count, self.denied
            )
        };
        match self.first_owner {
            Some(0) => {
                line.push_str("; owned by root \u{2014} restore ownership, then retry clud gc")
            }
            Some(uid) => {
                let _ = write!(line, "; owned by uid {uid}");
            }
            None => {}
        }
        Some(line)
    }
}

#[cfg(test)]
#[path = "undeletable_tests.rs"]
mod tests;
