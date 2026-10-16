// SPDX-License-Identifier: EUPL-1.2

use std::sync::Arc;

use misstep::{
    Result,
    ResultExt as _,
};
use ruff_db::{
    Db as SourceDb,
    diagnostic::Diagnostic,
    files::{
        File,
        Files,
    },
    system::{
        OsSystem,
        System,
        SystemPath,
        SystemPathBuf,
    },
    vendored::VendoredFileSystem,
};
use ty_module_resolver::SearchPathSettings;
use ty_python_core::{
    ProgramFile,
    platform::PythonPlatform,
    program::{
        FallibleStrategy,
        Program,
        ProgramSettings,
    },
};
use ty_python_semantic::{
    AnalysisSettings,
    PythonEnvironment,
    PythonVersionWithSource,
    dependency::DependencyMetadata,
    lint::{
        LintRegistry,
        RuleSelection,
    },
};

/// the settings a one-shot index run never changes
struct Settings {
    program:  ProgramSettings,
    rules:    RuleSelection,
    analysis: AnalysisSettings,
}

/// a salsa database over one project and its python environment, the
/// bare minimum ty's semantic queries need
#[salsa::db]
#[derive(Clone)]
pub struct Database {
    files:    Files,
    system:   OsSystem,
    settings: Arc<Settings>,
    storage:  salsa::Storage<Self>,
}

impl Database {
    /// a database for the project at `root`, resolving imports against the
    /// project itself, the python environment ty discovers for it (an active
    /// or local virtualenv, conda) and the vendored typeshed
    pub fn new(root: &SystemPath) -> Result<Self> {
        let system = OsSystem::new(root);
        let environment = PythonEnvironment::discover(Some(root), &system)
            .context("discovering the python environment")?;
        let site_packages = environment
            .as_ref()
            .map(|found| found.site_packages_paths(&system))
            .transpose()
            .context("finding site-packages")?
            .unwrap_or_default();
        let python_version = environment
            .as_ref()
            .and_then(PythonEnvironment::python_version_from_metadata)
            .cloned()
            .or_else(|| site_packages.python_version_from_layout())
            .unwrap_or_default();
        let real_stdlib_path = environment
            .as_ref()
            .and_then(|found| found.real_stdlib_path(&system).ok());

        let search_paths = SearchPathSettings {
            src_roots: src_roots(root, &system),
            site_packages_paths: site_packages.into_vec(),
            real_stdlib_path,
            ..SearchPathSettings::empty()
        }
        .to_search_paths(&system, ty_vendored::file_system(), &FallibleStrategy)
        .context("resolving the module search paths")?;

        let settings = Settings {
            program:  ProgramSettings {
                python_version,
                python_platform: PythonPlatform::default(),
                search_paths,
                python_environment: Ok(environment),
            },
            rules:    RuleSelection::from_registry(ty_python_semantic::default_lint_registry()),
            analysis: AnalysisSettings::default(),
        };

        Ok(Self {
            files: Files::default(),
            system,
            settings: Arc::new(settings),
            storage: salsa::Storage::new(None),
        })
    }

    pub fn program(&self) -> Program<'_> {
        program(self)
    }

    pub fn python_version(&self) -> &PythonVersionWithSource {
        &self.settings.program.python_version
    }
}

/// first-party roots the way ty picks them: the project, and a `src`,
/// `python` or `<project>` directory that isn't a package itself
fn src_roots(root: &SystemPath, system: &dyn System) -> Vec<SystemPathBuf> {
    let is_package = |dir: &SystemPath| {
        system.is_file(&dir.join("__init__.py")) || system.is_file(&dir.join("__init__.pyi"))
    };
    let mut roots = vec![root.to_path_buf()];
    let name = root.file_name().unwrap_or_default();
    for child in ["src", "python", name] {
        let dir = root.join(child);
        if system.is_directory(&dir) && !is_package(&dir) && !roots.contains(&dir) {
            roots.push(dir);
        }
    }
    roots
}

#[salsa::db]
trait ProgramDb: ty_python_core::Db {
    fn program_settings(&self) -> &ProgramSettings;
}

/// salsa-cached, as interning a program hashes every search path
#[salsa::tracked(returns(copy))]
fn program(db: &dyn ProgramDb) -> Program<'_> {
    Program::from_settings(db, db.program_settings())
}

#[salsa::db]
impl ProgramDb for Database {
    fn program_settings(&self) -> &ProgramSettings {
        &self.settings.program
    }
}

#[salsa::db]
impl SourceDb for Database {
    fn vendored(&self) -> &VendoredFileSystem {
        ty_vendored::file_system()
    }

    fn system(&self) -> &dyn System {
        &self.system
    }

    fn files(&self) -> &Files {
        &self.files
    }
}

#[salsa::db]
impl ty_module_resolver::Db for Database {}

#[salsa::db]
impl ty_python_core::Db for Database {
    fn should_check_file(&self, file: File) -> bool {
        !file.path(self).is_vendored_path()
    }
}

#[salsa::db]
impl ty_python_semantic::Db for Database {
    // indexing infers what it needs and reports nothing
    fn check_file(&self, _file: File) -> Vec<Diagnostic> {
        Vec::new()
    }

    fn program_file(&self, file: File) -> ProgramFile<'_> {
        self.program().program_file(self, file)
    }

    fn python_version_with_source(&self, _file: File) -> &PythonVersionWithSource {
        self.python_version()
    }

    fn rule_selection(&self, _file: File) -> &RuleSelection {
        &self.settings.rules
    }

    fn lint_registry(&self) -> &LintRegistry {
        ty_python_semantic::default_lint_registry()
    }

    fn analysis_settings(&self, _file: File) -> &AnalysisSettings {
        &self.settings.analysis
    }

    fn dependency_metadata(&self, _file: File) -> Option<&DependencyMetadata> {
        None
    }

    fn verbose(&self) -> bool {
        false
    }

    fn is_open_file(&self, _file: File) -> bool {
        false
    }

    fn dyn_clone(&self) -> Box<dyn ty_python_semantic::Db> {
        Box::new(self.clone())
    }
}

#[salsa::db]
impl salsa::Database for Database {}
