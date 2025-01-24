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
    collections::{BTreeMap, HashMap, HashSet},
    io::ErrorKind,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
};

use either::Either;
use itertools::Itertools;
use pest::Span;
use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};

use crate::{
    ast::{parse, AssignOp, Block, Comments, Expr, LValue, Node, PrimaryExpr, Statement},
    storage::{Document, DocumentStorage, DocumentVersion},
    util::{parse_simple_literal, LineIndex},
};

fn is_exported(name: &str) -> bool {
    !name.starts_with("_")
}

pub fn find_workspace_root(path: &Path) -> std::io::Result<PathBuf> {
    for dir in path.ancestors().skip(1) {
        if dir.join(".gn").try_exists()? {
            return Ok(dir.to_path_buf());
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("Workspace not found for {}", path.to_string_lossy()),
    ))
}

fn resolve_path(name: &str, root_dir: &Path, current_dir: &Path) -> PathBuf {
    if let Some(rest) = name.strip_prefix("//") {
        root_dir.join(rest)
    } else {
        current_dir.join(name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceContext {
    pub root: PathBuf,
    pub build_config: PathBuf,
}

impl WorkspaceContext {
    pub fn resolve_path(&self, name: &str, current_dir: &Path) -> PathBuf {
        resolve_path(name, &self.root, current_dir)
    }

    pub fn format_path(&self, path: &Path) -> String {
        if let Ok(relative_path) = path.strip_prefix(&self.root) {
            format!("//{}", relative_path.to_string_lossy())
        } else {
            path.to_string_lossy().to_string()
        }
    }
}

fn evaluate_dot_gn(workspace_root: &Path, input: &str) -> std::io::Result<PathBuf> {
    let line_index = LineIndex::new(input);
    let ast_root = parse(input);

    let mut build_config_path: Option<PathBuf> = None;

    for statement in &ast_root.statements {
        let Statement::Assignment(assignment) = statement else {
            continue;
        };
        if !matches!(&assignment.lvalue, LValue::Identifier(identifier) if identifier.name == "buildconfig")
        {
            continue;
        }

        let position = line_index.position(assignment.span.start());

        if assignment.op != AssignOp::Assign {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}:{}:{}: buildconfig must be assigned exactly once",
                    workspace_root.join(".gn").to_string_lossy(),
                    position.line + 1,
                    position.character + 1
                ),
            ));
        }
        let Some(string) = assignment.rvalue.as_primary_string() else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}:{}:{}: buildconfig is not a simple string",
                    workspace_root.join(".gn").to_string_lossy(),
                    position.line + 1,
                    position.character + 1
                ),
            ));
        };
        let Some(name) = parse_simple_literal(string.raw_value) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}:{}:{}: buildconfig is not a simple string",
                    workspace_root.join(".gn").to_string_lossy(),
                    position.line + 1,
                    position.character + 1
                ),
            ));
        };

        if build_config_path
            .replace(resolve_path(name, workspace_root, workspace_root))
            .is_some()
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "{}:{}:{}: buildconfig is assigned multiple times",
                    workspace_root.join(".gn").to_string_lossy(),
                    position.line + 1,
                    position.character + 1
                ),
            ));
        }
    }

    let Some(build_config_path) = build_config_path else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{}: buildconfig is not assigned directly",
                workspace_root.join(".gn").to_string_lossy()
            ),
        ));
    };

    Ok(build_config_path)
}

pub struct AnalyzedFile {
    pub document: Pin<Arc<Document>>,
    pub workspace: WorkspaceContext,
    pub ast_root: Pin<Box<Block<'static>>>,
    pub analyzed_root: AnalyzedBlock<'static, 'static>,
    pub deps: Vec<Arc<ThinAnalyzedFile>>,
    pub links: Vec<Link<'static>>,
    pub symbols: Vec<DocumentSymbol>,
}

