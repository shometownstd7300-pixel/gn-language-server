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
    sync::{Arc, Mutex, OnceLock, RwLock},
    time::Instant,
};

use either::Either;
use pest::Span;

use crate::{
    analyzer::{
        cache::AnalysisNode,
        diagnostics::collect_diagnostics,
        links::collect_links,
        shallow::ShallowAnalyzer,
        symbols::collect_symbols,
        AnalyzedAssignment, AnalyzedBlock, AnalyzedEvent, AnalyzedFile, AnalyzedImport,
        AnalyzedLink, AnalyzedTarget, AnalyzedTemplate, WorkspaceContext,
    },
    common::{
        builtins::{DECLARE_ARGS, FOREACH, FORWARD_VARIABLES_FROM, IMPORT, SET_DEFAULTS, TEMPLATE},
        storage::{Document, DocumentStorage},
        utils::parse_simple_literal,
    },
    parser::{parse, Block, Comments, Expr, LValue, Node, PrimaryExpr, Statement},
};

pub struct FullAnalyzer {
    context: WorkspaceContext,
    shallow_analyzer: ShallowAnalyzer,
    storage: Arc<Mutex<DocumentStorage>>,
    #[allow(clippy::type_complexity)]
    cache: RwLock<BTreeMap<PathBuf, Arc<RwLock<OnceLock<Pin<Arc<AnalyzedFile>>>>>>>,
}

