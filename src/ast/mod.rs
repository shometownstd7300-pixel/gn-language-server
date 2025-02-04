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

use either::Either;
use pest::Span;

mod parser;

pub trait Node<'i> {
    fn as_node(&self) -> &dyn Node<'i>;
    fn children(&self) -> Vec<&dyn Node<'i>>;
    fn span(&self) -> Span<'i>;

    fn as_statement(&self) -> Option<&Statement<'i>> {
        None
    }

    fn as_identifier(&self) -> Option<&Identifier<'i>> {
        None
    }

    fn as_string(&self) -> Option<&StringLiteral<'i>> {
        None
    }

    fn identifiers<'n>(&'n self) -> FilterWalk<'i, 'n, Identifier<'i>> {
        FilterWalk::new(self.as_node(), |node| node.as_identifier())
    }

    fn strings<'n>(&'n self) -> FilterWalk<'i, 'n, StringLiteral<'i>> {
        FilterWalk::new(self.as_node(), |node| node.as_string())
    }
}

pub struct Walk<'i, 'n> {
    stack: Vec<&'n dyn Node<'i>>,
}

impl<'i, 'n> Walk<'i, 'n> {
    pub fn new(node: &'n dyn Node<'i>) -> Self {
        Walk { stack: vec![node] }
    }
}

impl<'i, 'n> Iterator for Walk<'i, 'n> {
    type Item = &'n dyn Node<'i>;

    fn next(&mut self) -> Option<Self::Item> {
        let node = self.stack.pop()?;
        self.stack.extend(node.children().into_iter().rev());
        Some(node)
    }
}

pub struct FilterWalk<'i, 'n, T> {
    #[allow(clippy::type_complexity)]
    inner: std::iter::FilterMap<Walk<'i, 'n>, fn(&'n dyn Node<'i>) -> Option<&'n T>>,
}

impl<'i, 'n, T> FilterWalk<'i, 'n, T> {
    pub fn new(node: &'n dyn Node<'i>, filter: fn(&'n dyn Node<'i>) -> Option<&'n T>) -> Self {
        FilterWalk {
            inner: Walk::new(node).filter_map(filter),
        }
    }
}