impl AnalyzedFile {
    pub fn is_fresh(&self, storage: &DocumentStorage) -> std::io::Result<bool> {
        if self.document.will_check() {
            let version = storage.read_version(&self.document.path)?;
            if version != self.document.version {
                return Ok(false);
            }
        }
        for dep in &self.deps {
            if !dep.is_fresh(storage)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub fn scope_at(&self, pos: usize) -> AnalyzedScope {
        self.analyzed_root.scope_at(pos, None)
    }

    pub fn templates_at(&self, pos: usize) -> HashSet<&AnalyzedTemplate> {
        self.analyzed_root.templates_at(pos)
    }

    pub fn targets_at(&self, pos: usize) -> HashSet<&AnalyzedTarget> {
        self.analyzed_root.targets_at(pos)
    }
}

pub struct ThinAnalyzedFile {
    pub document: Pin<Arc<Document>>,
    pub workspace: WorkspaceContext,
    #[allow(unused)] // Backing analyzed_root
    pub ast_root: Pin<Box<Block<'static>>>,
    pub analyzed_root: ThinAnalyzedBlock<'static, 'static>,
    pub deps: Vec<Arc<ThinAnalyzedFile>>,
}

impl ThinAnalyzedFile {
    pub fn empty(path: &Path, workspace: &WorkspaceContext) -> Arc<Self> {
        let document = Arc::pin(Document::empty(path));
        let ast_root = Box::pin(parse(&document.data));
        let analyzed_root = ThinAnalyzedBlock::new_top_level();
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };
        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root =
            unsafe { std::mem::transmute::<ThinAnalyzedBlock, ThinAnalyzedBlock>(analyzed_root) };
        Arc::new(ThinAnalyzedFile {
            document,
            workspace: workspace.clone(),
            ast_root,
            analyzed_root,
            deps: Vec::new(),
        })
    }

    pub fn is_fresh(&self, storage: &DocumentStorage) -> std::io::Result<bool> {
        if self.document.will_check() {
            let version = storage.read_version(&self.document.path)?;
            if version != self.document.version {
                return Ok(false);
            }
        }
        for dep in &self.deps {
            if !dep.is_fresh(storage)? {
                return Ok(false);
            }
        }
        Ok(true)
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

    pub fn scope_at(
        &self,
        pos: usize,
        parent: Option<Box<AnalyzedScope<'i, 'p>>>,
    ) -> AnalyzedScope<'i, 'p> {
        let mut scope = AnalyzedScope::new(parent);

        // First pass: Collect all variables in the scope.
        for event in self.top_level_events() {
            match event {
                AnalyzedEvent::Assignment(assignment) => {
                    scope.insert(assignment.clone());
                }
                AnalyzedEvent::Import(import) => {
                    scope.merge(&import.file.analyzed_root.scope);
                }
                _ => {}
            }
        }

        // Second pass: Find the subscope that contains the position.
        for event in self.top_level_events() {
            if let AnalyzedEvent::NewScope(block) = event {
                if block.span.start() < pos && pos < block.span.end() {
                    return block.scope_at(pos, Some(Box::new(scope)));
                }
            }
        }

        scope
    }

    pub fn templates_at(&'i self, pos: usize) -> HashSet<&'i AnalyzedTemplate<'i>> {
        let mut templates = HashSet::new();
        for event in &self.events {
            match event {
                AnalyzedEvent::Conditions(blocks) => {
                    if blocks.last().unwrap().span.end() <= pos {
                        for block in blocks {
                            templates.extend(block.templates_at(pos));
                        }
                    } else {
                        for block in blocks {
                            if block.span.start() <= pos && pos <= block.span.end() {
                                templates.extend(block.templates_at(pos));
                            }
                        }
                    }
                }
                AnalyzedEvent::Import(import) => {
                    if import.span.end() <= pos {
                        templates.extend(import.file.analyzed_root.templates.iter());
                    }
                }
                AnalyzedEvent::Template(template) => {
                    if template.span.end() <= pos {
                        templates.insert(template);
                    }
                }
                AnalyzedEvent::NewScope(block) => {
                    if block.span.start() <= pos && pos <= block.span.end() {
                        templates.extend(block.templates_at(pos));
                    }
                }
                AnalyzedEvent::Assignment(_)
                | AnalyzedEvent::DeclareArgs(_)
                | AnalyzedEvent::Target(_) => {}
            }
        }
        templates
    }

    pub fn targets_at(&'i self, pos: usize) -> HashSet<&'i AnalyzedTarget<'i>> {
        let mut targets = HashSet::new();
        for event in &self.events {
            match event {
                AnalyzedEvent::Conditions(blocks) => {
                    if blocks.last().unwrap().span.end() <= pos {
                        for block in blocks {
                            targets.extend(block.targets_at(pos));
                        }
                    } else {
                        for block in blocks {
                            if block.span.start() <= pos && pos <= block.span.end() {
                                targets.extend(block.targets_at(pos));
                            }
                        }
                    }
                }
                AnalyzedEvent::Import(import) => {
                    if import.span.end() <= pos {
                        targets.extend(import.file.analyzed_root.targets.iter());
                    }
                }
                AnalyzedEvent::Target(target) => {
                    if target.span.end() <= pos {
                        targets.insert(target);
                    }
                }
                AnalyzedEvent::NewScope(block) => {
                    if block.span.start() <= pos && pos <= block.span.end() {
                        targets.extend(block.targets_at(pos));
                    }
                }
                AnalyzedEvent::Assignment(_)
                | AnalyzedEvent::DeclareArgs(_)
                | AnalyzedEvent::Template(_) => {}
            }
        }
        targets
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
        while let Some(event) = self.stack.pop() {
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
                | AnalyzedEvent::NewScope(_) => {
                    return Some(event);
                }
            }
        }
        None
    }
}

pub struct ThinAnalyzedBlock<'i, 'p> {
    pub scope: AnalyzedScope<'i, 'p>,
    pub templates: HashSet<AnalyzedTemplate<'i>>,
    pub targets: HashSet<AnalyzedTarget<'i>>,
}

impl ThinAnalyzedBlock<'_, '_> {
    pub fn new_top_level() -> Self {
        ThinAnalyzedBlock {
            scope: AnalyzedScope::new(None),
            templates: HashSet::new(),
            targets: HashSet::new(),
        }
    }

