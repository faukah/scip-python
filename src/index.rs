// SPDX-License-Identifier: EUPL-1.2

//! Documents for the project's files: each definition the walk in
//! [`crate::symbols`] finds, each reference ty resolves, and each method's
//! relationship to the ancestor methods it overrides.

use std::{
    collections::{
        HashMap,
        HashSet,
    },
    rc::Rc,
};

use ruff_db::{
    files::File,
    parsed::{
        ParsedModuleRef,
        parsed_module,
    },
    source::line_index,
    system::SystemPath,
};
use ruff_python_ast::{
    self as ast,
    Expr,
    ExprContext,
    visitor::{
        self,
        Visitor as _,
    },
};
use ruff_source_file::LineIndex;
use ruff_text_size::{
    Ranged as _,
    TextRange,
};
use scip::types::{
    Document,
    Occurrence,
    PositionEncoding,
    Relationship,
    SymbolInformation,
    SymbolRole,
    symbol_information,
};
use ty_module_resolver::SearchPath;
use ty_python_core::ProgramFile;
use ty_python_semantic::{
    HasType as _,
    ImportAliasResolution,
    ResolvedDefinition,
    SemanticModel,
    types::ide_support::{
        call_signature_details,
        definitions_for_keyword_argument,
    },
};

use crate::{
    db::Database,
    symbols::{
        self,
        Def,
        Kind,
        Table,
    },
};

/// the package the standard library's modules are in
const STDLIB: &str = "python-stdlib";

/// A class, by where it's defined.
struct Class {
    file:   File,
    range:  TextRange,
    symbol: String,
}

pub struct Indexer<'db> {
    db:        &'db Database,
    root:      &'db SystemPath,
    name:      String,
    version:   String,
    /// `builtins.object`, every class's implicit base
    object:    String,
    tables:    HashMap<File, Rc<Table>>,
    /// a class's ancestors, by the file and name range of the class
    ancestors: HashMap<(File, TextRange), Rc<[Class]>>,
    /// what binding each symbol binds, by the symbol
    targets:   HashMap<String, Rc<[String]>>,
}

impl<'db> Indexer<'db> {
    pub fn new(db: &'db Database, root: &'db SystemPath, name: String, version: String) -> Self {
        let builtins =
            symbols::module(STDLIB, &db.python_version().version.to_string(), "builtins");
        Self {
            db,
            root,
            name,
            version,
            object: symbols::class(&builtins, "object"),
            tables: HashMap::new(),
            ancestors: HashMap::new(),
            targets: HashMap::new(),
        }
    }

    /// The document for `file`, at `path` relative to the project root.
    pub fn document(&mut self, file: File, path: String) -> Document {
        let (program_file, parsed) = self.parsed(file);
        let table = self.table(file);
        let mut references = References {
            model:   SemanticModel::new(self.db, program_file),
            indexer: self,
            table:   Rc::clone(&table),
            found:   Vec::new(),
        };
        references.visit_body(parsed.suite());
        let found = references.found;

        let lines = line_index(self.db, file);
        let mut occurrences = Vec::new();
        let mut infos = Vec::new();
        for def in table.defs() {
            let targets = self.targets(file, &def.symbol);
            if *targets != [def.symbol.clone()] {
                for target in targets.iter() {
                    occurrences.push(occurrence(
                        &lines,
                        def.range,
                        target.clone(),
                        SymbolRole::WriteAccess,
                    ));
                }
            } else if def.first || !def.kind.assigns() {
                occurrences.push(occurrence(
                    &lines,
                    def.range,
                    def.symbol.clone(),
                    SymbolRole::Definition,
                ));
                if def.first {
                    infos.push(self.info(file, &table, def));
                }
            } else {
                occurrences.push(occurrence(
                    &lines,
                    def.range,
                    def.symbol.clone(),
                    SymbolRole::WriteAccess,
                ));
            }
        }
        for (range, symbol, role) in found {
            occurrences.push(occurrence(&lines, range, symbol, role));
        }
        occurrences.sort_by(|left, right| left.range.cmp(&right.range));

        Document {
            language: "python".into(),
            relative_path: path,
            occurrences,
            symbols: infos,
            position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
            ..Document::default()
        }
    }

