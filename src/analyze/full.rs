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
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use either::Either;
use futures::future::join_all;
use itertools::Itertools;
use pest::Span;
use tokio::sync::{Mutex, RwLock};

use crate::{
    analyze::{
        links::collect_links,
        shallow::ShallowAnalyzer,
        symbols::collect_symbols,
        utils::{compute_next_check, FreshCache},
        AnalyzedAssignment, AnalyzedBlock, AnalyzedEvent, AnalyzedFile, AnalyzedImport,
        AnalyzedLink, AnalyzedTarget, AnalyzedTemplate, ShallowAnalyzedFile, WorkspaceContext,
    },
    ast::{parse, Block, Comments, Expr, LValue, Node, PrimaryExpr, Statement},
    builtins::{DECLARE_ARGS, FOREACH, FORWARD_VARIABLES_FROM, IMPORT, SET_DEFAULTS, TEMPLATE},
    error::{Error, Result},
    storage::{Document, DocumentStorage},
    utils::{parse_simple_literal, CacheConfig},
};

pub struct FullAnalyzer {
    context: WorkspaceContext,
    shallow_analyzer: ShallowAnalyzer,
    storage: Arc<Mutex<DocumentStorage>>,
    cache: FreshCache<PathBuf, Pin<Arc<AnalyzedFile>>>,
}

