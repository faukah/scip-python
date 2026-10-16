// SPDX-License-Identifier: EUPL-1.2

//! The symbols a module defines, named by where they're defined: one AST walk
//! per file gives each class, function, method, attribute and module-level
//! variable its SCIP symbol, keyed by the range of its name. A reference
//! resolves to a definition's name range, and so to its symbol here.

use std::collections::HashMap;

use ruff_python_ast::{
    self as ast,
    Expr,
    ExprContext,
    Stmt,
    visitor::{
        self,
        Visitor as _,
    },
};
use ruff_text_size::{
    Ranged as _,
    TextRange,
    TextSize,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Class,
    Function,
    Method,
    /// assigned in a class body
    Attribute,
    /// assigned on `self` in a method, which may assign what a base declares
    SelfAttribute,
    /// a module-level variable
    Variable,
}

impl Kind {
    /// Whether binding the name again assigns it rather than defining it
    /// anew, as redefining a function does.
    pub const fn assigns(self) -> bool {
        matches!(self, Self::Attribute | Self::SelfAttribute | Self::Variable)
    }
}

#[derive(Debug)]
pub struct Def {
    pub symbol: String,
    pub name:   String,
    pub kind:   Kind,
    pub range:  TextRange,
    /// the class a method or attribute belongs to, by its index in the table
    pub owner:  Option<usize>,
    /// whether no earlier definition has the symbol
    pub first:  bool,
}

#[derive(Debug)]
pub struct Table {
    /// the symbol of the module itself, which every other one extends
    module: String,
    /// in source order
    defs:   Vec<Def>,
    /// by the end of their name: ty gives `self.name = ..` the range of the
    /// whole `self.name`, which ends where the name does
    ends:   HashMap<TextSize, usize>,
    firsts: HashMap<String, usize>,
}

impl Table {
    /// The symbols `module` defines, each extending `prefix`, the
    /// [`module`] symbol.
    pub fn build(module: &ast::ModModule, prefix: String) -> Self {
        let mut walker = Walker {
            table:  Self {
                module: prefix.clone(),
                defs:   Vec::new(),
                ends:   HashMap::new(),
                firsts: HashMap::new(),
            },
            scopes: vec![Scope::Module(prefix)],
        };
        walker.visit_body(&module.body);
        walker.table
    }

    pub fn module(&self) -> &str {
        &self.module
    }

    pub fn defs(&self) -> &[Def] {
        &self.defs
    }

    /// The definition whose name is at `range`, or ends it.
    pub fn at(&self, range: TextRange) -> Option<&Def> {
        self.ends
            .get(&range.end())
            .map(|&index| &self.defs[index])
            .filter(|def| range.contains_range(def.range))
    }

    /// The first definition of `symbol`.
    pub fn def(&self, symbol: &str) -> Option<&Def> {
        self.firsts.get(symbol).map(|&index| &self.defs[index])
    }

    /// The class `def` is a method or attribute of.
    pub fn owner(&self, def: &Def) -> Option<&Def> {
        def.owner.map(|index| &self.defs[index])
    }
}

const SCHEME: &str = "scip-python";
const MANAGER: &str = "python";

/// The symbol of the module `name` in `package` at `version`, which the
/// symbols of what it defines extend.
pub fn module(package: &str, version: &str, name: &str) -> String {
    // package names and versions are space-separated, so spaces double
    format!(
        "{SCHEME} {MANAGER} {} {} {}/",
        package.replace(' ', "  "),
        version.replace(' ', "  "),
        escape(name)
    )
}

pub fn class(parent: &str, name: &str) -> String {
    format!("{parent}{}#", escape(name))
}

pub fn method(parent: &str, name: &str) -> String {
    format!("{parent}{}().", escape(name))
}

pub fn term(parent: &str, name: &str) -> String {
    format!("{parent}{}.", escape(name))
}

