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
    path::Path,
    sync::{Arc, Mutex},
    time::Instant,
};

use walkdir::DirEntry;

use crate::{analyze::Analyzer, storage::DocumentStorage, utils::CacheConfig};

fn contains_args_gn(entry: &DirEntry) -> bool {
    entry.file_type().is_dir() && entry.path().join("args.gn").exists()
}

pub fn run_bench(workspace_root: &Path) {
    let storage = Arc::new(Mutex::new(DocumentStorage::new()));
    let mut analyzer = Analyzer::new(&storage);
    let cache_config = CacheConfig::new(false);

    let start_time = Instant::now();
    let mut count = 0;

    let walk = walkdir::WalkDir::new(workspace_root)
        .into_iter()
        .filter_entry(|entry| !contains_args_gn(entry));
    for entry in walk {
        let Ok(entry) = entry else { continue };
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".gn") || name.ends_with(".gni"))
        {
            analyzer.analyze_shallow(entry.path(), cache_config).ok();
            count += 1;
            eprint!(".");
        }
    }
    let elapsed = start_time.elapsed();

    eprintln!();
    eprintln!("Processed {} files in {:.1}s", count, elapsed.as_secs_f64());
}
