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

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use crate::{
    analyzer::Analyzer,
    common::{storage::DocumentStorage, testutils::testdata},
    parser::Statement,
};

#[test]
fn test_analyze_smoke() {
    let storage = Arc::new(Mutex::new(DocumentStorage::new()));
    let analyzer = Analyzer::new(&storage);

    let file = analyzer
        .analyze(&testdata("workspaces/smoke/BUILD.gn"), Instant::now())
        .unwrap();

    // No parse error.
    assert!(file
        .ast_root
        .statements
        .iter()
        .all(|s| !matches!(s, Statement::Error(_))));

    // Inspect the top-level variables.
    let variables = file.variables_at(0);
    assert!(variables.get("enable_opt").is_some());
    assert!(variables.get("_lib").is_some());
    assert!(variables.get("is_linux").is_some());
}

#[test]
fn test_analyze_cycles() {
    let request_time = Instant::now();
    let storage = Arc::new(Mutex::new(DocumentStorage::new()));
    let analyzer = Analyzer::new(&storage);

    assert!(analyzer
        .analyze(&testdata("workspaces/cycles/ok1.gni"), request_time)
        .is_ok());
    assert!(analyzer
        .analyze(&testdata("workspaces/cycles/bad1.gni"), request_time)
        .is_ok());
}
