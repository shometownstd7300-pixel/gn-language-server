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
    fmt::Write,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use either::Either;
use tokio::sync::{Mutex, RwLock};

use crate::{
    analyze::{
        data::{
            AnalyzedAssignment, AnalyzedTarget, AnalyzedTemplate, AnalyzedVariable,
            MutableShallowAnalyzedBlock, ShallowAnalyzedBlock, ShallowAnalyzedFile,
            WorkspaceContext,
        },
        links::collect_links,
        utils::{compute_next_check, FreshCache},
        AnalyzedLink,
    },
    ast::{parse, Block, Comments, LValue, Node, Statement},
    builtins::{DECLARE_ARGS, FOREACH, FORWARD_VARIABLES_FROM, IMPORT, SET_DEFAULTS, TEMPLATE},
    error::{Error, Result},
    storage::{Document, DocumentStorage},
    utils::{parse_simple_literal, CacheConfig},
};

fn is_exported(name: &str) -> bool {
    !name.starts_with("_")
}

fn make_loop_error(cycle: &[PathBuf]) -> Error {
    let mut message = String::new();
    write!(&mut message, "Cycle detected: ").ok();
    for (i, path) in cycle.iter().enumerate() {
        if i > 0 {
            write!(&mut message, " -> ").ok();
        }
        write!(&mut message, "{}", path.to_string_lossy()).ok();
    }
    Error::General(message)
}

pub struct ShallowAnalyzer {
    context: WorkspaceContext,
    storage: Arc<Mutex<DocumentStorage>>,
    cache: FreshCache<PathBuf, Pin<Arc<ShallowAnalyzedFile>>>,
}

impl ShallowAnalyzer {
    pub fn new(context: &WorkspaceContext, storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            context: context.clone(),
            storage: storage.clone(),
            cache: FreshCache::new(),
        }
    }

    pub async fn cached_files(&self) -> Vec<Pin<Arc<ShallowAnalyzedFile>>> {
        self.cache.ok_values().await
    }

    pub async fn analyze(
        &self,
        path: &Path,
        cache_config: CacheConfig,
    ) -> Result<Pin<Arc<ShallowAnalyzedFile>>> {
        self.analyze_cached(path, cache_config, &mut Vec::new())
            .await
    }

    async fn analyze_cached(
        &self,
        path: &Path,
        cache_config: CacheConfig,
        visiting: &mut Vec<PathBuf>,
    ) -> Result<Pin<Arc<ShallowAnalyzedFile>>> {
        self.cache
            .get_or_insert(
                path.to_path_buf(),
                async |file| {
                    file.is_fresh(cache_config, &*self.storage.lock().await)
                        .await
                },
                async || self.analyze_uncached(path, cache_config, visiting).await,
            )
            .await
    }

    async fn analyze_uncached(
        &self,
        path: &Path,
        cache_config: CacheConfig,
        visiting: &mut Vec<PathBuf>,
    ) -> Result<Pin<Arc<ShallowAnalyzedFile>>> {
        if visiting.iter().any(|p| p == path) {
            return Err(make_loop_error(visiting));
        }

        visiting.push(path.to_path_buf());
        let result = self
            .analyze_uncached_inner(path, cache_config, visiting)
            .await;
        visiting.pop();
        result
    }

    async fn analyze_uncached_inner(
        &self,
        path: &Path,
        cache_config: CacheConfig,
        visiting: &mut Vec<PathBuf>,
    ) -> Result<Pin<Arc<ShallowAnalyzedFile>>> {
        let document = match self.storage.lock().await.read(path) {
            Ok(document) => document,
            Err(err) if err.is_not_found() => {
                // Ignore missing imports as they might be imported conditionally.
                return Ok(ShallowAnalyzedFile::empty(path));
            }
            Err(err) => return Err(err),
        };
        let ast_root = Box::pin(parse(&document.data));
        let mut deps = Vec::new();
        let analyzed_root = self
            .analyze_block(&ast_root, cache_config, &document, &mut deps, visiting)
            .await?;

        let links = collect_links(&ast_root, path, &self.context);

        // SAFETY: links' contents are backed by pinned document.
        let links = unsafe { std::mem::transmute::<Vec<AnalyzedLink>, Vec<AnalyzedLink>>(links) };
        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root = unsafe {
            std::mem::transmute::<ShallowAnalyzedBlock, ShallowAnalyzedBlock>(analyzed_root)
        };
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };
        let next_check = RwLock::new(compute_next_check(Instant::now(), document.version));

        Ok(Arc::pin(ShallowAnalyzedFile {
            document,
            ast_root,
            analyzed_root,
            deps,
            links,
            next_check,
        }))
    }

    async fn analyze_block<'i, 'p>(
        &self,
        block: &'p Block<'i>,
        cache_config: CacheConfig,
        document: &'i Document,
        deps: &mut Vec<Pin<Arc<ShallowAnalyzedFile>>>,
        visiting: &mut Vec<PathBuf>,
    ) -> Result<ShallowAnalyzedBlock<'i, 'p>> {
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
                            let file = Box::pin(self.analyze_cached(&path, cache_config, visiting))
                                .await?;
                            analyzed_block.import(&file.analyzed_root);
                            deps.push(file);
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
                            analyzed_block.merge(
                                &Box::pin(self.analyze_block(
                                    block,
                                    cache_config,
                                    document,
                                    deps,
                                    visiting,
                                ))
                                .await?,
                            );
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
                        analyzed_block.merge(
                            &Box::pin(self.analyze_block(
                                &current_condition.then_block,
                                cache_config,
                                document,
                                deps,
                                visiting,
                            ))
                            .await?,
                        );
                        match &current_condition.else_block {
                            None => break,
                            Some(Either::Left(next_condition)) => {
                                current_condition = next_condition;
                            }
                            Some(Either::Right(block)) => {
                                analyzed_block.merge(
                                    &Box::pin(self.analyze_block(
                                        block,
                                        cache_config,
                                        document,
                                        deps,
                                        visiting,
                                    ))
                                    .await?,
                                );
                                break;
                            }
                        }
                    }
                }
                Statement::Error(_) => {}
            }
        }

        Ok(analyzed_block.finalize())
    }
}