impl FullAnalyzer {
    pub fn new(context: &WorkspaceContext, storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            context: context.clone(),
            storage: storage.clone(),
            shallow_analyzer: ShallowAnalyzer::new(context, storage),
            cache: FreshCache::new(),
        }
    }

    pub fn get_shallow(&self) -> &ShallowAnalyzer {
        &self.shallow_analyzer
    }

    pub async fn analyze(
        &self,
        path: &Path,
        cache_config: CacheConfig,
    ) -> Result<Pin<Arc<AnalyzedFile>>> {
        if !path.is_absolute() {
            return Err(Error::General("Path must be absolute".to_string()));
        }
        self.analyze_cached(path, cache_config).await
    }

    async fn analyze_cached(
        &self,
        path: &Path,
        cache_config: CacheConfig,
    ) -> Result<Pin<Arc<AnalyzedFile>>> {
        self.cache
            .get_or_insert(
                path.to_path_buf(),
                async |file| {
                    file.is_fresh(cache_config, &*self.storage.lock().await)
                        .await
                },
                async || self.analyze_uncached(path, cache_config).await,
            )
            .await
    }

    async fn analyze_uncached(
        &self,
        path: &Path,
        cache_config: CacheConfig,
    ) -> Result<Pin<Arc<AnalyzedFile>>> {
        let document = self.storage.lock().await.read(path)?;
        let ast_root = Box::pin(parse(&document.data));

        let mut deps = Vec::new();
        let mut analyzed_root = self
            .analyze_block(&ast_root, cache_config, &document, &mut deps)
            .await?;

        // Insert a synthetic import of BUILDCONFIG.gn.
        let dot_gn_file = self
            .shallow_analyzer
            .analyze(&self.context.build_config, cache_config)
            .await?;
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
        let next_check = RwLock::new(compute_next_check(Instant::now(), document.version));

        Ok(Arc::pin(AnalyzedFile {
            document,
            workspace_root: self.context.root.clone(),
            ast_root,
            analyzed_root,
            deps,
            links,
            symbols,
            next_check,
        }))
    }

    async fn analyze_block<'i, 'p>(
        &self,
        block: &'p Block<'i>,
        cache_config: CacheConfig,
        document: &'i Document,
        deps: &mut Vec<Pin<Arc<ShallowAnalyzedFile>>>,
    ) -> Result<AnalyzedBlock<'i, 'p>> {
        let events: Vec<AnalyzedEvent> = join_all(block.statements.iter().map(
            async move |statement| -> Result<Vec<AnalyzedEvent>> {
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
                        events.extend(
                            self.analyze_expr(&assignment.rvalue, cache_config, document, deps)
                                .await?,
                        );
                        Result::<Vec<AnalyzedEvent>>::Ok(events)
                    }
                    Statement::Call(call) => {
                        match call.function.name {
                            IMPORT => {
                                if let Some(name) = call
                                    .only_arg()
                                    .and_then(|expr| expr.as_primary_string())
                                    .and_then(|s| parse_simple_literal(s.raw_value))
                                {
                                    let path = self
                                        .context
                                        .resolve_path(name, document.path.parent().unwrap());
                                    let file = match self
                                        .shallow_analyzer
                                        .analyze(&path, cache_config)
                                        .await
                                    {
                                        Err(err) if err.is_not_found() => {
                                            // Ignore missing imports as they might be imported conditionally.
                                            ShallowAnalyzedFile::empty(&path)
                                        }
                                        other => other?,
                                    };
                                    deps.push(file.clone());
                                    Ok(vec![AnalyzedEvent::Import(AnalyzedImport {
                                        file,
                                        span: call.span(),
                                    })])
                                } else {
                                    Ok(Vec::new())
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
                                    events.push(AnalyzedEvent::NewScope(
                                        self.analyze_block(block, cache_config, document, deps)
                                            .await?,
                                    ));
                                }
                                Ok(events)
                            }
                            DECLARE_ARGS => {
                                if let Some(block) = &call.block {
                                    let analyzed_root = self
                                        .analyze_block(block, cache_config, document, deps)
                                        .await?;
                                    Ok(vec![AnalyzedEvent::DeclareArgs(analyzed_root)])
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            FOREACH => {
                                if let Some(block) = &call.block {
                                    Ok(self
                                        .analyze_block(block, cache_config, document, deps)
                                        .await?
                                        .events)
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            SET_DEFAULTS => {
                                if let Some(block) = &call.block {
                                    let analyzed_root = self
                                        .analyze_block(block, cache_config, document, deps)
                                        .await?;
                                    Ok(vec![AnalyzedEvent::NewScope(analyzed_root)])
                                } else {
                                    Ok(Vec::new())
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
                                    return Ok(strings
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
                                        .collect());
                                }
                                Ok(Vec::new())
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
                                    events.push(AnalyzedEvent::NewScope(
                                        self.analyze_block(block, cache_config, document, deps)
                                            .await?,
                                    ));
                                }
                                Ok(events)
                            }
                        }
                    }
                    Statement::Condition(condition) => {
                        let mut events = Vec::new();
                        let mut condition_blocks = Vec::new();
                        let mut current_condition = condition;
                        loop {
                            events.extend(
                                self.analyze_expr(
                                    &current_condition.condition,
                                    cache_config,
                                    document,
                                    deps,
                                )
                                .await?,
                            );
                            condition_blocks.push(
                                self.analyze_block(
                                    &current_condition.then_block,
                                    cache_config,
                                    document,
                                    deps,
                                )
                                .await?,
                            );
                            match &current_condition.else_block {
                                None => break,
                                Some(Either::Left(next_condition)) => {
                                    current_condition = next_condition;
                                }
                                Some(Either::Right(block)) => {
                                    condition_blocks.push(
                                        self.analyze_block(block, cache_config, document, deps)
                                            .await?,
                                    );
                                    break;
                                }
                            }
                        }
                        events.push(AnalyzedEvent::Conditions(condition_blocks));
                        Ok(events)
                    }
                    Statement::Error(_) => Ok(Vec::new()),
                }
            },
        ))
        .await
        .into_iter()
        .flatten_ok()
        .collect()?;

        Ok(AnalyzedBlock {
            events,
            span: block.span,
        })
    }

    async fn analyze_expr<'i, 'p>(
        &self,
        expr: &'p Expr<'i>,
        cache_config: CacheConfig,
        document: &'i Document,
        deps: &mut Vec<Pin<Arc<ShallowAnalyzedFile>>>,
    ) -> Result<Vec<AnalyzedEvent<'i, 'p>>> {
        match expr {
            Expr::Primary(primary_expr) => match primary_expr.as_ref() {
                PrimaryExpr::Block(block) => {
                    let analyzed_root = self
                        .analyze_block(block, cache_config, document, deps)
                        .await?;
                    Ok(vec![AnalyzedEvent::NewScope(analyzed_root)])
                }
                PrimaryExpr::Call(call) => {
                    let mut events: Vec<AnalyzedEvent> = join_all(call.args.iter().map(|expr| {
                        Box::pin(self.analyze_expr(expr, cache_config, document, deps))
                    }))
                    .await
                    .into_iter()
                    .flatten_ok()
                    .collect::<Result<Vec<_>>>()?;
                    if let Some(block) = &call.block {
                        let analyzed_root = self
                            .analyze_block(block, cache_config, document, deps)
                            .await?;
                        events.push(AnalyzedEvent::NewScope(analyzed_root));
                    }
                    Ok(events)
                }
                PrimaryExpr::ParenExpr(paren_expr) => {
                    self.analyze_expr(&paren_expr.expr, cache_config, document, deps)
                        .await
                }
                PrimaryExpr::List(list_literal) => {
                    Ok(join_all(list_literal.values.iter().map(async move |expr| {
                        Box::pin(self.analyze_expr(expr, cache_config, document, deps)).await
                    }))
                    .await
                    .into_iter()
                    .flatten_ok()
                    .collect::<Result<Vec<_>>>()?)
                }
                PrimaryExpr::Identifier(_)
                | PrimaryExpr::Integer(_)
                | PrimaryExpr::String(_)
                | PrimaryExpr::ArrayAccess(_)
                | PrimaryExpr::ScopeAccess(_)
                | PrimaryExpr::Error(_) => Ok(Vec::new()),
            },
            Expr::Unary(unary_expr) => {
                Box::pin(self.analyze_expr(&unary_expr.expr, cache_config, document, deps)).await
            }
            Expr::Binary(binary_expr) => {
                let mut events =
                    Box::pin(self.analyze_expr(&binary_expr.lhs, cache_config, document, deps))
                        .await?;
                events.extend(
                    Box::pin(self.analyze_expr(&binary_expr.rhs, cache_config, document, deps))
                        .await?,
                );
                Ok(events)
            }
        }
    }
}
