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

#![cfg(test)]

use std::sync::{Arc, Mutex};

use crate::{
    analyze::Analyzer, ast::Statement, storage::DocumentStorage, testutil::testdata,
    util::CacheConfig,
};

#[test]
fn test_analyze_smoke() {
    let storage = Arc::new(Mutex::new(DocumentStorage::new()));
    let mut analyzer = Analyzer::new(&storage);

    let file = analyzer
        .analyze(
            &testdata("workspaces/smoke/BUILD.gn"),
            CacheConfig::new(true),
        )
        .unwrap();

    // No parse error.
    assert!(file
        .ast_root
        .statements
        .iter()
        .all(|s| !matches!(s, Statement::Error(_))));

    // Inspect the top-level scope.
    let scope = file.scope_at(0);
    assert!(scope.get("enable_opt").is_some());
    assert!(scope.get("_lib").is_some());
    assert!(scope.get("is_linux").is_some());
}

#[test]
fn test_analyze_cycles() {
    let cache_config = CacheConfig::new(true);
    let storage = Arc::new(Mutex::new(DocumentStorage::new()));
    let mut analyzer = Analyzer::new(&storage);

    assert!(analyzer
        .analyze(&testdata("workspaces/cycles/ok1.gni"), cache_config)
        .is_ok());
    assert!(analyzer
        .analyze(&testdata("workspaces/cycles/bad1.gni"), cache_config)
        .is_err());
}
