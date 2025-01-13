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

use either::Either;
use itertools::Itertools;
use tower_lsp::lsp_types::{DocumentSymbol, Location, SymbolKind, Url};

use crate::{
    ast::{Node, Statement},
    builtins::is_no_target_call,
    parse::ParsedFile,
    util::{parse_simple_literal, LineIndex},
};

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

pub struct Template {
    pub name: String,
    pub comments: Option<String>,
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
    pub name: String,
    pub header: Location,
}

pub fn analyze_templates(file: &ParsedFile) -> Vec<Template> {
    file.root
        .top_level_calls()
        .filter_map(|call| {
            if call.function.name != "template" {
                return None;
            }
            if call.block.is_none() {
                return None;
            }
            let name = call.args.iter().exactly_one().ok()?.as_primary_string()?;
            Some(Template {
                name: parse_simple_literal(&name.raw_value)?.to_string(),
                comments: call.comments.as_ref().map(|comments| comments.text.clone()),
                header: Location {
                    uri: Url::from_file_path(&file.document.path).unwrap(),
                    range: file.document.line_index.range(name.span()),
                },
                block: Location {
                    uri: Url::from_file_path(&file.document.path).unwrap(),
                    range: file.document.line_index.range(call.span),
                },
            })
        })
        .collect()
}

pub fn analyze_links(file: &ParsedFile) -> Vec<Link> {
    file.root
        .strings()
        .filter_map(|string| {
            let content = parse_simple_literal(&string.raw_value)?;
            if !content.contains(":") && content.contains(".") {
                let path = file
                    .workspace
                    .resolve_path(content, file.document.path.parent().unwrap());
                if let Ok(true) = path.try_exists() {
                    return Some(Link::File {
                        uri: Url::from_file_path(&path).unwrap(),
                        location: Location {
                            uri: Url::from_file_path(&file.document.path).unwrap(),
                            range: file.document.line_index.range(string.span()),
                        },
                    });
                }
            } else if let Some((build_gn_path, name)) = resolve_label(content, file) {
                return Some(Link::Target {
                    uri: Url::from_file_path(&build_gn_path).unwrap(),
                    name: name.to_string(),
                    location: Location {
                        uri: Url::from_file_path(&file.document.path).unwrap(),
                        range: file.document.line_index.range(string.span()),
                    },
                });
            }
            None
        })
        .collect()
}

pub fn analyze_targets(file: &ParsedFile) -> Vec<Target> {
    file.root
        .top_level_calls()
        .filter_map(|call| {
            if is_no_target_call(call.function.name) {
                return None;
            }
            if call.block.is_none() {
                return None;
            }
            let name = call.args.iter().exactly_one().ok()?.as_primary_string()?;
            Some(Target {
                name: parse_simple_literal(&name.raw_value)?.to_string(),
                header: Location {
                    uri: Url::from_file_path(&file.document.path).unwrap(),
                    range: file.document.line_index.range(name.span()),
                },
            })
        })
        .collect()
}

#[allow(deprecated)]
fn collect_symbols(node: &dyn Node, line_index: &LineIndex) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();
    if let Some(statement) = node.as_statement() {
        match statement {
            Statement::Assignment(assignment) => {
                symbols.push(DocumentSymbol {
                    name: format!(
                        "{} {} ...",
                        assignment.lvalue.span().as_str(),
                        assignment.op
                    ),
                    detail: None,
                    kind: SymbolKind::VARIABLE,
                    tags: None,
                    deprecated: None,
                    range: line_index.range(assignment.span()),
                    selection_range: line_index.range(assignment.lvalue.span()),
                    children: Some(collect_symbols(assignment.rvalue.as_node(), line_index)),
                });
            }
            Statement::Call(call) => {
                if let Some(block) = &call.block {
                    symbols.push(DocumentSymbol {
                        name: call.function.name.to_string(),
                        detail: None,
                        kind: SymbolKind::FUNCTION,
                        tags: None,
                        deprecated: None,
                        range: line_index.range(call.span()),
                        selection_range: line_index.range(call.function.span()),
                        children: Some(collect_symbols(block.as_node(), line_index)),
                    });
                }
            }
            Statement::Condition(condition) => {
                symbols.push(DocumentSymbol {
                    name: format!("if ({})", condition.condition.span().as_str()),
                    detail: None,
                    kind: SymbolKind::NAMESPACE,
                    tags: None,
                    deprecated: None,
                    range: line_index.range(condition.span()),
                    selection_range: line_index.range(condition.condition.span()),
                    children: Some(collect_symbols(condition.then_block.as_node(), line_index)),
                });
                let mut next_else_block = &condition.else_block;
                loop {
                    match next_else_block {
                        None => break,
                        Some(Either::Left(condition)) => {
                            symbols.push(DocumentSymbol {
                                name: format!("else if ({})", condition.condition.span().as_str()),
                                detail: None,
                                kind: SymbolKind::NAMESPACE,
                                tags: None,
                                deprecated: None,
                                range: line_index.range(condition.span()),
                                selection_range: line_index.range(condition.condition.span()),
                                children: Some(collect_symbols(
                                    condition.then_block.as_node(),
                                    line_index,
                                )),
                            });
                            next_else_block = &condition.else_block;
                        }
                        Some(Either::Right(block)) => {
                            symbols.push(DocumentSymbol {
                                name: "else".to_string(),
                                detail: None,
                                kind: SymbolKind::NAMESPACE,
                                tags: None,
                                deprecated: None,
                                range: line_index.range(block.span()),
                                selection_range: line_index.range(block.span()),
                                children: Some(collect_symbols(block.as_node(), line_index)),
                            });
                            break;
                        }
                    }
                }
            }
            Statement::Unknown(_) => {}
            Statement::UnmatchedBrace(_) => {}
        }
    } else {
        for child in node.children() {
            symbols.extend(collect_symbols(child, line_index));
        }
    }
    symbols
}

pub fn analyze_symbols(file: &ParsedFile) -> Vec<DocumentSymbol> {
    collect_symbols(file.root.as_node(), &file.document.line_index)
}
