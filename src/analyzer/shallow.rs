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
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Instant,
};

use either::Either;

use crate::{
    analyzer::{
        cache::CacheNode,
        data::{
            FileEnvironment, MutableFileEnvironment, PathSpan, ShallowAnalyzedFile, Target,
            Template, Variable, VariableAssignment, WorkspaceContext,
        },
        links::collect_links,
        toplevel::TopLevelStatementsExt,
        AnalyzedLink,
    },
    common::{
        builtins::{DECLARE_ARGS, FOREACH, FORWARD_VARIABLES_FROM, IMPORT, SET_DEFAULTS, TEMPLATE},
        storage::{Document, DocumentStorage},
        utils::parse_simple_literal,
    },
    parser::{parse, Block, Call, Comments, LValue, Node, Statement},
};

fn is_exported(name: &str) -> bool {
    !name.starts_with("_")
}

pub type ShallowAnalysisSnapshot = HashMap<PathBuf, Pin<Arc<ShallowAnalyzedFile>>>;

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
            cache: Default::default(),
        }
    }

    pub fn cached_files(&self) -> Vec<Pin<Arc<ShallowAnalyzedFile>>> {
        self.cache.values().cloned().collect()
    }

    pub fn analyze(
        &mut self,
        path: &Path,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
    ) -> Pin<Arc<ShallowAnalyzedFile>> {
        if snapshot.contains_key(path) {
            return snapshot.get(path).unwrap().clone();
        }

        if let Some(cached_file) = self.cache.get(path) {
            if cached_file
                .node
                .verify(request_time, &self.storage.lock().unwrap())
            {
                return cached_file.clone();
            }
        }

        self.analyze_cached(path, request_time, snapshot, &mut Vec::new())
    }

    fn analyze_cached(
        &mut self,
        path: &Path,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
        visiting: &mut Vec<PathBuf>,
    ) -> Pin<Arc<ShallowAnalyzedFile>> {
        if visiting.iter().any(|p| p == path) {
            return ShallowAnalyzedFile::error(path, request_time);
        }

        if snapshot.contains_key(path) {
            return snapshot.get(path).unwrap().clone();
        }
        let file = self.analyze_cached_inner(path, request_time, snapshot, visiting);
        assert!(!snapshot.contains_key(path));
        snapshot.insert(path.to_path_buf(), file.clone());
        file
    }

    fn analyze_cached_inner(
        &mut self,
        path: &Path,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
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

        let new_file = self.analyze_uncached(path, request_time, snapshot, visiting);
        self.cache.insert(path.to_path_buf(), new_file.clone());
        new_file
    }

    fn analyze_uncached(
        &mut self,
        path: &Path,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
        visiting: &mut Vec<PathBuf>,
    ) -> Pin<Arc<ShallowAnalyzedFile>> {
        visiting.push(path.to_path_buf());
        let new_file = self.analyze_uncached_inner(path, request_time, snapshot, visiting);
        visiting.pop();
        new_file
    }

    fn analyze_uncached_inner(
        &mut self,
        path: &Path,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
        visiting: &mut Vec<PathBuf>,
    ) -> Pin<Arc<ShallowAnalyzedFile>> {
        let document = self.storage.lock().unwrap().read(path);
        let ast = Box::pin(parse(&document.data));
        let mut deps = Vec::new();
        let environment =
            self.analyze_block(&ast, &document, request_time, snapshot, &mut deps, visiting);

        let links = collect_links(&ast, path, &self.context);

        // SAFETY: links' contents are backed by pinned document.
        let links = unsafe { std::mem::transmute::<Vec<AnalyzedLink>, Vec<AnalyzedLink>>(links) };
        // SAFETY: environment's contents are backed by pinned document and pinned ast.
        let environment =
            unsafe { std::mem::transmute::<FileEnvironment, FileEnvironment>(environment) };
        // SAFETY: ast's contents are backed by pinned document.
        let ast = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast) };

        ShallowAnalyzedFile::new(document, ast, environment, links, deps, request_time)
    }

    #[allow(clippy::too_many_arguments)]
    fn analyze_block<'i, 'p>(
        &mut self,
        block: &'p Block<'i>,
        document: &'i Document,
        request_time: Instant,
        snapshot: &mut ShallowAnalysisSnapshot,
        deps: &mut Vec<Arc<CacheNode>>,
        visiting: &mut Vec<PathBuf>,
    ) -> FileEnvironment<'i, 'p> {
        let mut environment = MutableFileEnvironment::new();
        let mut declare_args_stack: Vec<&Call> = Vec::new();

        for statement in block.top_level_statements() {
            while let Some(last_declare_args) = declare_args_stack.last() {
                if statement.span().start_pos() <= last_declare_args.span.end_pos() {
                    break;
                }
                declare_args_stack.pop();
            }
            match statement {
                Statement::Assignment(assignment) => {
                    let identifier = match &assignment.lvalue {
                        LValue::Identifier(identifier) => identifier,
                        LValue::ArrayAccess(array_access) => &array_access.array,
                        LValue::ScopeAccess(scope_access) => &scope_access.scope,
                    };
                    if is_exported(identifier.name) {
                        environment
                            .variables
                            .ensure(identifier.name, || {
                                Variable::new(!declare_args_stack.is_empty())
                            })
                            .assignments
                            .insert(
                                PathSpan {
                                    path: &document.path,
                                    span: identifier.span,
                                },
                                VariableAssignment {
                                    document,
                                    assignment_or_call: Either::Left(assignment),
                                    primary_variable: identifier.span,
                                    comments: assignment.comments.clone(),
                                },
                            );
                    }
                }
                Statement::Call(call) => match call.function.name {
                    IMPORT => {
                        if let Some(name) = call.only_arg().and_then(|expr| expr.as_simple_string())
                        {
                            let path = self
                                .context
                                .resolve_path(name, document.path.parent().unwrap());
                            let file = self.analyze_cached(&path, request_time, snapshot, visiting);
                            environment.import(&file.environment);
                            deps.push(file.node.clone());
                        }
                    }
                    TEMPLATE => {
                        if let Some(name) = call.only_arg().and_then(|expr| expr.as_simple_string())
                        {
                            if is_exported(name) {
                                environment.templates.insert(
                                    name,
                                    Template {
                                        document,
                                        call,
                                        name,
                                        comments: call.comments.clone(),
                                    },
                                );
                            }
                        }
                    }
                    DECLARE_ARGS => {
                        declare_args_stack.push(call);
                    }
                    FOREACH | SET_DEFAULTS => {}
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
                                        environment
                                            .variables
                                            .ensure(name, || {
                                                Variable::new(!declare_args_stack.is_empty())
                                            })
                                            .assignments
                                            .insert(
                                                PathSpan {
                                                    path: &document.path,
                                                    span: string.span,
                                                },
                                                VariableAssignment {
                                                    document,
                                                    assignment_or_call: Either::Right(call),
                                                    primary_variable: string.span,
                                                    comments: Comments::default(),
                                                },
                                            );
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some(name) = call.only_arg().and_then(|expr| expr.as_simple_string())
                        {
                            environment.targets.insert(
                                name,
                                Target {
                                    document,
                                    call,
                                    name,
                                },
                            );
                        }
                    }
                },
                Statement::Condition(_) | Statement::Error(_) => {}
            }
        }

        environment.finalize()
    }
}
