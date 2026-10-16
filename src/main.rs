// SPDX-License-Identifier: EUPL-1.2

mod db;
mod index;
mod symbols;

use std::{
    env,
    fs,
    panic::{
        AssertUnwindSafe,
        catch_unwind,
    },
    path::{
        Path,
        PathBuf,
    },
    process::ExitCode,
};

use misstep::{
    OptionExt as _,
    Result,
    ResultExt as _,
};
use pound::Parse;
use protobuf::{
    Message as _,
    MessageField,
};
use ruff_db::{
    files::system_path_to_file,
    system::{
        SystemPath,
        SystemPathBuf,
    },
};
use scip::types::{
    Index,
    Metadata,
    ProtocolVersion,
    TextEncoding,
    ToolInfo,
};
use url::Url;

use crate::{
    db::Database,
    index::Indexer,
};

/// SCIP indexer for Python, built on ty
#[derive(Parse)]
#[pound(name = "scip-python", help_if_empty)]
enum Cli {
    /// index the Python project in the working directory
    Index {
        /// the project's package name in symbols (default: the directory's
        /// name)
        #[pound(long)]
        project_name:    Option<String>,
        /// the project's package version in symbols
        #[pound(long, default = "0")]
        project_version: String,
        /// where to write the index
        #[pound(long, default = "index.scip")]
        output:          PathBuf,
        /// the project to index (default: the working directory)
        #[pound(long)]
        cwd:             Option<PathBuf>,
        /// ignored: the python environment is discovered, like ty does, from
        /// `VIRTUAL_ENV`, `CONDA_PREFIX` or a `.venv` in the project
        #[pound(long)]
        environment:     Option<PathBuf>,
        /// print nothing but errors
        #[pound(long)]
        quiet:           bool,
    },
}

/// directories that hold no project sources
const SKIP: &[&str] = &["__pycache__", "node_modules", "site-packages"];

/// The project's Python sources under `root`, in order, honouring
/// `.gitignore` and skipping virtualenvs
fn sources(root: &SystemPath) -> Vec<SystemPathBuf> {
    let mut paths: Vec<_> = ignore::WalkBuilder::new(root)
        .filter_entry(|entry| {
            let is_dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            let file_name = entry.file_name().to_string_lossy();
            // virtualenvs are found by their pyvenv.cfg, whatever their name
            !(is_dir && (SKIP.contains(&&*file_name) || entry.path().join("pyvenv.cfg").is_file()))
        })
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|kind| !kind.is_dir()))
        .filter_map(|entry| SystemPathBuf::from_path_buf(entry.into_path()).ok())
        .filter(|path| matches!(path.extension(), Some("py" | "pyi")))
        .collect();
    paths.sort();
    paths
}

fn index(
    dir: &Path,
    project_name: Option<String>,
    version: String,
    output: &Path,
    quiet: bool,
) -> Result<()> {
    let canonical =
        fs::canonicalize(dir).with_context(|| format!("resolving {}", dir.display()))?;
    let root = SystemPathBuf::from_path_buf(canonical)
        .ok()
        .context("the project path is not UTF-8")?;
    let name = project_name.unwrap_or_else(|| root.file_name().unwrap_or("project").to_owned());
    let db = Database::new(&root)?;

    let mut indexer = Indexer::new(&db, &root, name, version);
    let paths = sources(&root);
    let mut documents = Vec::with_capacity(paths.len());
    for path in &paths {
        let Ok(relative) = path
            .strip_prefix(&root)
            .map(|rest| rest.as_str().to_owned())
        else {
            continue;
        };
        let Ok(file) = system_path_to_file(&db, path) else {
            eprintln!("warning: skipping {relative}, it is not readable");
            continue;
        };
        // ty may panic on code it doesn't handle yet; lose the file, not the
        // index
        match catch_unwind(AssertUnwindSafe(|| {
            indexer.document(file, relative.clone())
        })) {
            Ok(document) => documents.push(document),
            Err(_) => eprintln!("warning: skipping {relative}, indexing it panicked"),
        }
    }

    let project_root = Url::from_directory_path(root.as_std_path())
        .ok()
        .context("the project path is not absolute")?;
    let count = documents.len();
    let index = Index {
        metadata: MessageField::some(Metadata {
            version: ProtocolVersion::UnspecifiedProtocolVersion.into(),
            tool_info: MessageField::some(ToolInfo {
                name: "scip-python".into(),
                version: env!("CARGO_PKG_VERSION").into(),
                arguments: env::args().skip(1).collect(),
                ..ToolInfo::default()
            }),
            project_root: project_root.into(),
            text_document_encoding: TextEncoding::UTF8.into(),
            ..Metadata::default()
        }),
        documents,
        ..Index::default()
    };
    let bytes = index.write_to_bytes().context("encoding the index")?;
    fs::write(output, bytes).with_context(|| format!("writing {}", output.display()))?;
    if !quiet {
        eprintln!("indexed {count} files into {}", output.display());
    }
    Ok(())
}

fn main() -> ExitCode {
    let result = match Cli::parse() {
        Cli::Index {
            project_name,
            project_version,
            output,
            cwd,
            quiet,
            ..
        } => {
            index(
                cwd.as_deref().unwrap_or_else(|| Path::new(".")),
                project_name,
                project_version,
                &output,
                quiet,
            )
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(report) => {
            eprintln!("error: {report}");
            ExitCode::FAILURE
        },
    }
}
