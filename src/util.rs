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

use pest::Span;
use tower_lsp::lsp_types::{Position, Range};

#[derive(Clone)]
pub struct LineIndex<'i> {
    input: &'i str,
    lines: Vec<&'i str>,
}

impl<'i> LineIndex<'i> {
    pub fn new(input: &'i str) -> Self {
        let mut lines: Vec<&str> = input.lines().collect();
        if lines.is_empty() {
            lines.push(input);
        }
        Self { input, lines }
    }

    fn str_offset(&self, s: &str) -> usize {
        // SAFETY: s must be in the same string as input.
        unsafe { s.as_ptr().offset_from(self.input.as_ptr()) as usize }
    }

    pub fn position(&self, offset: usize) -> Position {
        let index = self
            .lines
            .binary_search_by_key(&offset, |line| self.str_offset(line))
            .unwrap_or_else(|index| index - 1);
        let line = self.lines[index];
        let bytes = offset - self.str_offset(line);
        let character = line
            .get(..bytes)
            .map(|s| s.encode_utf16().count())
            .unwrap_or(0);
        Position {
            line: index as u32,
            character: character as u32,
        }
    }

    pub fn range(&self, span: Span) -> Range {
        Range {
            start: self.position(span.start()),
            end: self.position(span.end()),
        }
    }

    pub fn offset(&self, position: Position) -> Option<usize> {
        let line = self.lines.get(position.line as usize)?;
        let mut character = 0;
        for (i, ch) in line.char_indices() {
            if character >= position.character as usize {
                return Some(self.str_offset(line) + i);
            }
            let mut buf = [0; 2];
            character += ch.encode_utf16(&mut buf).len();
        }
        if character >= position.character as usize {
            Some(self.str_offset(line) + line.len())
        } else {
            None
        }
    }
}

pub fn parse_simple_literal(s: &str) -> Option<&str> {
    if s.contains(['\\', '$']) {
        None
    } else {
        Some(s)
    }
}