impl<'n, T> Iterator for FilterWalk<'_, 'n, T> {
    type Item = &'n T;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Statement<'i> {
    Assignment(Box<Assignment<'i>>),
    Call(Box<Call<'i>>),
    Condition(Box<Condition<'i>>),
    Error(Box<ErrorStatement<'i>>),
}

impl<'i> Node<'i> for Statement<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        match self {
            Statement::Assignment(assignment) => vec![assignment.as_node()],
            Statement::Call(call) => vec![call.as_node()],
            Statement::Condition(condition) => vec![condition.as_node()],
            Statement::Error(error) => vec![error.as_node()],
        }
    }

    fn span(&self) -> Span<'i> {
        match self {
            Statement::Assignment(assignment) => assignment.span,
            Statement::Call(call) => call.span,
            Statement::Condition(condition) => condition.span,
            Statement::Error(error) => error.span(),
        }
    }

    fn as_statement(&self) -> Option<&Statement<'i>> {
        Some(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum LValue<'i> {
    Identifier(Box<Identifier<'i>>),
    ArrayAccess(Box<ArrayAccess<'i>>),
    ScopeAccess(Box<ScopeAccess<'i>>),
}

impl<'i> Node<'i> for LValue<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        match self {
            LValue::Identifier(identifier) => vec![identifier.as_node()],
            LValue::ArrayAccess(array_access) => vec![array_access.as_node()],
            LValue::ScopeAccess(scope_access) => vec![scope_access.as_node()],
        }
    }

    fn span(&self) -> Span<'i> {
        match self {
            LValue::Identifier(identifier) => identifier.span,
            LValue::ArrayAccess(array_access) => array_access.span,
            LValue::ScopeAccess(scope_access) => scope_access.span,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Assignment<'i> {
    pub lvalue: LValue<'i>,
    pub op: AssignOp,
    pub rvalue: Box<Expr<'i>>,
    pub comments: Comments<'i>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for Assignment<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        vec![&self.lvalue, &*self.rvalue]
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Call<'i> {
    pub function: Identifier<'i>,
    pub args: Vec<Expr<'i>>,
    pub block: Option<Block<'i>>,
    pub comments: Comments<'i>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for Call<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        let mut children: Vec<&dyn Node> = vec![&self.function];
        children.extend(self.args.iter().map(|arg| arg as &dyn Node));
        if let Some(block) = &self.block {
            children.push(block);
        }
        children
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Condition<'i> {
    pub condition: Box<Expr<'i>>,
    pub then_block: Block<'i>,
    pub else_block: Option<Either<Box<Condition<'i>>, Block<'i>>>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for Condition<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        let mut children: Vec<&dyn Node> = vec![&*self.condition, &self.then_block];
        match &self.else_block {
            Some(Either::Left(else_condition)) => children.push(&**else_condition),
            Some(Either::Right(else_block)) => children.push(else_block),
            None => {}
        }
        children
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Block<'i> {
    pub statements: Vec<Statement<'i>>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for Block<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        self.statements
            .iter()
            .map(|statement| statement as &dyn Node)
            .collect()
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ArrayAccess<'i> {
    pub array: Identifier<'i>,
    pub index: Box<Expr<'i>>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for ArrayAccess<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        vec![&self.array, &*self.index]
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ScopeAccess<'i> {
    pub scope: Identifier<'i>,
    pub member: Identifier<'i>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for ScopeAccess<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        vec![&self.scope, &self.member]
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Expr<'i> {
    Primary(Box<PrimaryExpr<'i>>),
    Unary(Box<UnaryExpr<'i>>),
    Binary(Box<BinaryExpr<'i>>),
}

impl<'i> Expr<'i> {
    pub fn as_primary(&self) -> Option<&PrimaryExpr<'i>> {
        match self {
            Expr::Primary(primary_expr) => Some(primary_expr),
            _ => None,
        }
    }

    pub fn as_primary_string(&self) -> Option<&StringLiteral<'i>> {
        match self.as_primary()? {
            PrimaryExpr::String(string) => Some(string),
            _ => None,
        }
    }

    pub fn as_primary_list(&self) -> Option<&ListLiteral<'i>> {
        match self.as_primary()? {
            PrimaryExpr::List(list) => Some(list),
            _ => None,
        }
    }
}

impl<'i> Node<'i> for Expr<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        match self {
            Expr::Primary(primary_expr) => vec![primary_expr.as_node()],
            Expr::Unary(unary_expr) => vec![unary_expr.as_node()],
            Expr::Binary(binary_expr) => vec![binary_expr.as_node()],
        }
    }

    fn span(&self) -> Span<'i> {
        match self {
            Expr::Primary(primary_expr) => primary_expr.span(),
            Expr::Unary(unary_expr) => unary_expr.span(),
            Expr::Binary(binary_expr) => binary_expr.span(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum PrimaryExpr<'i> {
    Identifier(Box<Identifier<'i>>),
    Integer(Box<IntegerLiteral<'i>>),
    String(Box<StringLiteral<'i>>),
    Call(Box<Call<'i>>),
    ArrayAccess(Box<ArrayAccess<'i>>),
    ScopeAccess(Box<ScopeAccess<'i>>),
    Block(Box<Block<'i>>),
    ParenExpr(Box<ParenExpr<'i>>),
    List(Box<ListLiteral<'i>>),
}

impl<'i> Node<'i> for PrimaryExpr<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        match self {
            PrimaryExpr::Identifier(identifier) => vec![identifier.as_node()],
            PrimaryExpr::Integer(integer) => vec![integer.as_node()],
            PrimaryExpr::String(string) => vec![string.as_node()],
            PrimaryExpr::Call(call) => vec![call.as_node()],
            PrimaryExpr::ArrayAccess(array_access) => vec![array_access.as_node()],
            PrimaryExpr::ScopeAccess(scope_access) => vec![scope_access.as_node()],
            PrimaryExpr::Block(block) => vec![block.as_node()],
            PrimaryExpr::ParenExpr(paren_expr) => vec![paren_expr.as_node()],
            PrimaryExpr::List(list) => vec![list.as_node()],
        }
    }

    fn span(&self) -> Span<'i> {
        match self {
            PrimaryExpr::Identifier(identifier) => identifier.span(),
            PrimaryExpr::Integer(integer) => integer.span(),
            PrimaryExpr::String(string) => string.span(),
            PrimaryExpr::Call(call) => call.span(),
            PrimaryExpr::ArrayAccess(array_access) => array_access.span(),
            PrimaryExpr::ScopeAccess(scope_access) => scope_access.span(),
            PrimaryExpr::Block(block) => block.span(),
            PrimaryExpr::ParenExpr(expr) => expr.span(),
            PrimaryExpr::List(list) => list.span(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnaryExpr<'i> {
    pub op: UnaryOp,
    pub expr: Box<Expr<'i>>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for UnaryExpr<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        vec![&*self.expr]
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct BinaryExpr<'i> {
    pub lhs: Box<Expr<'i>>,
    pub op: BinaryOp,
    pub rhs: Box<Expr<'i>>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for BinaryExpr<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        vec![&*self.lhs, &*self.rhs]
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum AssignOp {
    Assign,
    AddAssign,
    SubAssign,
}

impl std::fmt::Display for AssignOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssignOp::Assign => write!(f, "="),
            AssignOp::AddAssign => write!(f, "+="),
            AssignOp::SubAssign => write!(f, "-="),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum UnaryOp {
    Not,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum BinaryOp {
    Add,
    Sub,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Identifier<'i> {
    pub name: &'i str,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for Identifier<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        Vec::new()
    }

    fn span(&self) -> Span<'i> {
        self.span
    }

    fn as_identifier(&self) -> Option<&Identifier<'i>> {
        Some(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct IntegerLiteral<'i> {
    pub value: i64,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for IntegerLiteral<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        Vec::new()
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct StringLiteral<'i> {
    pub raw_value: &'i str,
    pub embedded_exprs: Vec<Expr<'i>>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for StringLiteral<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        self.embedded_exprs
            .iter()
            .map(|expr| expr.as_node())
            .collect()
    }

    fn span(&self) -> Span<'i> {
        self.span
    }

    fn as_string(&self) -> Option<&StringLiteral<'i>> {
        Some(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ParenExpr<'i> {
    pub expr: Box<Expr<'i>>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for ParenExpr<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        vec![&*self.expr]
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ListLiteral<'i> {
    pub values: Vec<Expr<'i>>,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for ListLiteral<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        self.values.iter().map(|value| value as &dyn Node).collect()
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ErrorStatement<'i> {
    UnknownStatement(Box<UnknownStatement<'i>>),
    UnmatchedBrace(Box<UnmatchedBrace<'i>>),
}

impl<'i> Node<'i> for ErrorStatement<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        match self {
            ErrorStatement::UnknownStatement(unknown) => vec![unknown.as_node()],
            ErrorStatement::UnmatchedBrace(unmatched_brace) => vec![unmatched_brace.as_node()],
        }
    }

    fn span(&self) -> Span<'i> {
        match self {
            ErrorStatement::UnknownStatement(unknown) => unknown.span,
            ErrorStatement::UnmatchedBrace(unmatched_brace) => unmatched_brace.span,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnknownStatement<'i> {
    pub text: &'i str,
    pub span: Span<'i>,
}

impl<'i> Node<'i> for UnknownStatement<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        Vec::new()
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnmatchedBrace<'i> {
    pub span: Span<'i>,
}

impl<'i> Node<'i> for UnmatchedBrace<'i> {
    fn as_node(&self) -> &dyn Node<'i> {
        self
    }

    fn children(&self) -> Vec<&dyn Node<'i>> {
        Vec::new()
    }

    fn span(&self) -> Span<'i> {
        self.span
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct Comments<'i> {
    pub lines: Vec<&'i str>,
}

impl Comments<'_> {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

impl std::fmt::Display for Comments<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in &self.lines {
            writeln!(f, "{}", line)?;
        }
        Ok(())
    }
}

pub fn parse(input: &str) -> Block {
    parser::parse(input)
}
