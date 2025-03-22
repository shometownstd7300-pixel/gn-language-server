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

pub fn find_gn_binary(root_dir: Option<&Path>) -> Option<PathBuf> {
    let binary_name = if cfg!(target_os = "windows") {
        "gn.exe"
    } else {
        "gn"
    };

    // Find the binary in $PATH.
    if let Ok(path) = which::which("gn") {
        return Some(path);
    }

    // Find the prebuilt binary in the source tree.
    let root_dir = root_dir?;

    let prebuilt_dir = if cfg!(target_os = "windows") {
        "buildtools/win"
    } else if cfg!(target_os = "macos") {
        "buildtools/mac"
    } else {
        "buildtools/linux64"
    };
    let binary_path = root_dir.join(prebuilt_dir).join(binary_name);
    binary_path.exists().then_some(binary_path)
}
