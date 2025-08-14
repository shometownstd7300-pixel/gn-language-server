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
    collections::{BTreeSet, HashMap},
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use pest::Span;
use tower_lsp::lsp_types::{Diagnostic, DocumentSymbol};

use crate::{
    analyzer::{cache::AnalysisNode, utils::resolve_path},
    common::storage::{Document, DocumentVersion},
    parser::{parse, Block, Call, Comments, Node, Statement},
};

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct PathSpan<'i> {
    pub path: &'i Path,
    pub span: Span<'i>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceContext {
    pub root: PathBuf,
    pub dot_gn_version: DocumentVersion,
    pub build_config: PathBuf,
}

impl WorkspaceContext {
    pub fn resolve_path(&self, name: &str, current_dir: &Path) -> PathBuf {
        resolve_path(name, &self.root, current_dir)
    }
}

#[derive(Clone)]
pub struct Scope<'i, T> {
    imports: Vec<Arc<Scope<'i, T>>>,
    locals: HashMap<&'i str, T>,
}

impl<T> Default for Scope<'_, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'i, T> Scope<'i, T> {
    pub fn new() -> Self {
        Self {
            imports: Vec::new(),
            locals: HashMap::new(),
        }
    }

    pub fn locals(&self) -> &HashMap<&'i str, T> {
        &self.locals
    }

    pub fn get(&self, name: &str) -> Option<&T> {
        self.locals
            .get(name)
            .or_else(|| self.imports.iter().find_map(|import| import.get(name)))
    }

    pub fn import(&mut self, other: &Arc<Scope<'i, T>>) {
        self.imports.push(Arc::clone(other));
    }

    pub fn insert(&mut self, name: &'i str, item: T) {
        self.locals.insert(name, item);
    }

    pub fn ensure(&mut self, name: &'i str, f: impl FnOnce() -> T) -> &mut T {
        self.locals.entry(name).or_insert_with(f)
    }

    pub fn merge(&mut self, other: Scope<'i, T>) {
        for (name, item) in other.locals {
            self.insert(name, item);
        }
        self.imports.extend(other.imports);
    }
}

impl<'i, T> Scope<'i, T>
where
    T: Clone,
{
    pub fn all_items(&self) -> HashMap<&'i str, T> {
        let mut items = HashMap::new();
        self.collect_items(&mut items, &mut Default::default());
        items
    }

    fn collect_items<'e>(
        &'e self,
        items: &mut HashMap<&'i str, T>,
        visited: &mut BTreeSet<*const Self>,
    ) {
        if !visited.insert(self as *const Self) {
            return;
        }
        for import in &self.imports {
            import.collect_items(items, visited);
        }
        for (name, item) in &self.locals {
            items.insert(name, item.clone());
        }
    }
}

pub struct ShallowAnalyzedFile {
    pub document: Pin<Arc<Document>>,
    #[allow(unused)] // Backing analyzed_root
    pub ast_root: Pin<Box<Block<'static>>>,
    pub analyzed_root: ShallowAnalyzedBlock<'static, 'static>,
    pub links: Vec<AnalyzedLink<'static>>,
    pub node: Arc<AnalysisNode>,
}

impl ShallowAnalyzedFile {
    pub fn new(
        document: Pin<Arc<Document>>,
        ast_root: Pin<Box<Block<'static>>>,
        analyzed_root: ShallowAnalyzedBlock<'static, 'static>,
        links: Vec<AnalyzedLink<'static>>,
        deps: Vec<Arc<AnalysisNode>>,
        request_time: Instant,
    ) -> Pin<Arc<Self>> {
        let node = Arc::new(AnalysisNode::new(
            document.path.clone(),
            document.version,
            deps,
            request_time,
        ));
        Arc::pin(Self {
            document,
            ast_root,
            analyzed_root,
            links,
            node,
        })
    }

    pub fn error(path: &Path, request_time: Instant) -> Pin<Arc<Self>> {
        let document = Arc::pin(Document::analysis_error(path));
        let ast_root = Box::pin(parse(&document.data));
        let analyzed_root = ShallowAnalyzedBlock::new_top_level();
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };
        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root = unsafe {
            std::mem::transmute::<ShallowAnalyzedBlock, ShallowAnalyzedBlock>(analyzed_root)
        };
        Self::new(
            document,
            ast_root,
            analyzed_root,
            Vec::new(),
            Vec::new(),
            request_time,
        )
    }
}

