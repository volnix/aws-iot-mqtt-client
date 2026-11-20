// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Platform directories, resolved with the standard library only.

use std::path::PathBuf;

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// The user's home directory (`HOME`, or `USERPROFILE` on Windows).
pub fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        env_path("USERPROFILE").or_else(|| env_path("HOME"))
    } else {
        env_path("HOME")
    }
}

/// The per-user cache directory:
/// macOS `~/Library/Caches`, Windows `%LOCALAPPDATA%`, otherwise `$XDG_CACHE_HOME` or `~/.cache`.
pub fn cache_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        home_dir().map(|h| h.join("Library").join("Caches"))
    } else if cfg!(windows) {
        env_path("LOCALAPPDATA")
    } else {
        env_path("XDG_CACHE_HOME")
            .filter(|p| p.is_absolute())
            .or_else(|| home_dir().map(|h| h.join(".cache")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_dir_is_absolute_when_home_is_set() {
        if home_dir().is_some() {
            assert!(cache_dir().is_some_and(|p| p.is_absolute()));
        }
    }
}