/// `name` as a SCIP descriptor name, backquoted unless it's a plain
/// identifier
fn escape(name: &str) -> String {
    if name
        .chars()
        .all(|char| char.is_ascii_alphanumeric() || matches!(char, '_' | '+' | '-' | '$'))
    {
        name.to_owned()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

enum Scope {
    /// the module symbol
    Module(String),
    /// the class's index in the table
    Class(usize),
    Function {
        symbol:   String,
        /// for a method, its class and the name its first parameter binds
        receiver: Option<(usize, String)>,
    },
    /// a lambda or comprehension, whose bindings are all local
    Local,
}

struct Walker {
    table:  Table,
    scopes: Vec<Scope>,
}

impl Walker {
    fn scope(&self) -> &Scope {
        self.scopes
            .last()
            .expect("the module scope is never popped")
    }

    /// the symbol a nested definition extends, none in a local scope
    fn prefix(&self) -> Option<&str> {
        match *self.scope() {
            Scope::Module(ref symbol) | Scope::Function { ref symbol, .. } => Some(symbol),
            Scope::Class(index) => Some(&self.table.defs[index].symbol),
            Scope::Local => None,
        }
    }

    fn define(
        &mut self,
        symbol: String,
        name: &str,
        kind: Kind,
        range: TextRange,
        owner: Option<usize>,
    ) -> usize {
        let index = self.table.defs.len();
        let first = !self.table.firsts.contains_key(&symbol);
        if first {
            self.table.firsts.insert(symbol.clone(), index);
        }
        self.table.ends.insert(range.end(), index);
        self.table.defs.push(Def {
            symbol,
            name: name.to_owned(),
            kind,
            range,
            owner,
            first,
        });
        index
    }

    fn function(&mut self, function: &ast::StmtFunctionDef) {
        for decorator in &function.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(ref type_params) = function.type_params {
            self.visit_type_params(type_params);
        }
        self.visit_parameters(&function.parameters);
        if let Some(ref returns) = function.returns {
            self.visit_annotation(returns);
        }
        let Some(prefix) = self.prefix() else {
            return;
        };

        let symbol = method(prefix, &function.name);
        let owner = if let Scope::Class(index) = *self.scope() {
            Some(index)
        } else {
            None
        };
        let receiver = owner.zip(
            function
                .parameters
                .posonlyargs
                .iter()
                .chain(&function.parameters.args)
                .next()
                .map(|first| first.parameter.name.to_string()),
        );
        let kind = if owner.is_some() {
            Kind::Method
        } else {
            Kind::Function
        };
        self.define(
            symbol.clone(),
            &function.name,
            kind,
            function.name.range(),
            owner,
        );
        self.scopes.push(Scope::Function { symbol, receiver });
        self.visit_body(&function.body);
        self.scopes.pop();
    }

    fn class(&mut self, class_def: &ast::StmtClassDef) {
        for decorator in &class_def.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(ref type_params) = class_def.type_params {
            self.visit_type_params(type_params);
        }
        if let Some(ref arguments) = class_def.arguments {
            self.visit_arguments(arguments);
        }
        let Some(prefix) = self.prefix() else {
            return;
        };

        let symbol = class(prefix, &class_def.name);
        let index = self.define(
            symbol,
            &class_def.name,
            Kind::Class,
            class_def.name.range(),
            None,
        );
        self.scopes.push(Scope::Class(index));
        self.visit_body(&class_def.body);
        self.scopes.pop();
    }

    /// `name = ..` at module or class level
    fn store_name(&mut self, name: &ast::ExprName) {
        let (kind, owner) = match *self.scope() {
            Scope::Module(_) => (Kind::Variable, None),
            Scope::Class(index) => (Kind::Attribute, Some(index)),
            Scope::Function { .. } | Scope::Local => return,
        };
        let Some(prefix) = self.prefix() else {
            return;
        };
        let symbol = term(prefix, &name.id);
        self.define(symbol, &name.id, kind, name.range(), owner);
    }

    /// `self.name = ..` in a method
    fn store_attribute(&mut self, attribute: &ast::ExprAttribute) {
        let Scope::Function {
            receiver: Some((owner, ref receiver)),
            ..
        } = *self.scope()
        else {
            return;
        };
        if !matches!(*attribute.value, Expr::Name(ref name) if name.id.as_str() == receiver) {
            return;
        }
        let symbol = term(&self.table.defs[owner].symbol, &attribute.attr);
        self.define(
            symbol,
            &attribute.attr,
            Kind::SelfAttribute,
            attribute.attr.range(),
            Some(owner),
        );
    }
}

impl<'ast> visitor::Visitor<'ast> for Walker {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(ref function) = *stmt {
            self.function(function);
        } else if let Stmt::ClassDef(ref class_def) = *stmt {
            self.class(class_def);
        } else {
            visitor::walk_stmt(self, stmt);
        }
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(ref name) = *expr
            && name.ctx == ExprContext::Store
        {
            self.store_name(name);
        } else if let Expr::Attribute(ref attribute) = *expr
            && attribute.ctx == ExprContext::Store
        {
            self.store_attribute(attribute);
            visitor::walk_expr(self, expr);
        } else if matches!(
            *expr,
            Expr::Lambda(_)
                | Expr::ListComp(_)
                | Expr::SetComp(_)
                | Expr::DictComp(_)
                | Expr::Generator(_)
        ) {
            self.scopes.push(Scope::Local);
            visitor::walk_expr(self, expr);
            self.scopes.pop();
        } else {
            visitor::walk_expr(self, expr);
        }
    }
}
