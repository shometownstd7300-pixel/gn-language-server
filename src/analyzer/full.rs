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
use pest::Span;

use crate::{
    analyzer::{
        cache::CacheNode,
        data::{
            AnalyzedBuiltinCall, AnalyzedCondition, AnalyzedDeclareArgs, AnalyzedForeach,
            AnalyzedForwardVariablesFrom, AnalyzedStatement, SyntheticImport,
        },
        links::collect_links,
        shallow::{ShallowAnalysisSnapshot, ShallowAnalyzer},
        symbols::collect_symbols,
        AnalyzedAssignment, AnalyzedBlock, AnalyzedFile, AnalyzedImport, AnalyzedLink,
        AnalyzedTarget, AnalyzedTemplate, WorkspaceContext,
    },
    common::{
        builtins::{DECLARE_ARGS, FOREACH, FORWARD_VARIABLES_FROM, IMPORT, SET_DEFAULTS, TEMPLATE},
        storage::{Document, DocumentStorage},
    },
    parser::{parse, Block, Call, Condition, Expr, LValue, Node, PrimaryExpr, Statement},
};

pub struct FullAnalyzer {
    context: WorkspaceContext,
    shallow_analyzer: ShallowAnalyzer,
    storage: Arc<Mutex<DocumentStorage>>,
    #[allow(clippy::type_complexity)]
    cache: BTreeMap<PathBuf, Pin<Arc<AnalyzedFile>>>,
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

    pub fn get_shallow(&mut self) -> &mut ShallowAnalyzer {
        &mut self.shallow_analyzer
    }

    pub fn analyze(&mut self, path: &Path, request_time: Instant) -> Pin<Arc<AnalyzedFile>> {
        self.analyze_cached(path, request_time)
    }

    fn analyze_cached(&mut self, path: &Path, request_time: Instant) -> Pin<Arc<AnalyzedFile>> {
        if let Some(cached_file) = self.cache.get(path) {
            if cached_file
                .node
                .verify(request_time, &self.storage.lock().unwrap())
            {
                return cached_file.clone();
            }
        }

        let new_file = self.analyze_uncached(path, request_time);
        self.cache.insert(path.to_path_buf(), new_file.clone());
        new_file
    }

