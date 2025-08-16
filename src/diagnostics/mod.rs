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

use tower_lsp::lsp_types::Diagnostic;

use crate::{
    analyzer::AnalyzedBlock,
    common::config::Configurations,
    diagnostics::{syntax::collect_syntax_errors, undefined::collect_undefined_identifiers},
};

mod syntax;
mod undefined;

pub fn compute_diagnostics(
    analyzed_root: &AnalyzedBlock,
    config: &Configurations,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    collect_syntax_errors(
        analyzed_root.block,
        analyzed_root.document,
        &mut diagnostics,
    );
    if config.experimental.undefined_variable_analysis {
        collect_undefined_identifiers(analyzed_root, &mut diagnostics);
    }
    diagnostics
}
