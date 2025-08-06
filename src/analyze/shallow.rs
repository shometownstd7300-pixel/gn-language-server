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
    collections::BTreeMap,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Instant,
};

use either::Either;

use crate::{
    analyze::{
        cache::AnalysisNode,
        data::{
            AnalyzedAssignment, AnalyzedTarget, AnalyzedTemplate, AnalyzedVariable,
            MutableShallowAnalyzedBlock, ShallowAnalyzedBlock, ShallowAnalyzedFile,
            WorkspaceContext,
        },
        links::collect_links,
        AnalyzedLink,
    },
    ast::{parse, Block, Comments, LValue, Node, Statement},
    builtins::{DECLARE_ARGS, FOREACH, FORWARD_VARIABLES_FROM, IMPORT, SET_DEFAULTS, TEMPLATE},
    storage::{Document, DocumentStorage},
    utils::parse_simple_literal,
};

fn is_exported(name: &str) -> bool {
    !name.starts_with("_")
}

pub struct ShallowAnalyzer {
    context: WorkspaceContext,
    storage: Arc<Mutex<DocumentStorage>>,
    cache: BTreeMap<PathBuf, Pin<Arc<ShallowAnalyzedFile>>>,
}

impl ShallowAnalyzer {
    pub fn new(context: &WorkspaceContext, storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            context: context.clone(),
            storage: storage.clone(),
            cache: BTreeMap::new(),
        }
    }

    pub fn cached_files(&self) -> Vec<Pin<Arc<ShallowAnalyzedFile>>> {
        self.cache.values().cloned().collect()
    }

    pub fn analyze(&mut self, path: &Path, request_time: Instant) -> Pin<Arc<ShallowAnalyzedFile>> {
        self.analyze_cached(path, request_time, &mut Vec::new())
    }

    fn analyze_cached(
        &mut self,
        path: &Path,
        request_time: Instant,
        visiting: &mut Vec<PathBuf>,
    ) -> Pin<Arc<ShallowAnalyzedFile>> {
        if let Some(cached_file) = self.cache.get(path) {
            if cached_file
                .node
                .verify(request_time, &self.storage.lock().unwrap())
            {
                return cached_file.clone();
            }
        }

        let new_file = self.analyze_uncached(path, request_time, visiting);
        self.cache.insert(path.to_path_buf(), new_file.clone());
        new_file
    }

    fn analyze_uncached(
        &mut self,
        path: &Path,
        request_time: Instant,
        visiting: &mut Vec<PathBuf>,
    ) -> Pin<Arc<ShallowAnalyzedFile>> {
        if visiting.iter().any(|p| p == path) {
            return ShallowAnalyzedFile::error(path, request_time);
        }

        visiting.push(path.to_path_buf());
        let result = self.analyze_uncached_inner(path, request_time, visiting);
        visiting.pop();
        result
    }

    fn analyze_uncached_inner(
        &mut self,
        path: &Path,
        request_time: Instant,
        visiting: &mut Vec<PathBuf>,
    ) -> Pin<Arc<ShallowAnalyzedFile>> {
        let document = self.storage.lock().unwrap().read(path);
        let ast_root = Box::pin(parse(&document.data));
        let mut deps = Vec::new();
        let analyzed_root =
            self.analyze_block(&ast_root, request_time, &document, &mut deps, visiting);

        let links = collect_links(&ast_root, path, &self.context);

        // SAFETY: links' contents are backed by pinned document.
        let links = unsafe { std::mem::transmute::<Vec<AnalyzedLink>, Vec<AnalyzedLink>>(links) };
        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root = unsafe {
            std::mem::transmute::<ShallowAnalyzedBlock, ShallowAnalyzedBlock>(analyzed_root)
        };
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };

        ShallowAnalyzedFile::new(document, ast_root, analyzed_root, links, deps, request_time)
    }

    fn analyze_block<'i, 'p>(
        &mut self,
        block: &'p Block<'i>,
        request_time: Instant,
        document: &'i Document,
        deps: &mut Vec<Arc<AnalysisNode>>,
        visiting: &mut Vec<PathBuf>,
    ) -> ShallowAnalyzedBlock<'i, 'p> {
        let mut analyzed_block = MutableShallowAnalyzedBlock::new_top_level();

        for statement in &block.statements {
            match statement {
                Statement::Assignment(assignment) => {
                    let identifier = match &assignment.lvalue {
                        LValue::Identifier(identifier) => identifier,
                        LValue::ArrayAccess(array_access) => &array_access.array,
                        LValue::ScopeAccess(scope_access) => &scope_access.scope,
                    };
                    if is_exported(identifier.name) {
                        analyzed_block.variables.insert(
                            identifier.name,
                            AnalyzedVariable {
                                assignments: [(
                                    identifier.span,
                                    AnalyzedAssignment {
                                        name: identifier.name,
                                        comments: assignment.comments.clone(),
                                        statement,
                                        document,
                                        variable_span: identifier.span,
                                    },
                                )]
                                .into(),
                            },
                        );
                    }
                }
                Statement::Call(call) => match call.function.name {
                    IMPORT => {
                        if let Some(name) = call
                            .only_arg()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            let path = self
                                .context
                                .resolve_path(name, document.path.parent().unwrap());
                            let file = self.analyze_cached(&path, request_time, visiting);
                            analyzed_block.import(&file.analyzed_root);
                            deps.push(file.node.clone());
                        }
                    }
                    TEMPLATE => {
                        if let Some(name) = call
                            .only_arg()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            if is_exported(name) {
                                analyzed_block.templates.insert(
                                    name,
                                    AnalyzedTemplate {
                                        name,
                                        comments: call.comments.clone(),
                                        document,
                                        header: call.function.span,
                                        span: call.span,
                                    },
                                );
                            }
                        }
                    }
                    DECLARE_ARGS | FOREACH => {
                        if let Some(block) = &call.block {
                            analyzed_block.merge(&self.analyze_block(
                                block,
                                request_time,
                                document,
                                deps,
                                visiting,
                            ));
                        }
                    }
                    SET_DEFAULTS => {}
                    FORWARD_VARIABLES_FROM => {
                        if let Some(strings) = call
                            .args
                            .get(1)
                            .and_then(|expr| expr.as_primary_list())
                            .map(|list| {
                                list.values
                                    .iter()
                                    .filter_map(|expr| expr.as_primary_string())
                                    .collect::<Vec<_>>()
                            })
                        {
                            for string in strings {
                                if let Some(name) = parse_simple_literal(string.raw_value) {
                                    if is_exported(name) {
                                        analyzed_block.variables.insert(
                                            name,
                                            AnalyzedVariable {
                                                assignments: [(
                                                    string.span,
                                                    AnalyzedAssignment {
                                                        name,
                                                        comments: Comments::default(),
                                                        statement,
                                                        document,
                                                        variable_span: string.span,
                                                    },
                                                )]
                                                .into(),
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some(name) = call
                            .only_arg()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            analyzed_block.targets.insert(
                                name,
                                AnalyzedTarget {
                                    name,
                                    call,
                                    document,
                                    header: call.args[0].span(),
                                    span: call.span,
                                },
                            );
                        }
                    }
                },
                Statement::Condition(condition) => {
                    let mut current_condition = condition;
                    loop {
                        analyzed_block.merge(&self.analyze_block(
                            &current_condition.then_block,
                            request_time,
                            document,
                            deps,
                            visiting,
                        ));
                        match &current_condition.else_block {
                            None => break,
                            Some(Either::Left(next_condition)) => {
                                current_condition = next_condition;
                            }
                            Some(Either::Right(block)) => {
                                analyzed_block.merge(&self.analyze_block(
                                    block,
                                    request_time,
                                    document,
                                    deps,
                                    visiting,
                                ));
                                break;
                            }
                        }
                    }
                }
                Statement::Error(_) => {}
            }
        }

        analyzed_block.finalize()
    }
}
