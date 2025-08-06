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
    sync::{Arc, Mutex, RwLock},
    time::Instant,
};

use either::Either;
use pest::Span;

use crate::{
    analyze::{
        links::collect_links, shallow::ShallowAnalyzer, symbols::collect_symbols,
        utils::compute_next_verify, AnalyzedAssignment, AnalyzedBlock, AnalyzedEvent, AnalyzedFile,
        AnalyzedImport, AnalyzedLink, AnalyzedTarget, AnalyzedTemplate, ShallowAnalyzedFile,
        WorkspaceContext,
    },
    ast::{parse, Block, Comments, Expr, LValue, Node, PrimaryExpr, Statement},
    builtins::{DECLARE_ARGS, FOREACH, FORWARD_VARIABLES_FROM, IMPORT, SET_DEFAULTS, TEMPLATE},
    storage::{Document, DocumentStorage},
    utils::{parse_simple_literal, CacheConfig},
};

pub struct FullAnalyzer {
    context: WorkspaceContext,
    shallow_analyzer: ShallowAnalyzer,
    storage: Arc<Mutex<DocumentStorage>>,
    cache: BTreeMap<PathBuf, Pin<Arc<AnalyzedFile>>>,
}

impl FullAnalyzer {
    pub fn new(context: &WorkspaceContext, storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            context: context.clone(),
            storage: storage.clone(),
            shallow_analyzer: ShallowAnalyzer::new(context, storage),
            cache: BTreeMap::new(),
        }
    }

    pub fn get_shallow(&self) -> &ShallowAnalyzer {
        &self.shallow_analyzer
    }

    pub fn get_shallow_mut(&mut self) -> &mut ShallowAnalyzer {
        &mut self.shallow_analyzer
    }

    pub fn analyze(&mut self, path: &Path, cache_config: CacheConfig) -> Pin<Arc<AnalyzedFile>> {
        self.analyze_cached(path, cache_config)
    }

    fn analyze_cached(&mut self, path: &Path, cache_config: CacheConfig) -> Pin<Arc<AnalyzedFile>> {
        if self.cache.contains_key(path) {
            let cached_file = self.cache.get(path).unwrap();
            let storage = self.storage.lock().unwrap();
            if cached_file.maybe_verify(cache_config, &storage) {
                return cached_file.clone();
            }
        }

        let new_file = self.analyze_uncached(path, cache_config);
        self.cache.insert(path.to_path_buf(), new_file.clone());
        new_file
    }

    fn analyze_uncached(
        &mut self,
        path: &Path,
        cache_config: CacheConfig,
    ) -> Pin<Arc<AnalyzedFile>> {
        let document = self.storage.lock().unwrap().read(path);
        let ast_root = Box::pin(parse(&document.data));

        let mut deps = Vec::new();
        let mut analyzed_root = self.analyze_block(&ast_root, cache_config, &document, &mut deps);

        // Insert a synthetic import of BUILDCONFIG.gn.
        let dot_gn_file = self
            .shallow_analyzer
            .analyze(&self.context.build_config, cache_config);
        analyzed_root.events.insert(
            0,
            AnalyzedEvent::Import(AnalyzedImport {
                file: dot_gn_file.clone(),
                span: Span::new(&document.data, 0, 0).unwrap(),
            }),
        );
        deps.push(dot_gn_file);

        let links = collect_links(&ast_root, path, &self.context);
        let symbols = collect_symbols(ast_root.as_node(), &document.line_index);

        // SAFETY: links' contents are backed by pinned document.
        let links = unsafe { std::mem::transmute::<Vec<AnalyzedLink>, Vec<AnalyzedLink>>(links) };
        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root =
            unsafe { std::mem::transmute::<AnalyzedBlock, AnalyzedBlock>(analyzed_root) };
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };
        let next_verify = RwLock::new(compute_next_verify(Instant::now(), document.version));

        Arc::pin(AnalyzedFile {
            document,
            workspace_root: self.context.root.clone(),
            ast_root,
            analyzed_root,
            deps,
            links,
            symbols,
            next_verify,
        })
    }

    fn analyze_block<'i, 'p>(
        &mut self,
        block: &'p Block<'i>,
        cache_config: CacheConfig,
        document: &'i Document,
        deps: &mut Vec<Pin<Arc<ShallowAnalyzedFile>>>,
    ) -> AnalyzedBlock<'i, 'p> {
        let events: Vec<AnalyzedEvent> = block
            .statements
            .iter()
            .flat_map(|statement| -> Vec<AnalyzedEvent> {
                match statement {
                    Statement::Assignment(assignment) => {
                        let mut events = Vec::new();
                        let identifier = match &assignment.lvalue {
                            LValue::Identifier(identifier) => identifier,
                            LValue::ArrayAccess(array_access) => &array_access.array,
                            LValue::ScopeAccess(scope_access) => &scope_access.scope,
                        };
                        events.push(AnalyzedEvent::Assignment(AnalyzedAssignment {
                            name: identifier.name,
                            comments: assignment.comments.clone(),
                            statement,
                            document,
                            variable_span: identifier.span,
                        }));
                        events.extend(self.analyze_expr(
                            &assignment.rvalue,
                            cache_config,
                            document,
                            deps,
                        ));
                        events
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
                                let file = self.shallow_analyzer.analyze(&path, cache_config);
                                deps.push(file.clone());
                                vec![AnalyzedEvent::Import(AnalyzedImport {
                                    file,
                                    span: call.span(),
                                })]
                            } else {
                                Vec::new()
                            }
                        }
                        TEMPLATE => {
                            let mut events = Vec::new();
                            if let Some(name) = call
                                .only_arg()
                                .and_then(|expr| expr.as_primary_string())
                                .and_then(|s| parse_simple_literal(s.raw_value))
                            {
                                events.push(AnalyzedEvent::Template(AnalyzedTemplate {
                                    name,
                                    comments: call.comments.clone(),
                                    document,
                                    header: call.function.span,
                                    span: call.span,
                                }));
                            }
                            if let Some(block) = &call.block {
                                events.push(AnalyzedEvent::NewScope(self.analyze_block(
                                    block,
                                    cache_config,
                                    document,
                                    deps,
                                )));
                            }
                            events
                        }
                        DECLARE_ARGS => {
                            if let Some(block) = &call.block {
                                let analyzed_root =
                                    self.analyze_block(block, cache_config, document, deps);
                                vec![AnalyzedEvent::DeclareArgs(analyzed_root)]
                            } else {
                                Vec::new()
                            }
                        }
                        FOREACH => {
                            if let Some(block) = &call.block {
                                self.analyze_block(block, cache_config, document, deps)
                                    .events
                            } else {
                                Vec::new()
                            }
                        }
                        SET_DEFAULTS => {
                            if let Some(block) = &call.block {
                                let analyzed_root =
                                    self.analyze_block(block, cache_config, document, deps);
                                vec![AnalyzedEvent::NewScope(analyzed_root)]
                            } else {
                                Vec::new()
                            }
                        }
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
                                return strings
                                    .into_iter()
                                    .filter_map(|string| {
                                        parse_simple_literal(string.raw_value).map(|name| {
                                            AnalyzedEvent::Assignment(AnalyzedAssignment {
                                                name,
                                                comments: Comments::default(),
                                                statement,
                                                document,
                                                variable_span: string.span,
                                            })
                                        })
                                    })
                                    .collect();
                            }
                            Vec::new()
                        }
                        _ => {
                            let mut events = Vec::new();
                            if let Some(name) = call
                                .only_arg()
                                .and_then(|expr| expr.as_primary_string())
                                .and_then(|s| parse_simple_literal(s.raw_value))
                            {
                                events.push(AnalyzedEvent::Target(AnalyzedTarget {
                                    name,
                                    call,
                                    document,
                                    header: call.args[0].span(),
                                    span: call.span,
                                }));
                            }
                            if let Some(block) = &call.block {
                                events.push(AnalyzedEvent::NewScope(self.analyze_block(
                                    block,
                                    cache_config,
                                    document,
                                    deps,
                                )));
                            }
                            events
                        }
                    },
                    Statement::Condition(condition) => {
                        let mut events = Vec::new();
                        let mut condition_blocks = Vec::new();
                        let mut current_condition = condition;
                        loop {
                            events.extend(self.analyze_expr(
                                &current_condition.condition,
                                cache_config,
                                document,
                                deps,
                            ));
                            condition_blocks.push(self.analyze_block(
                                &current_condition.then_block,
                                cache_config,
                                document,
                                deps,
                            ));
                            match &current_condition.else_block {
                                None => break,
                                Some(Either::Left(next_condition)) => {
                                    current_condition = next_condition;
                                }
                                Some(Either::Right(block)) => {
                                    condition_blocks.push(self.analyze_block(
                                        block,
                                        cache_config,
                                        document,
                                        deps,
                                    ));
                                    break;
                                }
                            }
                        }
                        events.push(AnalyzedEvent::Conditions(condition_blocks));
                        events
                    }
                    Statement::Error(_) => Vec::new(),
                }
            })
            .collect();

        AnalyzedBlock {
            events,
            span: block.span,
        }
    }

    fn analyze_expr<'i, 'p>(
        &mut self,
        expr: &'p Expr<'i>,
        cache_config: CacheConfig,
        document: &'i Document,
        deps: &mut Vec<Pin<Arc<ShallowAnalyzedFile>>>,
    ) -> Vec<AnalyzedEvent<'i, 'p>> {
        match expr {
            Expr::Primary(primary_expr) => match primary_expr.as_ref() {
                PrimaryExpr::Block(block) => {
                    let analyzed_root = self.analyze_block(block, cache_config, document, deps);
                    vec![AnalyzedEvent::NewScope(analyzed_root)]
                }
                PrimaryExpr::Call(call) => {
                    let mut events: Vec<AnalyzedEvent> = call
                        .args
                        .iter()
                        .flat_map(|expr| self.analyze_expr(expr, cache_config, document, deps))
                        .collect();
                    if let Some(block) = &call.block {
                        let analyzed_root = self.analyze_block(block, cache_config, document, deps);
                        events.push(AnalyzedEvent::NewScope(analyzed_root));
                    }
                    events
                }
                PrimaryExpr::ParenExpr(paren_expr) => {
                    self.analyze_expr(&paren_expr.expr, cache_config, document, deps)
                }
                PrimaryExpr::List(list_literal) => list_literal
                    .values
                    .iter()
                    .flat_map(|expr| self.analyze_expr(expr, cache_config, document, deps))
                    .collect(),
                PrimaryExpr::Identifier(_)
                | PrimaryExpr::Integer(_)
                | PrimaryExpr::String(_)
                | PrimaryExpr::ArrayAccess(_)
                | PrimaryExpr::ScopeAccess(_)
                | PrimaryExpr::Error(_) => Vec::new(),
            },
            Expr::Unary(unary_expr) => {
                self.analyze_expr(&unary_expr.expr, cache_config, document, deps)
            }
            Expr::Binary(binary_expr) => {
                let mut events = self.analyze_expr(&binary_expr.lhs, cache_config, document, deps);
                events.extend(self.analyze_expr(&binary_expr.rhs, cache_config, document, deps));
                events
            }
        }
    }
}