    fn info(&mut self, file: File, table: &Table, def: &Def) -> SymbolInformation {
        let kind = match def.kind {
            Kind::Class => symbol_information::Kind::Class,
            Kind::Function => symbol_information::Kind::Function,
            Kind::Method => symbol_information::Kind::Method,
            Kind::Attribute | Kind::SelfAttribute => symbol_information::Kind::Field,
            Kind::Variable => symbol_information::Kind::Variable,
        };
        // a constructor runs only for its own class, `Foo()` or `super().__init__()`
        // never dispatches to a subclass's, so it implements nothing
        let constructor = matches!(def.name.as_str(), "__init__" | "__new__");
        let relationships = match (def.kind, table.owner(def)) {
            (Kind::Method, Some(class)) if !constructor => {
                self.overridden(file, class.range, &def.name)
                    .into_iter()
                    .map(|symbol| {
                        Relationship {
                            symbol,
                            is_implementation: true,
                            is_reference: true,
                            ..Relationship::default()
                        }
                    })
                    .collect()
            },
            (
                Kind::Class
                | Kind::Function
                | Kind::Method
                | Kind::Attribute
                | Kind::SelfAttribute
                | Kind::Variable,
                _,
            ) => Vec::new(),
        };
        SymbolInformation {
            symbol: def.symbol.clone(),
            display_name: def.name.clone(),
            kind: kind.into(),
            relationships,
            ..SymbolInformation::default()
        }
    }

    /// The methods named `name` the ancestors of the class named at `range`
    /// in `file` define, which a method of that name on it overrides.
    fn overridden(&mut self, file: File, range: TextRange, name: &str) -> Vec<String> {
        let mut found = Vec::new();
        for ancestor in self.ancestors(file, range).iter() {
            let method = symbols::method(&ancestor.symbol, name);
            if self.table(ancestor.file).def(&method).is_some() {
                found.push(method);
            }
        }
        found
    }

    /// What binding `symbol` in `file` binds: the symbol itself, or for
    /// `self.name = ..` an ancestor declares, the attributes of the topmost
    /// ancestors declaring it.
    fn targets(&mut self, file: File, symbol: &str) -> Rc<[String]> {
        if let Some(cached) = self.targets.get(symbol) {
            return Rc::clone(cached);
        }
        let table = self.table(file);
        let mut declared = Vec::new();
        if let Some(def) = table.def(symbol)
            && def.kind == Kind::SelfAttribute
            && let Some(class) = table.owner(def)
        {
            let ancestors = self.ancestors(file, class.range);
            let declaring: Vec<&Class> = ancestors
                .iter()
                .filter(|ancestor| {
                    let attribute = symbols::term(&ancestor.symbol, &def.name);
                    self.table(ancestor.file).def(&attribute).is_some()
                })
                .collect();
            for ancestor in &declaring {
                let above = self.ancestors(ancestor.file, ancestor.range);
                if !declaring
                    .iter()
                    .any(|other| above.iter().any(|over| over.symbol == other.symbol))
                {
                    declared.push(symbols::term(&ancestor.symbol, &def.name));
                }
            }
        }
        if declared.is_empty() {
            declared.push(symbol.to_owned());
        }
        let targets: Rc<[String]> = declared.into();
        self.targets.insert(symbol.to_owned(), Rc::clone(&targets));
        targets
    }

    /// The ancestors of the class named at `range` in `file`, nearest first,
    /// leaving out the implicit `object`.
    fn ancestors(&mut self, file: File, range: TextRange) -> Rc<[Class]> {
        if let Some(cached) = self.ancestors.get(&(file, range)) {
            return Rc::clone(cached);
        }
        let mut found: Vec<Class> = Vec::new();
        let mut seen = HashSet::new();
        let mut next = 0;
        let mut bases = self.bases(file, range);
        loop {
            for base in bases {
                if seen.insert(base.symbol.clone()) {
                    found.push(base);
                }
            }
            let Some(class) = found.get(next) else {
                break;
            };
            next += 1;
            bases = self.bases(class.file, class.range);
        }
        let ancestors: Rc<[Class]> = found.into();
        self.ancestors.insert((file, range), Rc::clone(&ancestors));
        ancestors
    }