#[derive(Default)]
pub struct ShallowAnalyzedBlock<'i, 'p> {
    pub variables: Arc<AnalyzedVariableScope<'i, 'p>>,
    pub templates: Arc<AnalyzedTemplateScope<'i>>,
    pub targets: Arc<AnalyzedTargetScope<'i, 'p>>,
}

impl ShallowAnalyzedBlock<'_, '_> {
    pub fn new_top_level() -> Self {
        Default::default()
    }
}

pub struct AnalyzedFile {
    pub document: Pin<Arc<Document>>,
    pub workspace_root: PathBuf,
    pub ast_root: Pin<Box<Block<'static>>>,
    pub analyzed_root: AnalyzedBlock<'static, 'static>,
    pub links: Vec<AnalyzedLink<'static>>,
    pub symbols: Vec<DocumentSymbol>,
    pub diagnostics: Vec<Diagnostic>,
    pub node: Arc<AnalysisNode>,
}

impl AnalyzedFile {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        document: Pin<Arc<Document>>,
        workspace_root: PathBuf,
        ast_root: Pin<Box<Block<'static>>>,
        analyzed_root: AnalyzedBlock<'static, 'static>,
        links: Vec<AnalyzedLink<'static>>,
        symbols: Vec<DocumentSymbol>,
        diagnostics: Vec<Diagnostic>,
        deps: Vec<Arc<AnalysisNode>>,
        request_time: Instant,
    ) -> Pin<Arc<Self>> {
        let node = Arc::new(AnalysisNode::new(
            document.path.clone(),
            document.version,
            deps,
            request_time,
        ));

        Arc::pin(Self {
            document,
            workspace_root,
            ast_root,
            analyzed_root,
            links,
            symbols,
            diagnostics,
            node,
        })
    }

    pub fn variables_at(&self, pos: usize) -> AnalyzedVariableScope {
        self.analyzed_root.variables_at(pos)
    }

    pub fn templates_at(&self, pos: usize) -> AnalyzedTemplateScope {
        self.analyzed_root.templates_at(pos)
    }
}

pub struct AnalyzedBlock<'i, 'p> {
    pub events: Vec<AnalyzedEvent<'i, 'p>>,
    pub span: Span<'i>,
}

impl<'i, 'p> AnalyzedBlock<'i, 'p> {
    pub fn top_level_events<'a>(&'a self) -> TopLevelEvents<'i, 'p, 'a> {
        TopLevelEvents::new(&self.events)
    }

    pub fn targets(&self) -> impl Iterator<Item = &AnalyzedTarget<'i, 'p>> {
        self.top_level_events().filter_map(|event| match event {
            AnalyzedEvent::Target(target) => Some(target),
            _ => None,
        })
    }

    pub fn variables_at(&self, pos: usize) -> AnalyzedVariableScope<'i, 'p> {
        let mut variables = AnalyzedVariableScope::new();

        // First pass: Collect all variables in the scope.
        let mut declare_args_stack: Vec<&AnalyzedBlock> = Vec::new();
        for event in self.top_level_events() {
            match event {
                AnalyzedEvent::Assignment(assignment) => {
                    while let Some(last_declare_args) = declare_args_stack.last() {
                        if assignment.statement.span().end_pos() <= last_declare_args.span.end_pos()
                        {
                            break;
                        }
                        declare_args_stack.pop();
                    }
                    variables
                        .ensure(assignment.name, || AnalyzedVariable {
                            assignments: Default::default(),
                            is_args: !declare_args_stack.is_empty(),
                        })
                        .assignments
                        .insert(
                            PathSpan {
                                path: &assignment.document.path,
                                span: assignment.variable_span,
                            },
                            assignment.clone(),
                        );
                }
                AnalyzedEvent::Import(import) => {
                    // TODO: Handle import() within declare_args.
                    variables.import(&import.file.analyzed_root.variables);
                }
                AnalyzedEvent::DeclareArgs(block) => {
                    declare_args_stack.push(block);
                }
                _ => {}
            }
        }

        // Second pass: Find the subscope that contains the position, and merge
        // its variables.
        for event in self.top_level_events() {
            if let AnalyzedEvent::NewScope(block) = event {
                if block.span.start() < pos && pos < block.span.end() {
                    variables.merge(block.variables_at(pos));
                }
            }
        }

        variables
    }

    pub fn templates_at(&self, pos: usize) -> AnalyzedTemplateScope<'i> {
        let mut templates = AnalyzedTemplateScope::new();

        // First pass: Collect all templates in the scope.
        for event in self.top_level_events() {
            match event {
                AnalyzedEvent::Template(template) => {
                    templates.insert(template.name, template.clone());
                }
                AnalyzedEvent::Import(import) => {
                    templates.import(&import.file.analyzed_root.templates);
                }
                _ => {}
            }
        }

        // Second pass: Find the subscope that contains the position, and merge
        // its templates.
        for event in self.top_level_events() {
            if let AnalyzedEvent::NewScope(block) = event {
                if block.span.start() < pos && pos < block.span.end() {
                    templates.merge(block.templates_at(pos));
                }
            }
        }

        templates
    }
}

