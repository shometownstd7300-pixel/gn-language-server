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

use std::path::PathBuf;

use itertools::Itertools;
use streaming_iterator::StreamingIterator;
use tower_lsp::lsp_types::{Location, Range, Url};
use tree_sitter::{Node, Query, QueryCursor};

use crate::{
    parse::{gn_language, ParsedFile},
    util::{parse_simple_literal, to_lsp_position, to_lsp_range},
};

fn is_template_block_node(node: &Node, source: &[u8]) -> bool {
    if node.kind() != "block" {
        return false;
    }
    let Some(node) = node.parent() else {
        return false;
    };
    if node.kind() != "primary_expression" {
        return false;
    }
    let Some(node) = node.prev_sibling() else {
        return false;
    };
    if node.kind() != "primary_expression" {
        return false;
    }
    let Some(node) = node.child(0) else {
        return false;
    };
    if node.kind() != "call_expression" {
        return false;
    }
    let Some(node) = node.named_child(0) else {
        return false;
    };
    if node.kind() != "identifier" {
        return false;
    }
    node.utf8_text(source).unwrap() == "template"
}

fn is_toplevel_node(node: &Node, source: &[u8]) -> bool {
    let mut current_node = node.parent();
    while let Some(node) = current_node {
        if is_template_block_node(&node, source) {
            return false;
        }
        current_node = node.parent();
    }
    true
}

#[allow(clippy::manual_map)]
fn resolve_label<'s>(label: &'s str, file: &ParsedFile) -> Option<(PathBuf, &'s str)> {
    if let Some((prefix, name)) = label.split_once(':') {
        if prefix.is_empty() {
            Some((file.document.path.clone(), name))
        } else if let Some(rel_dir) = prefix.strip_prefix("//") {
            Some((file.workspace.root.join(rel_dir).join("BUILD.gn"), name))
        } else {
            None
        }
    } else if let Some(rel_dir) = label.strip_prefix("//") {
        if !rel_dir.is_empty() {
            Some((
                file.workspace.root.join(rel_dir).join("BUILD.gn"),
                rel_dir.split('/').last().unwrap(),
            ))
        } else {
            None
        }
    } else {
        None
    }
}

struct Queries {
    pub templates: Query,
    pub strings: Query,
    pub targets: Query,
}