    /// The direct bases of the class named at `range` in `file`, as ty infers
    /// them.
    fn bases(&mut self, file: File, range: TextRange) -> Vec<Class> {
        let (program_file, parsed) = self.parsed(file);
        let model = SemanticModel::new(self.db, program_file);
        let Some(ty) =
            find_class(parsed.suite(), range).and_then(|class| class.inferred_type(&model))
        else {
            return Vec::new();
        };
        let environment = model.program_environment();
        let mut bases = Vec::new();
        for base in ty_python_semantic::type_hierarchy_supertypes(self.db, &environment, ty) {
            let base_file = base.file.file(self.db);
            if let Some(def) = self.table(base_file).at(base.selection_range)
                && def.symbol != self.object
            {
                bases.push(Class {
                    file:   base_file,
                    range:  def.range,
                    symbol: def.symbol.clone(),
                });
            }
        }
        bases
    }

    fn parsed(&self, file: File) -> (ProgramFile<'db>, ParsedModuleRef) {
        let program_file = self.db.program().program_file(self.db, file);
        let parsed = parsed_module(self.db, program_file.python_file(self.db)).load(self.db);
        (program_file, parsed)
    }

    /// The symbols `file` defines.
    fn table(&mut self, file: File) -> Rc<Table> {
        if let Some(cached) = self.tables.get(&file) {
            return Rc::clone(cached);
        }
        let (program_file, parsed) = self.parsed(file);
        let table = Rc::new(Table::build(
            parsed.syntax(),
            self.module(file, program_file),
        ));
        self.tables.insert(file, Rc::clone(&table));
        table
    }

    /// The symbol of the module `file` is: the project's for first-party
    /// files, [`STDLIB`]'s at the target version for the standard library's,
    /// and its top-level module's for a third-party one.
    fn module(&self, file: File, program_file: ProgramFile<'_>) -> String {
        let db = self.db;
        let module = ty_module_resolver::file_to_module(db, program_file.resolver_file(db));
        let path = file.path(db);
        let name = module.map_or_else(
            || module_name_from_path(path.as_str(), self.root),
            |resolved| resolved.name(db).to_string(),
        );
        let search_path = module.and_then(|resolved| resolved.search_path(db));
        let first_party = search_path.map_or_else(
            || {
                path.as_system_path()
                    .is_some_and(|system| system.starts_with(self.root))
            },
            SearchPath::is_first_party,
        );
        if search_path.is_some_and(SearchPath::is_standard_library) {
            symbols::module(STDLIB, &db.python_version().version.to_string(), &name)
        } else if first_party {
            symbols::module(&self.name, &self.version, &name)
        } else {
            symbols::module(name.split('.').next().unwrap_or(&name), ".", &name)
        }
    }

    /// The symbols of the definition whose name is at `range` in `file`.
    fn symbols_at(&mut self, file: File, range: TextRange) -> Rc<[String]> {
        let table = self.table(file);
        table
            .at(range)
            .map_or_else(|| Rc::from([]), |def| self.targets(file, &def.symbol))
    }

    /// The symbols of what `definitions` name, skipping locals.
    fn resolve(&mut self, definitions: Vec<ResolvedDefinition<'_>>) -> Vec<String> {
        let mut symbols = Vec::new();
        for definition in definitions {
            let found = if let ResolvedDefinition::Module(module) = definition {
                Rc::from([self.table(module.file(self.db)).module().to_owned()])
            } else {
                let target = definition.focus_range(self.db);
                self.symbols_at(target.file(), target.range())
            };
            for symbol in found.iter() {
                if !symbols.contains(symbol) {
                    symbols.push(symbol.clone());
                }
            }
        }
        symbols
    }
}