impl FullAnalyzer {
    pub fn new(context: &WorkspaceContext, storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            context: context.clone(),
            storage: storage.clone(),
            shallow_analyzer: ShallowAnalyzer::new(context, storage),
            cache: Default::default(),
        }
    }

    pub fn get_shallow(&self) -> &ShallowAnalyzer {
        &self.shallow_analyzer
    }

    pub fn analyze(&self, path: &Path, request_time: Instant) -> Pin<Arc<AnalyzedFile>> {
        self.analyze_cached(path, request_time)
    }

    fn analyze_cached(&self, path: &Path, request_time: Instant) -> Pin<Arc<AnalyzedFile>> {
        let entry = {
            let read_lock = self.cache.read().unwrap();
            if let Some(entry) = read_lock.get(path) {
                entry.clone()
            } else {
                drop(read_lock);
                let mut write_lock = self.cache.write().unwrap();
                write_lock.entry(path.to_path_buf()).or_default().clone()
            }
        };

        {
            let read_lock = entry.read().unwrap();
            let cached_file = read_lock.get_or_init(|| self.analyze_uncached(path, request_time));
            if cached_file
                .node
                .verify(request_time, &self.storage.lock().unwrap())
            {
                return cached_file.clone();
            }
        }

        let mut write_lock = entry.write().unwrap();

        let cached_file = write_lock.get_or_init(|| self.analyze_uncached(path, request_time));
        if cached_file
            .node
            .verify(request_time, &self.storage.lock().unwrap())
        {
            return cached_file.clone();
        }

        *write_lock = Default::default();
        let cached_file = write_lock.get_or_init(|| self.analyze_uncached(path, request_time));
        if cached_file
            .node
            .verify(request_time, &self.storage.lock().unwrap())
        {
            return cached_file.clone();
        }
        unreachable!();
    }

    fn analyze_uncached(&self, path: &Path, request_time: Instant) -> Pin<Arc<AnalyzedFile>> {
        let document = self.storage.lock().unwrap().read(path);
        let ast_root = Box::pin(parse(&document.data));

        let mut deps = Vec::new();
        let mut analyzed_root = self.analyze_block(&ast_root, request_time, &document, &mut deps);

        // Insert a synthetic import of BUILDCONFIG.gn.
        let dot_gn_file = self
            .shallow_analyzer
            .analyze(&self.context.build_config, request_time);
        analyzed_root.events.insert(
            0,
            AnalyzedEvent::Import(AnalyzedImport {
                file: dot_gn_file.clone(),
                span: Span::new(&document.data, 0, 0).unwrap(),
            }),
        );
        deps.push(dot_gn_file.node.clone());

        let links = collect_links(&ast_root, path, &self.context);
        let symbols = collect_symbols(ast_root.as_node(), &document.line_index);
        let diagnostics = collect_diagnostics(&document, &ast_root);

        // SAFETY: links' contents are backed by pinned document.
        let links = unsafe { std::mem::transmute::<Vec<AnalyzedLink>, Vec<AnalyzedLink>>(links) };
        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root =
            unsafe { std::mem::transmute::<AnalyzedBlock, AnalyzedBlock>(analyzed_root) };
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };

        AnalyzedFile::new(
            document,
            self.context.root.clone(),
            ast_root,
            analyzed_root,
            links,
            symbols,
            diagnostics,
            deps,
            request_time,
        )
    }

    fn analyze_block<'i, 'p>(
        &self,
        block: &'p Block<'i>,
        request_time: Instant,
        document: &'i Document,
        deps: &mut Vec<Arc<AnalysisNode>>,
    ) -> AnalyzedBlock<'i, 'p> {
        let mut events: Vec<AnalyzedEvent> = Vec::new();

        for statement in &block.statements {
            match statement {
                Statement::Assignment(assignment) => {
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
                        request_time,
                        document,
                        deps,
                    ));
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
                            let file = self.shallow_analyzer.analyze(&path, request_time);
                            deps.push(file.node.clone());
                            events.push(AnalyzedEvent::Import(AnalyzedImport {
                                file,
                                span: call.span(),
                            }));
                        }
                    }
                    TEMPLATE => {
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
                                request_time,
                                document,
                                deps,
                            )));
                        }
                    }
                    DECLARE_ARGS => {
                        if let Some(block) = &call.block {
                            let analyzed_root =
                                self.analyze_block(block, request_time, document, deps);
                            events.push(AnalyzedEvent::DeclareArgs(analyzed_root));
                        }
                    }
                    FOREACH => {
                        if let Some(block) = &call.block {
                            events.extend(
                                self.analyze_block(block, request_time, document, deps)
                                    .events,
                            );
                        }
                    }
                    SET_DEFAULTS => {
                        if let Some(block) = &call.block {
                            let analyzed_root =
                                self.analyze_block(block, request_time, document, deps);
                            events.push(AnalyzedEvent::NewScope(analyzed_root));
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
                            events.extend(strings.into_iter().filter_map(|string| {
                                parse_simple_literal(string.raw_value).map(|name| {
                                    AnalyzedEvent::Assignment(AnalyzedAssignment {
                                        name,
                                        comments: Comments::default(),
                                        statement,
                                        document,
                                        variable_span: string.span,
                                    })
                                })
                            }));
                        }
                    }
                    _ => {
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
                                request_time,
                                document,
                                deps,
                            )));
                        }
                    }
                },
                Statement::Condition(condition) => {
                    let mut condition_blocks = Vec::new();
                    let mut current_condition = condition;
                    loop {
                        events.extend(self.analyze_expr(
                            &current_condition.condition,
                            request_time,
                            document,
                            deps,
                        ));
                        condition_blocks.push(self.analyze_block(
                            &current_condition.then_block,
                            request_time,
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
                                    request_time,
                                    document,
                                    deps,
                                ));
                                break;
                            }
                        }
                    }
                    events.push(AnalyzedEvent::Conditions(condition_blocks));
                }
                Statement::Error(_) => {}
            }
        }

        AnalyzedBlock {
            events,
            span: block.span,
        }
    }

    fn analyze_expr<'i, 'p>(
        &self,
        expr: &'p Expr<'i>,
        request_time: Instant,
        document: &'i Document,
        deps: &mut Vec<Arc<AnalysisNode>>,
    ) -> Vec<AnalyzedEvent<'i, 'p>> {
        match expr {
            Expr::Primary(primary_expr) => match primary_expr.as_ref() {
                PrimaryExpr::Block(block) => {
                    let analyzed_root = self.analyze_block(block, request_time, document, deps);
                    vec![AnalyzedEvent::NewScope(analyzed_root)]
                }
                PrimaryExpr::Call(call) => {
                    let mut events: Vec<AnalyzedEvent> = call
                        .args
                        .iter()
                        .flat_map(|expr| self.analyze_expr(expr, request_time, document, deps))
                        .collect();
                    if let Some(block) = &call.block {
                        let analyzed_root = self.analyze_block(block, request_time, document, deps);
                        events.push(AnalyzedEvent::NewScope(analyzed_root));
                    }
                    events
                }
                PrimaryExpr::ParenExpr(paren_expr) => {
                    self.analyze_expr(&paren_expr.expr, request_time, document, deps)
                }
                PrimaryExpr::List(list_literal) => list_literal
                    .values
                    .iter()
                    .flat_map(|expr| self.analyze_expr(expr, request_time, document, deps))
                    .collect(),
                PrimaryExpr::Identifier(_)
                | PrimaryExpr::Integer(_)
                | PrimaryExpr::String(_)
                | PrimaryExpr::ArrayAccess(_)
                | PrimaryExpr::ScopeAccess(_)
                | PrimaryExpr::Error(_) => Vec::new(),
            },
            Expr::Unary(unary_expr) => {
                self.analyze_expr(&unary_expr.expr, request_time, document, deps)
            }
            Expr::Binary(binary_expr) => {
                let mut events = self.analyze_expr(&binary_expr.lhs, request_time, document, deps);
                events.extend(self.analyze_expr(&binary_expr.rhs, request_time, document, deps));
                events
            }
        }
    }
}