impl Queries {
    pub fn new() -> Self {
        let templates = Query::new(
            gn_language(),
            r#"
            (
                (primary_expression
                    (call_expression
                        function: (identifier) @keyword
                        .
                        (primary_expression (string (string_content) @name))
                        (#eq? @keyword "template"))) @header
                .
                (primary_expression (block)) @block
            )
        "#,
        )
        .unwrap();
        let strings = Query::new(
            gn_language(),
            r#"(string (string_content) @content) @literal"#,
        )
        .unwrap();
        let targets = Query::new(
            gn_language(),
            r#"
            (
                (primary_expression
                    (call_expression
                        function: (identifier) @kind
                        .
                        (primary_expression (string (string_content) @name)))) @header
                .
                (primary_expression (block)) @block
                (#not-eq? @kind "template")
                (#not-eq? @kind "foreach")
                (#not-eq? @kind "set_defaults")
            )
        "#,
        )
        .unwrap();
        Self {
            templates,
            strings,
            targets,
        }
    }
}

pub struct Template {
    pub name: String,
    pub comment: Option<String>,
    pub header: Location,
    pub block: Location,
}

pub enum Link {
    File {
        uri: Url,
        location: Location,
    },
    Target {
        uri: Url,
        name: String,
        location: Location,
    },
}

pub struct Target {
    pub kind: String,
    pub name: String,
    pub header: Location,
    pub block: Location,
}

pub struct Analyzer {
    queries: Queries,
}

impl Analyzer {
    pub fn new() -> Self {
        Self {
            queries: Queries::new(),
        }
    }

    pub fn scan_templates(&self, file: &ParsedFile) -> Vec<Template> {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(
            &self.queries.templates,
            file.tree.root_node(),
            file.document.data.as_slice(),
        );

        let mut templates = Vec::new();
        while let Some(m) = matches.next() {
            let name_node = m.nodes_for_capture_index(1).next().unwrap();
            let header_node = m.nodes_for_capture_index(2).next().unwrap();
            let block_node = m.nodes_for_capture_index(3).next().unwrap();
            let name = name_node.utf8_text(&file.document.data).unwrap();
            let comment = {
                let mut comments = Vec::new();
                let mut current_node = header_node.prev_sibling();
                while let Some(node) = current_node {
                    if node.kind() != "comment" {
                        break;
                    }
                    let mut comment = node.utf8_text(&file.document.data).unwrap();
                    comment = comment.trim_start_matches('#');
                    if let Some(new_comment) = comment.strip_prefix(' ') {
                        comment = new_comment;
                    }
                    comments.push(comment.to_string());
                    current_node = node.prev_sibling();
                }
                if comments.is_empty() {
                    None
                } else {
                    Some(comments.into_iter().rev().join("\n"))
                }
            };
            templates.push(Template {
                name: name.to_string(),
                comment,
                header: Location {
                    uri: Url::from_file_path(&file.document.path).unwrap(),
                    range: to_lsp_range(&header_node.range()),
                },
                block: Location {
                    uri: Url::from_file_path(&file.document.path).unwrap(),
                    range: Range {
                        start: to_lsp_position(&header_node.start_position()),
                        end: to_lsp_position(&block_node.end_position()),
                    },
                },
            });
        }
        templates
    }

    pub fn scan_links(&self, file: &ParsedFile) -> Vec<Link> {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(
            &self.queries.strings,
            file.tree.root_node(),
            file.document.data.as_slice(),
        );

        let mut links = Vec::new();
        while let Some(m) = matches.next() {
            let content_node = m.nodes_for_capture_index(0).next().unwrap();
            let literal_node = m.nodes_for_capture_index(1).next().unwrap();

            if let Some(content) =
                parse_simple_literal(content_node.utf8_text(&file.document.data).unwrap())
            {
                if !content.contains(":") && content.contains(".") {
                    let path = file
                        .workspace
                        .resolve_path(content, file.document.path.parent().unwrap());
                    if let Ok(true) = path.try_exists() {
                        links.push(Link::File {
                            uri: Url::from_file_path(&path).unwrap(),
                            location: Location {
                                uri: Url::from_file_path(&file.document.path).unwrap(),
                                range: to_lsp_range(&literal_node.range()),
                            },
                        });
                    }
                } else if let Some((build_gn_path, name)) = resolve_label(content, file) {
                    links.push(Link::Target {
                        uri: Url::from_file_path(&build_gn_path).unwrap(),
                        name: name.to_string(),
                        location: Location {
                            uri: Url::from_file_path(&file.document.path).unwrap(),
                            range: to_lsp_range(&literal_node.range()),
                        },
                    });
                }
            }
        }
        links
    }

    pub fn scan_targets(&self, file: &ParsedFile) -> Vec<Target> {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(
            &self.queries.targets,
            file.tree.root_node(),
            file.document.data.as_slice(),
        );

        let mut targets = Vec::new();
        while let Some(m) = matches.next() {
            let kind_node = m.nodes_for_capture_index(0).next().unwrap();
            let name_node = m.nodes_for_capture_index(1).next().unwrap();
            let header_node = m.nodes_for_capture_index(2).next().unwrap();
            let block_node = m.nodes_for_capture_index(3).next().unwrap();
            if let Some(name) =
                parse_simple_literal(name_node.utf8_text(&file.document.data).unwrap())
            {
                if is_toplevel_node(&header_node, &file.document.data) {
                    targets.push(Target {
                        kind: kind_node
                            .utf8_text(&file.document.data)
                            .unwrap()
                            .to_string(),
                        name: name.to_string(),
                        header: Location {
                            uri: Url::from_file_path(&file.document.path).unwrap(),
                            range: to_lsp_range(&header_node.range()),
                        },
                        block: Location {
                            uri: Url::from_file_path(&file.document.path).unwrap(),
                            range: Range {
                                start: to_lsp_position(&header_node.start_position()),
                                end: to_lsp_position(&block_node.end_position()),
                            },
                        },
                    });
                }
            }
        }
        targets
    }
}