fn occurrence(lines: &LineIndex, range: TextRange, symbol: String, role: SymbolRole) -> Occurrence {
    Occurrence {
        range: position(lines, range),
        symbol,
        symbol_roles: role as i32,
        ..Occurrence::default()
    }
}

/// `range` as SCIP has it: start line, start column, end line unless it's the
/// same, end column, all zero-based, columns in UTF-8 bytes
fn position(lines: &LineIndex, range: TextRange) -> Vec<i32> {
    let int = |value: usize| i32::try_from(value).unwrap_or(i32::MAX);
    let start = lines.line_index(range.start()).to_zero_indexed();
    let end = lines.line_index(range.end()).to_zero_indexed();
    let start_column = int(usize::from(range.start() - lines.line_starts()[start]));
    let end_column = int(usize::from(range.end() - lines.line_starts()[end]));
    if start == end {
        vec![int(start), start_column, end_column]
    } else {
        vec![int(start), start_column, int(end), end_column]
    }
}

/// `a/b/__init__.py` -> `a.b`, for a file outside the search paths
fn module_name_from_path(path: &str, root: &SystemPath) -> String {
    let relative = path
        .strip_prefix(root.as_str())
        .unwrap_or(path)
        .trim_start_matches('/');
    let stem = relative
        .strip_suffix(".pyi")
        .or_else(|| relative.strip_suffix(".py"))
        .unwrap_or(relative);
    stem.strip_suffix("/__init__")
        .unwrap_or(stem)
        .replace('/', ".")
}

fn find_class(body: &[ast::Stmt], range: TextRange) -> Option<&ast::StmtClassDef> {
    struct Find<'ast> {
        range: TextRange,
        found: Option<&'ast ast::StmtClassDef>,
    }

    impl<'ast> visitor::Visitor<'ast> for Find<'ast> {
        fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
            if self.found.is_some() || !stmt.range().contains_range(self.range) {
                return;
            }
            if let ast::Stmt::ClassDef(ref class) = *stmt
                && class.name.range() == self.range
            {
                self.found = Some(class);
                return;
            }
            visitor::walk_stmt(self, stmt);
        }

        // classes only nest in statements
        fn visit_expr(&mut self, _expr: &'ast Expr) {}
    }

    let mut find = Find { range, found: None };
    find.visit_body(body);
    find.found
}

/// Every reference in a file: the range of the name it's at, and what it
/// does there.
struct References<'indexer, 'db> {
    indexer: &'indexer mut Indexer<'db>,
    model:   SemanticModel<'db>,
    table:   Rc<Table>,
    found:   Vec<(TextRange, String, SymbolRole)>,
}

