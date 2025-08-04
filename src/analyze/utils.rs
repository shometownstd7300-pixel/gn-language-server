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

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::storage::DocumentVersion;

pub fn resolve_path(name: &str, root_dir: &Path, current_dir: &Path) -> PathBuf {
    if let Some(rest) = name.strip_prefix("//") {
        root_dir.join(rest)
    } else {
        current_dir.join(name)
    }
}

const CHECK_INTERVAL: Duration = Duration::from_secs(5);

pub fn compute_next_check(t: Instant, version: DocumentVersion) -> Instant {
    match version {
        DocumentVersion::OnDisk { .. } => t + CHECK_INTERVAL,
        // Do not skip version checks for in-memory documents.
        _ => t,
    }
}
