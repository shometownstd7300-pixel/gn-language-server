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

use tower_lsp::lsp_types::{Position, Range};

pub fn to_lsp_position(ts_position: &tree_sitter::Point) -> Position {
    Position {
        line: ts_position.row as u32,
        character: ts_position.column as u32,
    }
}

pub fn to_lsp_range(ts_range: &tree_sitter::Range) -> Range {
    Range {
        start: to_lsp_position(&ts_range.start_point),
        end: to_lsp_position(&ts_range.end_point),
    }
}

pub fn parse_simple_literal(s: &str) -> Option<&str> {
    if s.contains(['\\', '$']) {
        None
    } else {
        Some(s)
    }
}
