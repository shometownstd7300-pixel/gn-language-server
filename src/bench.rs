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

use crate::{
    analyze::Analyzer,
    storage::DocumentStorage,
    utils::{find_gn_files, CacheConfig},
};

pub fn run_bench(workspace_root: &Path) {
    let storage = Arc::new(Mutex::new(DocumentStorage::new()));
    let mut analyzer = Analyzer::new(&storage);
    let cache_config = CacheConfig::new(false);

    let start_time = Instant::now();
    let mut count = 0;

    for path in find_gn_files(workspace_root) {
        analyzer.analyze_shallow(&path, cache_config).ok();
        count += 1;
        eprint!(".");
    }
    let elapsed = start_time.elapsed();

    eprintln!();
    eprintln!("Processed {} files in {:.1}s", count, elapsed.as_secs_f64());
}
