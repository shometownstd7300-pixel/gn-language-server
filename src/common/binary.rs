// Copyright 2025 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::path::{Path, PathBuf};

const BINARY_NAME: &str = if cfg!(target_os = "windows") {
    "gn.exe"
} else {
    "gn"
};

const WELLKNOWN_PREBUILT_DIRS: [&str; 2] = [
    // Chromium
    if cfg!(target_os = "windows") {
        "buildtools/win"
    } else if cfg!(target_os = "macos") {
        "buildtools/mac"
    } else {
        "buildtools/linux64"
    },
    // Fuchsia
    "prebuilt/third_party/gn/linux-x64",
];

pub fn find_gn_binary(root_dir: Option<&Path>) -> Option<PathBuf> {
    // Find a prebuilt binary in the source tree.
    let root_dir = root_dir?;

    for prebuilt_dir in WELLKNOWN_PREBUILT_DIRS {
        let binary_path = root_dir.join(prebuilt_dir).join(BINARY_NAME);
        if binary_path.exists() {
            return Some(binary_path);
        }
    }

    // Find a binary in $PATH.
    if let Ok(path) = which::which(BINARY_NAME) {
        return Some(path);
    }

    None
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod tests {
    use super::*;

    use std::{fs::Permissions, os::unix::fs::PermissionsExt};

    #[test]
    fn test_find_gn_binary_chromium_prebuilt() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root_dir = temp_dir.path();

        // Create a fake prebuilt.
        std::fs::create_dir_all(root_dir.join("buildtools/linux64")).unwrap();
        std::fs::write(
            root_dir.join("buildtools/linux64/gn"),
            b"#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(
            root_dir.join("buildtools/linux64/gn"),
            Permissions::from_mode(0o755),
        )
        .unwrap();

        let gn_binary = find_gn_binary(Some(root_dir));
        assert_eq!(gn_binary, Some(root_dir.join("buildtools/linux64/gn")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_find_gn_binary_fuchsia_prebuilt() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root_dir = temp_dir.path();

        // Create a fake prebuilt.
        std::fs::create_dir_all(root_dir.join("prebuilt/third_party/gn/linux-x64")).unwrap();
        std::fs::write(
            root_dir.join("prebuilt/third_party/gn/linux-x64/gn"),
            b"#!/bin/sh\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(
            root_dir.join("prebuilt/third_party/gn/linux-x64/gn"),
            Permissions::from_mode(0o755),
        )
        .unwrap();

        let gn_binary = find_gn_binary(Some(root_dir));
        assert_eq!(
            gn_binary,
            Some(root_dir.join("prebuilt/third_party/gn/linux-x64/gn"))
        );
    }
}