pub struct TopLevelEvents<'i, 'p, 'a> {
    stack: Vec<&'a AnalyzedEvent<'i, 'p>>,
}

impl<'i, 'p, 'a> TopLevelEvents<'i, 'p, 'a> {
    pub fn new<I>(events: impl IntoIterator<Item = &'a AnalyzedEvent<'i, 'p>, IntoIter = I>) -> Self
    where
        I: DoubleEndedIterator<Item = &'a AnalyzedEvent<'i, 'p>>,
    {
        TopLevelEvents {
            stack: events.into_iter().rev().collect(),
        }
    }
}

impl<'i, 'p, 'a> Iterator for TopLevelEvents<'i, 'p, 'a> {
    type Item = &'a AnalyzedEvent<'i, 'p>;

    fn next(&mut self) -> Option<Self::Item> {
        let event = self.stack.pop()?;
        match event {
            AnalyzedEvent::Conditions(blocks) => {
                self.stack
                    .extend(blocks.iter().flat_map(|block| &block.events).rev());
            }
            AnalyzedEvent::DeclareArgs(block) => {
                self.stack.extend(block.events.iter().rev());
            }
            AnalyzedEvent::Import(_)
            | AnalyzedEvent::Assignment(_)
            | AnalyzedEvent::Template(_)
            | AnalyzedEvent::Target(_)
            | AnalyzedEvent::NewScope(_) => {}
        }
        Some(event)
    }
}

pub enum AnalyzedEvent<'i, 'p> {
    Conditions(Vec<AnalyzedBlock<'i, 'p>>),
    Import(AnalyzedImport),
    DeclareArgs(AnalyzedBlock<'i, 'p>),
    Assignment(AnalyzedAssignment<'i, 'p>),
    Template(AnalyzedTemplate<'i>),
    Target(AnalyzedTarget<'i, 'p>),
    NewScope(AnalyzedBlock<'i, 'p>),
}

pub struct AnalyzedImport {
    pub file: Pin<Arc<ShallowAnalyzedFile>>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct AnalyzedAssignment<'i, 'p> {
    pub name: &'i str,
    pub comments: Comments<'i>,
    pub statement: &'p Statement<'i>,
    pub document: &'i Document,
    pub variable_span: Span<'i>,
}

#[derive(Clone, Default)]
pub struct AnalyzedVariable<'i, 'p> {
    pub assignments: HashMap<PathSpan<'i>, AnalyzedAssignment<'i, 'p>>,
    pub is_args: bool,
}

pub type AnalyzedVariableScope<'i, 'p> = Scope<'i, AnalyzedVariable<'i, 'p>>;

#[derive(Clone, Eq, PartialEq)]
pub struct AnalyzedTemplate<'i> {
    pub name: &'i str,
    pub comments: Comments<'i>,
    pub document: &'i Document,
    pub header: Span<'i>,
    pub span: Span<'i>,
}

pub type AnalyzedTemplateScope<'i> = Scope<'i, AnalyzedTemplate<'i>>;

#[derive(Clone, Eq, PartialEq)]
pub struct AnalyzedTarget<'i, 'p> {
    pub name: &'i str,
    pub call: &'p Call<'i>,
    pub document: &'i Document,
    pub header: Span<'i>,
    pub span: Span<'i>,
}

pub type AnalyzedTargetScope<'i, 'p> = Scope<'i, AnalyzedTarget<'i, 'p>>;

#[derive(Clone, Eq, PartialEq)]
pub enum AnalyzedLink<'i> {
    /// Link to a file. No range is specified.
    File { path: PathBuf, span: Span<'i> },
    /// Link to a target defined in a BUILD.gn file.
    Target {
        path: PathBuf,
        name: &'i str,
        span: Span<'i>,
    },
}

impl<'i> AnalyzedLink<'i> {
    pub fn span(&self) -> Span<'i> {
        match self {
            AnalyzedLink::File { span, .. } => *span,
            AnalyzedLink::Target { span, .. } => *span,
        }
    }
}