    pub fn merge(&mut self, other: &Self) {
        self.scope.merge(&other.scope);
        self.templates.extend(other.templates.clone());
        self.targets.extend(other.targets.clone());
    }
}

pub enum AnalyzedEvent<'i, 'p> {
    Conditions(Vec<AnalyzedBlock<'i, 'p>>),
    Import(AnalyzedImport<'i>),
    DeclareArgs(AnalyzedBlock<'i, 'p>),
    Assignment(AnalyzedAssignment<'i, 'p>),
    Template(AnalyzedTemplate<'i>),
    Target(AnalyzedTarget<'i>),
    NewScope(AnalyzedBlock<'i, 'p>),
}

pub struct AnalyzedImport<'i> {
    pub file: Arc<ThinAnalyzedFile>,
    pub span: Span<'i>,
}

pub struct AnalyzedScope<'i, 'p> {
    pub parent: Option<Box<AnalyzedScope<'i, 'p>>>,
    pub variables: HashMap<&'i str, AnalyzedVariable<'i, 'p>>,
}

impl<'i, 'p> AnalyzedScope<'i, 'p> {
    pub fn new(parent: Option<Box<AnalyzedScope<'i, 'p>>>) -> Self {
        AnalyzedScope {
            parent,
            variables: HashMap::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&AnalyzedVariable<'i, 'p>> {
        self.variables
            .get(name)
            .or_else(|| self.parent.as_ref().and_then(|p| p.get(name)))
    }

    pub fn insert(&mut self, assignment: AnalyzedAssignment<'i, 'p>) {
        self.variables
            .entry(assignment.name)
            .or_insert_with(|| AnalyzedVariable {
                assignments: HashSet::new(),
            })
            .assignments
            .insert(assignment);
    }

    pub fn merge(&mut self, other: &Self) {
        for (name, other_variable) in &other.variables {
            self.variables
                .entry(name)
                .or_insert_with(|| AnalyzedVariable {
                    assignments: HashSet::new(),
                })
                .assignments
                .extend(other_variable.assignments.clone());
        }
    }

    pub fn all_variables(&self) -> HashMap<&'i str, &AnalyzedVariable<'i, 'p>> {
        let mut variables = if let Some(parent) = &self.parent {
            parent.all_variables()
        } else {
            HashMap::new()
        };
        for (name, variable) in &self.variables {
            variables.insert(name, variable);
        }
        variables
    }
}

#[derive(Clone)]
pub struct AnalyzedVariable<'i, 'p> {
    pub assignments: HashSet<AnalyzedAssignment<'i, 'p>>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalyzedAssignment<'i, 'p> {
    pub name: &'i str,
    pub comments: Comments<'i>,
    pub statement: &'p Statement<'i>,
    pub document: &'i Document,
    pub variable_span: Span<'i>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalyzedTemplate<'i> {
    pub name: &'i str,
    pub comments: Comments<'i>,
    pub document: &'i Document,
    pub header: Span<'i>,
    pub span: Span<'i>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct AnalyzedTarget<'i> {
    pub name: &'i str,
    pub document: &'i Document,
    pub header: Span<'i>,
    pub span: Span<'i>,
}

pub enum Link<'i> {
    /// Link to a file. No range is specified.
    File { path: PathBuf, span: Span<'i> },
    /// Link to a target defined in a BUILD.gn file.
    Target {
        path: PathBuf,
        name: &'i str,
        span: Span<'i>,
    },
}

#[allow(clippy::manual_map)]
fn resolve_taret<'s>(
    label: &'s str,
    current_path: &Path,
    workspace: &WorkspaceContext,
) -> Option<(PathBuf, &'s str)> {
    if let Some((prefix, name)) = label.split_once(':') {
        if prefix.is_empty() {
            Some((current_path.to_path_buf(), name))
        } else if let Some(rel_dir) = prefix.strip_prefix("//") {
            Some((workspace.root.join(rel_dir).join("BUILD.gn"), name))
        } else {
            None
        }
    } else if let Some(rel_dir) = label.strip_prefix("//") {
        if !rel_dir.is_empty() {
            Some((
                workspace.root.join(rel_dir).join("BUILD.gn"),
                rel_dir.split('/').last().unwrap(),
            ))
        } else {
            None
        }
    } else {
        None
    }
}

fn collect_links<'i>(
    ast_root: &Block<'i>,
    path: &Path,
    workspace: &WorkspaceContext,
) -> Vec<Link<'i>> {
    ast_root
        .strings()
        .filter_map(|string| {
            let content = parse_simple_literal(string.raw_value)?;
            if !content.contains(":") && content.contains(".") {
                let path = workspace.resolve_path(content, path.parent().unwrap());
                if let Ok(true) = path.try_exists() {
                    return Some(Link::File {
                        path: path.to_path_buf(),
                        span: string.span,
                    });
                }
            } else if let Some((build_gn_path, name)) = resolve_taret(content, path, workspace) {
                return Some(Link::Target {
                    path: build_gn_path,
                    name,
                    span: string.span,
                });
            }
            None
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
            Statement::Condition(top_condition) => {
                let mut top_symbol = DocumentSymbol {
                    name: format!("if ({})", top_condition.condition.span().as_str()),
                    detail: None,
                    kind: SymbolKind::NAMESPACE,
                    tags: None,
                    deprecated: None,
                    range: line_index.range(top_condition.span()),
                    selection_range: line_index.range(top_condition.condition.span()),
                    children: Some(Vec::new()),
                };

                let mut current_condition = top_condition;
                let mut current_children = top_symbol.children.as_mut().unwrap();
                loop {
                    current_children.extend(collect_symbols(
                        current_condition.then_block.as_node(),
                        line_index,
                    ));
                    match &current_condition.else_block {
                        None => break,
                        Some(Either::Left(next_condition)) => {
                            current_children.push(DocumentSymbol {
                                name: format!(
                                    "else if ({})",
                                    next_condition.condition.span().as_str()
                                ),
                                detail: None,
                                kind: SymbolKind::NAMESPACE,
                                tags: None,
                                deprecated: None,
                                range: line_index.range(next_condition.span()),
                                selection_range: line_index.range(next_condition.condition.span()),
                                children: Some(Vec::new()),
                            });
                            current_children = current_children
                                .last_mut()
                                .unwrap()
                                .children
                                .as_mut()
                                .unwrap();
                            current_condition = next_condition;
                        }
                        Some(Either::Right(else_block)) => {
                            current_children.push(DocumentSymbol {
                                name: "else".to_string(),
                                detail: None,
                                kind: SymbolKind::NAMESPACE,
                                tags: None,
                                deprecated: None,
                                range: line_index.range(else_block.span()),
                                selection_range: line_index.range(else_block.span()),
                                children: Some(collect_symbols(else_block.as_node(), line_index)),
                            });
                            break;
                        }
                    }
                }

                symbols.push(top_symbol);
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

#[derive(Debug)]
struct LoopError {
    cycle: Vec<PathBuf>,
}

impl std::fmt::Display for LoopError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Cycle detected: ")?;
        for (i, path) in self.cycle.iter().enumerate() {
            if i > 0 {
                write!(f, " -> ")?;
            }
            write!(f, "{}", path.to_string_lossy())?;
        }
        Ok(())
    }
}

impl std::error::Error for LoopError {}

impl From<LoopError> for std::io::Error {
    fn from(err: LoopError) -> Self {
        std::io::Error::new(std::io::ErrorKind::InvalidData, err)
    }
}

pub struct ThinAnalyzer {
    storage: Arc<Mutex<DocumentStorage>>,
    cache: BTreeMap<PathBuf, Arc<ThinAnalyzedFile>>,
}

impl ThinAnalyzer {
    pub fn new(storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            storage: storage.clone(),
            cache: BTreeMap::new(),
        }
    }

    pub fn analyze(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
    ) -> std::io::Result<Arc<ThinAnalyzedFile>> {
        self.analyze_cached(path, workspace, &mut Vec::new())
    }

    fn analyze_cached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        visiting: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ThinAnalyzedFile>> {
        if let Some(cached_file) = self.cache.get(path) {
            if &cached_file.workspace == workspace
                && cached_file.is_fresh(&self.storage.lock().unwrap())?
            {
                return Ok(cached_file.clone());
            }
        }

        let new_file = self.analyze_uncached(path, workspace, visiting)?;
        self.cache.insert(path.to_path_buf(), new_file.clone());

        Ok(new_file)
    }

    fn analyze_uncached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        visiting: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ThinAnalyzedFile>> {
        if visiting.iter().any(|p| p == path) {
            return Err(LoopError {
                cycle: std::mem::take(visiting),
            }
            .into());
        }

        visiting.push(path.to_path_buf());
        let result = self.analyze_uncached_inner(path, workspace, visiting);
        visiting.pop();
        result
    }

    fn analyze_uncached_inner(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
        visiting: &mut Vec<PathBuf>,
    ) -> std::io::Result<Arc<ThinAnalyzedFile>> {
        let document = match self.storage.lock().unwrap().read(path) {
            Ok(document) => document,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                // Ignore missing imports as they might be imported conditionally.
                return Ok(ThinAnalyzedFile::empty(path, workspace));
            }
            Err(err) => return Err(err),
        };
        let ast_root = Box::pin(parse(&document.data));
        let mut deps = Vec::new();
        let analyzed_root =
            self.analyze_block(&ast_root, workspace, &document, &mut deps, visiting)?;

        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root =
            unsafe { std::mem::transmute::<ThinAnalyzedBlock, ThinAnalyzedBlock>(analyzed_root) };
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };

        Ok(Arc::new(ThinAnalyzedFile {
            document,
            workspace: workspace.clone(),
            ast_root,
            analyzed_root,
            deps,
        }))
    }

    fn analyze_block<'i, 'p>(
        &mut self,
        block: &'p Block<'i>,
        workspace: &WorkspaceContext,
        document: &'i Document,
        deps: &mut Vec<Arc<ThinAnalyzedFile>>,
        visiting: &mut Vec<PathBuf>,
    ) -> std::io::Result<ThinAnalyzedBlock<'i, 'p>> {
        let mut analyzed_block = ThinAnalyzedBlock::new_top_level();

        for statement in &block.statements {
            match statement {
                Statement::Assignment(assignment) => {
                    let identifier = match &assignment.lvalue {
                        LValue::Identifier(identifier) => identifier,
                        LValue::ArrayAccess(array_access) => &array_access.array,
                        LValue::ScopeAccess(scope_access) => &scope_access.scope,
                    };
                    if is_exported(identifier.name) {
                        analyzed_block.scope.insert(AnalyzedAssignment {
                            name: identifier.name,
                            comments: assignment.comments.clone(),
                            statement,
                            document,
                            variable_span: identifier.span,
                        });
                    }
                }
                Statement::Call(call) => match call.function.name {
                    "import" => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            let path =
                                workspace.resolve_path(name, document.path.parent().unwrap());
                            let file = self.analyze_cached(&path, workspace, visiting)?;
                            analyzed_block.merge(&file.analyzed_root);
                            deps.push(file);
                        }
                    }
                    "template" => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            if is_exported(name) {
                                analyzed_block.templates.insert(AnalyzedTemplate {
                                    name,
                                    comments: call.comments.clone(),
                                    document,
                                    header: call.function.span,
                                    span: call.span,
                                });
                            }
                        }
                    }
                    "declare_args" | "foreach" => {
                        if let Some(block) = &call.block {
                            analyzed_block.merge(
                                &self.analyze_block(block, workspace, document, deps, visiting)?,
                            );
                        }
                    }
                    "set_defaults" => {}
                    "forward_variables_from" => {
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
                                        analyzed_block.scope.insert(AnalyzedAssignment {
                                            name,
                                            comments: Comments::default(),
                                            statement,
                                            document,
                                            variable_span: string.span,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        if let Some(name) = call
                            .args
                            .iter()
                            .exactly_one()
                            .ok()
                            .and_then(|expr| expr.as_primary_string())
                            .and_then(|s| parse_simple_literal(s.raw_value))
                        {
                            analyzed_block.targets.insert(AnalyzedTarget {
                                name,
                                document,
                                header: call.args[0].span(),
                                span: call.span,
                            });
                        }
                    }
                },
                Statement::Condition(condition) => {
                    let mut current_condition = condition;
                    loop {
                        analyzed_block.merge(&self.analyze_block(
                            &current_condition.then_block,
                            workspace,
                            document,
                            deps,
                            visiting,
                        )?);
                        match &current_condition.else_block {
                            None => break,
                            Some(Either::Left(next_condition)) => {
                                current_condition = next_condition;
                            }
                            Some(Either::Right(block)) => {
                                analyzed_block.merge(
                                    &self.analyze_block(
                                        block, workspace, document, deps, visiting,
                                    )?,
                                );
                                break;
                            }
                        }
                    }
                }
                Statement::Unknown(_) | Statement::UnmatchedBrace(_) => {}
            }
        }

        Ok(analyzed_block)
    }
}

struct WorkspaceCache {
    dot_gn_version: DocumentVersion,
    context: WorkspaceContext,
    files: BTreeMap<PathBuf, Arc<AnalyzedFile>>,
}

pub struct Analyzer {
    thin_analyzer: ThinAnalyzer,
    storage: Arc<Mutex<DocumentStorage>>,
    cache: BTreeMap<PathBuf, WorkspaceCache>,
}

impl Analyzer {
    pub fn new(storage: &Arc<Mutex<DocumentStorage>>) -> Self {
        Self {
            storage: storage.clone(),
            thin_analyzer: ThinAnalyzer::new(storage),
            cache: BTreeMap::new(),
        }
    }