    fn analyze_uncached(&mut self, path: &Path, request_time: Instant) -> Pin<Arc<AnalyzedFile>> {
        let document = self.storage.lock().unwrap().read(path);
        let ast = Box::pin(parse(&document.data));

        let mut deps = Vec::new();
        let mut snapshot = ShallowAnalysisSnapshot::new();
        let mut analyzed_root =
            self.analyze_block(&ast, &document, request_time, &mut snapshot, &mut deps);

        // Insert a synthetic import of BUILDCONFIG.gn.
        let dot_gn_file =
            self.shallow_analyzer
                .analyze(&self.context.build_config, request_time, &mut snapshot);
        analyzed_root.statements.insert(
            0,
            AnalyzedStatement::SyntheticImport(Box::new(SyntheticImport {
                file: dot_gn_file.clone(),
                span: Span::new(&document.data, 0, 0).unwrap(),
            })),
        );
        deps.push(dot_gn_file.node.clone());

        let links = collect_links(&ast, path, &self.context);
        let symbols = collect_symbols(ast.as_node(), &document.line_index);

        // SAFETY: links' contents are backed by pinned document.
        let links = unsafe { std::mem::transmute::<Vec<AnalyzedLink>, Vec<AnalyzedLink>>(links) };
        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast.
        let analyzed_root =
            unsafe { std::mem::transmute::<AnalyzedBlock, AnalyzedBlock>(analyzed_root) };
        // SAFETY: ast's contents are backed by pinned document.
        let ast = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast) };

        AnalyzedFile::new(
            document,
            self.context.root.clone(),
            ast,
            analyzed_root,
            links,
            symbols,
            deps,
            request_time,
        )
    }

    fn analyze_block<'i, 'p>(
        &mut self,
        block: &'p Block<'i>,
        document: &'i Document,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
        deps: &mut Vec<Arc<CacheNode>>,
    ) -> AnalyzedBlock<'i, 'p> {
        let mut statements: Vec<AnalyzedStatement> = Vec::new();

        for statement in &block.statements {
            match statement {
                Statement::Assignment(assignment) => {
                    let (identifier, mut expr_scopes) = match &assignment.lvalue {
                        LValue::Identifier(identifier) => (identifier.as_ref(), Vec::new()),
                        LValue::ArrayAccess(array_access) => (
                            &array_access.array,
                            self.analyze_expr(
                                &array_access.index,
                                document,
                                request_time,
                                snapshot,
                                deps,
                            ),
                        ),
                        LValue::ScopeAccess(scope_access) => (&scope_access.scope, Vec::new()),
                    };
                    expr_scopes.extend(self.analyze_expr(
                        &assignment.rvalue,
                        document,
                        request_time,
                        snapshot,
                        deps,
                    ));
                    statements.push(AnalyzedStatement::Assignment(Box::new(
                        AnalyzedAssignment {
                            assignment,
                            primary_variable: identifier.span,
                            comments: assignment.comments.clone(),
                            expr_scopes,
                        },
                    )));
                }
                Statement::Call(call) => {
                    statements.push(self.analyze_call(
                        call,
                        document,
                        request_time,
                        snapshot,
                        deps,
                    ));
                }
                Statement::Condition(condition) => {
                    statements.push(AnalyzedStatement::Conditions(Box::new(
                        self.analyze_condition(condition, document, request_time, snapshot, deps),
                    )));
                }
                Statement::Error(_) => {}
            }
        }

        AnalyzedBlock {
            statements,
            block,
            document,
            span: block.span,
        }
    }

    fn analyze_call<'i, 'p>(
        &mut self,
        call: &'p Call<'i>,
        document: &'i Document,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
        deps: &mut Vec<Arc<CacheNode>>,
    ) -> AnalyzedStatement<'i, 'p> {
        let body_block = call
            .block
            .as_ref()
            .map(|block| self.analyze_block(block, document, request_time, snapshot, deps));

        let body_block = match (call.function.name, body_block) {
            (DECLARE_ARGS, Some(body_block)) => {
                return AnalyzedStatement::DeclareArgs(Box::new(AnalyzedDeclareArgs {
                    call,
                    body_block,
                }));
            }
            (FOREACH, Some(body_block)) => {
                if call.args.len() == 2 {
                    if let Some(loop_variable) = call.args[0].as_primary_identifier() {
                        let loop_items = &call.args[1];
                        let expr_scopes =
                            self.analyze_expr(loop_items, document, request_time, snapshot, deps);
                        return AnalyzedStatement::Foreach(Box::new(AnalyzedForeach {
                            call,
                            loop_variable,
                            loop_items,
                            expr_scopes,
                            body_block,
                        }));
                    }
                }
                Some(body_block)
            }
            (FORWARD_VARIABLES_FROM, None) => {
                if call.args.len() == 2 || call.args.len() == 3 {
                    let expr_scopes = call
                        .args
                        .iter()
                        .flat_map(|expr| {
                            self.analyze_expr(expr, document, request_time, snapshot, deps)
                        })
                        .collect();
                    return AnalyzedStatement::ForwardVariablesFrom(Box::new(
                        AnalyzedForwardVariablesFrom {
                            call,
                            expr_scopes,
                            includes: &call.args[1],
                        },
                    ));
                }
                None
            }
            (IMPORT, None) => {
                if let Some(name) = call.only_arg().and_then(|expr| expr.as_simple_string()) {
                    let path = self
                        .context
                        .resolve_path(name, document.path.parent().unwrap());
                    let file = self.shallow_analyzer.analyze(&path, request_time, snapshot);
                    deps.push(file.node.clone());
                    return AnalyzedStatement::Import(Box::new(AnalyzedImport { call, file }));
                }
                None
            }
            (TEMPLATE, Some(body_block)) => {
                if let Some(name) = call.only_arg() {
                    let expr_scopes = call
                        .args
                        .iter()
                        .flat_map(|expr| {
                            self.analyze_expr(expr, document, request_time, snapshot, deps)
                        })
                        .collect();
                    return AnalyzedStatement::Template(Box::new(AnalyzedTemplate {
                        call,
                        name,
                        comments: call.comments.clone(),
                        expr_scopes,
                        body_block,
                    }));
                }
                Some(body_block)
            }
            (name, Some(body_block)) if name != SET_DEFAULTS => {
                if let Some(name) = call.only_arg() {
                    let expr_scopes = call
                        .args
                        .iter()
                        .flat_map(|expr| {
                            self.analyze_expr(expr, document, request_time, snapshot, deps)
                        })
                        .collect();
                    return AnalyzedStatement::Target(Box::new(AnalyzedTarget {
                        call,
                        name,
                        expr_scopes,
                        body_block,
                    }));
                }
                Some(body_block)
            }
            (_, body_block) => body_block,
        };

        let expr_scopes = call
            .args
            .iter()
            .flat_map(|expr| self.analyze_expr(expr, document, request_time, snapshot, deps))
            .collect();
        AnalyzedStatement::BuiltinCall(Box::new(AnalyzedBuiltinCall {
            call,
            expr_scopes,
            body_block,
        }))
    }

    fn analyze_condition<'i, 'p>(
        &mut self,
        condition: &'p Condition<'i>,
        document: &'i Document,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
        deps: &mut Vec<Arc<CacheNode>>,
    ) -> AnalyzedCondition<'i, 'p> {
        let expr_scopes =
            self.analyze_expr(&condition.condition, document, request_time, snapshot, deps);
        let then_block = self.analyze_block(
            &condition.then_block,
            document,
            request_time,
            snapshot,
            deps,
        );
        let else_block =
            match &condition.else_block {
                None => None,
                Some(Either::Left(next_condition)) => Some(Either::Left(Box::new(
                    self.analyze_condition(next_condition, document, request_time, snapshot, deps),
                ))),
                Some(Either::Right(last_block)) => Some(Either::Right(Box::new(
                    self.analyze_block(last_block, document, request_time, snapshot, deps),
                ))),
            };
        AnalyzedCondition {
            condition,
            expr_scopes,
            then_block,
            else_block,
        }
    }

    fn analyze_expr<'i, 'p>(
        &mut self,
        expr: &'p Expr<'i>,
        document: &'i Document,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
        deps: &mut Vec<Arc<CacheNode>>,
    ) -> Vec<AnalyzedBlock<'i, 'p>> {
        match expr {
            Expr::Primary(primary_expr) => match primary_expr.as_ref() {
                PrimaryExpr::Block(block) => {
                    let analyzed_block =
                        self.analyze_block(block, document, request_time, snapshot, deps);
                    vec![analyzed_block]
                }
                PrimaryExpr::Call(call) => {
                    let mut analyzed_blocks: Vec<AnalyzedBlock> = call
                        .args
                        .iter()
                        .flat_map(|expr| {
                            self.analyze_expr(expr, document, request_time, snapshot, deps)
                        })
                        .collect();
                    if let Some(block) = &call.block {
                        let analyzed_block =
                            self.analyze_block(block, document, request_time, snapshot, deps);
                        analyzed_blocks.push(analyzed_block);
                    }
                    analyzed_blocks
                }
                PrimaryExpr::ParenExpr(paren_expr) => {
                    self.analyze_expr(&paren_expr.expr, document, request_time, snapshot, deps)
                }
                PrimaryExpr::List(list_literal) => list_literal
                    .values
                    .iter()
                    .flat_map(|expr| {
                        self.analyze_expr(expr, document, request_time, snapshot, deps)
                    })
                    .collect(),
                PrimaryExpr::Identifier(_)
                | PrimaryExpr::Integer(_)
                | PrimaryExpr::String(_)
                | PrimaryExpr::ArrayAccess(_)
                | PrimaryExpr::ScopeAccess(_)
                | PrimaryExpr::Error(_) => Vec::new(),
            },
            Expr::Unary(unary_expr) => {
                self.analyze_expr(&unary_expr.expr, document, request_time, snapshot, deps)
            }
            Expr::Binary(binary_expr) => {
                let mut analyzed_blocks =
                    self.analyze_expr(&binary_expr.lhs, document, request_time, snapshot, deps);
                analyzed_blocks.extend(self.analyze_expr(
                    &binary_expr.rhs,
                    document,
                    request_time,
                    snapshot,
                    deps,
                ));
                analyzed_blocks
            }
        }
    }
}
