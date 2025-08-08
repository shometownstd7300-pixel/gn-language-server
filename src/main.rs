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

use std::path::Path;

use crate::bench::run_bench;

mod analyze;
mod ast;
mod bench;
mod binary;
mod builtins;
mod client;
mod config;
mod error;
mod indexing;
mod providers;
mod server;
mod storage;
mod testutils;
mod utils;

#[tokio::main]
async fn main() {
    if let Ok(path) = std::env::var("GN_BENCH") {
        run_bench(Path::new(&path)).await;
        return;
    }
    server::run().await;
}