    pub fn analyze(&mut self, path: &Path) -> std::io::Result<Arc<AnalyzedFile>> {
        let path = path.canonicalize()?;
        self.analyze_cached(&path)
    }

    fn workspace_cache_for(&mut self, path: &Path) -> std::io::Result<&mut WorkspaceCache> {
        let workspace_root = find_workspace_root(path)?;
        let dot_gn_path = workspace_root.join(".gn");
        let dot_gn_version = {
            let storage = self.storage.lock().unwrap();
            storage.read_version(&dot_gn_path)?
        };

        let cache_hit = self
            .cache
            .get(&workspace_root)
            .is_some_and(|workspace_cache| workspace_cache.dot_gn_version == dot_gn_version);
        if cache_hit {
            return Ok(self.cache.get_mut(&workspace_root).unwrap());
        }

        let build_config = {
            let storage = self.storage.lock().unwrap();
            let document = storage.read(&dot_gn_path)?;
            evaluate_dot_gn(&workspace_root, &document.data)?
        };

        let context = WorkspaceContext {
            root: workspace_root.clone(),
            build_config,
        };

        let workspace_cache = WorkspaceCache {
            dot_gn_version,
            context,
            files: BTreeMap::new(),
        };
        Ok(self.cache.entry(workspace_root).or_insert(workspace_cache))
    }