impl<'db> References<'_, 'db> {
    fn found(&mut self, range: TextRange, symbols: Vec<String>, role: SymbolRole) {
        self.found
            .extend(symbols.into_iter().map(|symbol| (range, symbol, role)));
    }

    fn name(&self, name: &ast::ExprName) -> Vec<ResolvedDefinition<'db>> {
        ty_python_semantic::definitions_for_name(
            &self.model,
            name.id.as_str(),
            name.into(),
            ImportAliasResolution::ResolveAliases,
        )
    }

    fn attribute(&self, attribute: &ast::ExprAttribute) -> Vec<ResolvedDefinition<'db>> {
        ty_python_semantic::definitions_for_attribute(&self.model, attribute)
    }

    /// The module `import` names, relative to this file for a `level` above 0.
    fn module(&mut self, name: Option<&str>, level: u32) -> Vec<String> {
        let db = self.indexer.db;
        self.model
            .resolve_module(name, level)
            .and_then(|module| module.file(db))
            .map(|file| self.indexer.table(file).module().to_owned())
            .into_iter()
            .collect()
    }

    /// What a call's callee names, and what ty says the call runs: the same
    /// for a function, but `__init__` for a class, `__call__` for an object,
    /// and the base's method for `super().method`. A keyword argument names
    /// a parameter, which is local, or a field, as a dataclass's do.
    fn call(&mut self, call: &ast::ExprCall) {
        for keyword in &call.arguments.keywords {
            if let Some(ref arg) = keyword.arg {
                let definitions = definitions_for_keyword_argument(&self.model, keyword, call);
                let fields = self.indexer.resolve(definitions);
                self.found(arg.range(), fields, SymbolRole::WriteAccess);
            }
        }

        let (range, mut definitions) = if let Expr::Name(ref name) = *call.func {
            (name.range(), self.name(name))
        } else if let Expr::Attribute(ref attribute) = *call.func {
            (attribute.attr.range(), self.attribute(attribute))
        } else {
            return;
        };
        definitions.extend(
            call_signature_details(&self.model, call)
                .into_iter()
                .filter_map(|details| details.definition.map(ResolvedDefinition::Definition)),
        );
        let symbols = self.indexer.resolve(definitions);
        self.found(range, symbols, SymbolRole::ReadAccess);
    }

    /// `import a.b` names the module, `from a import b` the module and what
    /// it takes from it
    fn import(&mut self, stmt: &ast::Stmt) {
        if let ast::Stmt::Import(ref import) = *stmt {
            for alias in &import.names {
                let module = self.module(Some(alias.name.as_str()), 0);
                self.found(alias.name.range(), module, SymbolRole::Import);
            }
        } else if let ast::Stmt::ImportFrom(ref import) = *stmt {
            if let Some(ref name) = import.module {
                let module = self.module(Some(name.as_str()), import.level);
                self.found(name.range(), module, SymbolRole::Import);
            }
            for alias in &import.names {
                if alias.name.as_str() == "*" {
                    continue;
                }
                let definitions = ty_python_semantic::definitions_for_imported_symbol(
                    &self.model,
                    import,
                    alias.name.as_str(),
                    ImportAliasResolution::ResolveAliases,
                );
                let mut imported = self.indexer.resolve(definitions);
                // ty finds no symbol for a submodule, `from pkg import module`
                if imported.is_empty() {
                    let submodule = import.module.as_ref().map_or_else(
                        || alias.name.to_string(),
                        |name| format!("{name}.{}", alias.name),
                    );
                    imported = self.module(Some(&submodule), import.level);
                }
                self.found(alias.name.range(), imported, SymbolRole::Import);
            }
        }
    }
}

const fn access(context: ExprContext) -> SymbolRole {
    if matches!(context, ExprContext::Store | ExprContext::Del) {
        SymbolRole::WriteAccess
    } else {
        SymbolRole::ReadAccess
    }
}

impl<'ast> visitor::Visitor<'ast> for References<'_, '_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        if matches!(*stmt, ast::Stmt::Import(_) | ast::Stmt::ImportFrom(_)) {
            self.import(stmt);
        } else {
            visitor::walk_stmt(self, stmt);
        }
    }

    // a name at a definition is a binding the table has, not a reference
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Name(ref name) = *expr {
            if self.table.at(name.range()).is_none() {
                let symbols = self.indexer.resolve(self.name(name));
                self.found(name.range(), symbols, access(name.ctx));
            }
        } else if let Expr::Attribute(ref attribute) = *expr {
            if self.table.at(attribute.attr.range()).is_none() {
                let symbols = self.indexer.resolve(self.attribute(attribute));
                self.found(attribute.attr.range(), symbols, access(attribute.ctx));
            }
            self.visit_expr(&attribute.value);
        } else if let Expr::Call(ref call) = *expr {
            self.call(call);
            // the callee itself is resolved, only what it's looked up on is left
            if let Expr::Attribute(ref attribute) = *call.func {
                self.visit_expr(&attribute.value);
            } else if !matches!(*call.func, Expr::Name(_)) {
                self.visit_expr(&call.func);
            }
            self.visit_arguments(&call.arguments);
        } else {
            visitor::walk_expr(self, expr);
        }
    }
}