    fn analyze_cached(&mut self, path: &Path) -> std::io::Result<Arc<AnalyzedFile>> {
        let (cached_file, context) = {
            let workspace_cache = self.workspace_cache_for(path)?;
            (
                workspace_cache.files.get(path).cloned(),
                workspace_cache.context.clone(),
            )
        };
        if let Some(cached_file) = cached_file {
            let storage = self.storage.lock().unwrap();
            if cached_file.is_fresh(&storage)? {
                return Ok(cached_file);
            }
        }

        let new_file = self.analyze_uncached(path, &context)?;
        self.workspace_cache_for(path)?
            .files
            .insert(path.to_path_buf(), new_file.clone());
        Ok(new_file)
    }

    fn analyze_uncached(
        &mut self,
        path: &Path,
        workspace: &WorkspaceContext,
    ) -> std::io::Result<Arc<AnalyzedFile>> {
        let document = self.storage.lock().unwrap().read(path)?;
        let ast_root = Box::pin(parse(&document.data));

        let mut deps = Vec::new();
        let mut analyzed_root = self.analyze_block(&ast_root, workspace, &document, &mut deps)?;

        // Insert a synthetic import of BUILDCONFIG.gn.
        let dot_gn_file = self
            .thin_analyzer
            .analyze(&workspace.build_config, workspace)?;
        analyzed_root.events.insert(
            0,
            AnalyzedEvent::Import(AnalyzedImport {
                file: dot_gn_file.clone(),
                span: Span::new(&document.data, 0, 0).unwrap(),
            }),
        );
        deps.push(dot_gn_file);

        let links = collect_links(&ast_root, path, workspace);
        let symbols = collect_symbols(ast_root.as_node(), &document.line_index);

        // SAFETY: links' contents are backed by pinned document.
        let links = unsafe { std::mem::transmute::<Vec<Link>, Vec<Link>>(links) };
        // SAFETY: analyzed_root's contents are backed by pinned document and pinned ast_root.
        let analyzed_root =
            unsafe { std::mem::transmute::<AnalyzedBlock, AnalyzedBlock>(analyzed_root) };
        // SAFETY: ast_root's contents are backed by pinned document.
        let ast_root = unsafe { std::mem::transmute::<Pin<Box<Block>>, Pin<Box<Block>>>(ast_root) };

        Ok(Arc::new(AnalyzedFile {
            document,
            workspace: workspace.clone(),
            ast_root,
            analyzed_root,
            deps,
            links,
            symbols,
        }))
    }

    fn analyze_block<'i, 'p>(
        &mut self,
        block: &'p Block<'i>,
        workspace: &WorkspaceContext,
        document: &'i Document,
        deps: &mut Vec<Arc<ThinAnalyzedFile>>,
    ) -> std::io::Result<AnalyzedBlock<'i, 'p>> {
        let events: Vec<AnalyzedEvent> = block
            .statements
            .iter()
            .map(|statement| -> std::io::Result<Vec<AnalyzedEvent>> {
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
                            workspace,
                            document,
                            deps,
                        )?);
                        Ok(events)
                    }
                    Statement::Call(call) => {
                        match call.function.name {
                            "import" => {
                                if let Some(name) = call
                                    .args
                                    .iter()
                                    .exactly_one()
                                    .ok()
                                    .and_then(|expr| expr.as_primary_string())
                                    .and_then(|s| parse_simple_literal(s.raw_value))
                                {
                                    let path = workspace
                                        .resolve_path(name, document.path.parent().unwrap());
                                    let file = match self.thin_analyzer.analyze(&path, workspace) {
                                        Err(err) if err.kind() == ErrorKind::NotFound => {
                                            // Ignore missing imports as they might be imported conditionally.
                                            ThinAnalyzedFile::empty(&path, workspace)
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
                            "template" => {
                                let mut events = Vec::new();
                                if let Some(name) = call
                                    .args
                                    .iter()
                                    .exactly_one()
                                    .ok()
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
                                        self.analyze_block(block, workspace, document, deps)?,
                                    ));
                                }
                                Ok(events)
                            }
                            "declare_args" => {
                                if let Some(block) = &call.block {
                                    let analyzed_root =
                                        self.analyze_block(block, workspace, document, deps)?;
                                    Ok(vec![AnalyzedEvent::DeclareArgs(analyzed_root)])
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            "foreach" => {
                                if let Some(block) = &call.block {
                                    Ok(self.analyze_block(block, workspace, document, deps)?.events)
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            "set_defaults" => {
                                if let Some(block) = &call.block {
                                    let analyzed_root =
                                        self.analyze_block(block, workspace, document, deps)?;
                                    Ok(vec![AnalyzedEvent::NewScope(analyzed_root)])
                                } else {
                                    Ok(Vec::new())
                                }
                            }
                            "forward_variables_from" => {
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
                                    .args
                                    .iter()
                                    .exactly_one()
                                    .ok()
                                    .and_then(|expr| expr.as_primary_string())
                                    .and_then(|s| parse_simple_literal(s.raw_value))
                                {
                                    events.push(AnalyzedEvent::Target(AnalyzedTarget {
                                        name,
                                        document,
                                        header: call.args[0].span(),
                                        span: call.span,
                                    }));
                                }
                                if let Some(block) = &call.block {
                                    events.push(AnalyzedEvent::NewScope(
                                        self.analyze_block(block, workspace, document, deps)?,
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
                            events.extend(self.analyze_expr(
                                &current_condition.condition,
                                workspace,
                                document,
                                deps,
                            )?);
                            condition_blocks.push(self.analyze_block(
                                &current_condition.then_block,
                                workspace,
                                document,
                                deps,
                            )?);
                            match &current_condition.else_block {
                                None => break,
                                Some(Either::Left(next_condition)) => {
                                    current_condition = next_condition;
                                }
                                Some(Either::Right(block)) => {
                                    condition_blocks.push(
                                        self.analyze_block(block, workspace, document, deps)?,
                                    );
                                    break;
                                }
                            }
                        }
                        events.push(AnalyzedEvent::Conditions(condition_blocks));
                        Ok(events)
                    }
                    Statement::Unknown(_) | Statement::UnmatchedBrace(_) => Ok(Vec::new()),
                }
            })
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect();

        Ok(AnalyzedBlock {
            events,
            span: block.span,
        })
    }

    fn analyze_expr<'i, 'p>(
        &mut self,
        expr: &'p Expr<'i>,
        workspace: &WorkspaceContext,
        document: &'i Document,
        deps: &mut Vec<Arc<ThinAnalyzedFile>>,
    ) -> std::io::Result<Vec<AnalyzedEvent<'i, 'p>>> {
        match expr {
            Expr::Primary(primary_expr) => match primary_expr.as_ref() {
                PrimaryExpr::Block(block) => {
                    let analyzed_root = self.analyze_block(block, workspace, document, deps)?;
                    Ok(vec![AnalyzedEvent::NewScope(analyzed_root)])
                }
                PrimaryExpr::Call(call) => {
                    let mut events: Vec<AnalyzedEvent> = call
                        .args
                        .iter()
                        .map(|expr| self.analyze_expr(expr, workspace, document, deps))
                        .collect::<std::io::Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect();
                    if let Some(block) = &call.block {
                        let analyzed_root = self.analyze_block(block, workspace, document, deps)?;
                        events.push(AnalyzedEvent::NewScope(analyzed_root));
                    }
                    Ok(events)
                }
                PrimaryExpr::ParenExpr(paren_expr) => {
                    self.analyze_expr(&paren_expr.expr, workspace, document, deps)
                }
                PrimaryExpr::List(list_literal) => Ok(list_literal
                    .values
                    .iter()
                    .map(|expr| self.analyze_expr(expr, workspace, document, deps))
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect()),
                PrimaryExpr::Identifier(_)
                | PrimaryExpr::Integer(_)
                | PrimaryExpr::String(_)
                | PrimaryExpr::ArrayAccess(_)
                | PrimaryExpr::ScopeAccess(_) => Ok(Vec::new()),
            },
            Expr::Unary(unary_expr) => {
                self.analyze_expr(&unary_expr.expr, workspace, document, deps)
            }
            Expr::Binary(binary_expr) => {
                let mut events = self.analyze_expr(&binary_expr.lhs, workspace, document, deps)?;
                events.extend(self.analyze_expr(&binary_expr.rhs, workspace, document, deps)?);
                Ok(events)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::testutil::testdata;

    use super::*;

    #[test]
    fn test_analyze_smoke() {
        let storage = Arc::new(Mutex::new(DocumentStorage::new()));
        let mut analyzer = Analyzer::new(&storage);

        let file = analyzer
            .analyze(&testdata("workspaces/smoke/BUILD.gn"))
            .unwrap();

        // No parse error.
        assert!(file
            .ast_root
            .statements
            .iter()
            .all(|s| !matches!(s, Statement::Unknown(_) | Statement::UnmatchedBrace(_))));

        // Inspect the top-level scope.
        let scope = file.scope_at(0);
        assert!(scope.get("enable_opt").is_some());
        assert!(scope.get("_lib").is_some());
        assert!(scope.get("is_linux").is_some());
    }
}
