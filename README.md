# scip-python

a [SCIP](https://github.com/sourcegraph/scip) indexer for python, built on
[ty](https://github.com/astral-sh/ty)'s semantic model instead of a pyright fork.

## usage

```
scip-python index [--project-name <name>] [--project-version <version>]
                  [--output <path>] [--cwd <dir>] [--quiet]
```

indexes every `.py` and `.pyi` file under `--cwd` (default: the working
directory), honouring `.gitignore` and skipping virtualenvs, and writes
`index.scip` unless `--output` says otherwise. `--environment` is accepted and
ignored: like ty, scip-python finds the python environment itself, from
`VIRTUAL_ENV`, `CONDA_PREFIX` or a `.venv` in the project, and resolves
third-party imports against its site-packages. stdlib imports resolve against
ty's vendored typeshed.

## what's in an index

- definitions of classes, functions, methods, class attributes, attributes
  methods assign on `self`, and module-level variables
- references to them, resolved by ty: names and attributes, marked as reads
  or writes, the modules and names import statements name, and keyword
  arguments naming a field, as a dataclass's do
- assigning an attribute again writes it rather than defining it, and
  `self.name = ..` writes the attribute of the topmost ancestor that declares
  one
- `Foo(..)` as a reference to the `__init__` it runs as well as to `Foo`, and
  `super().__init__(..)` to the base's `__init__`
- each method's `is_implementation` relationship to the ancestor methods it
  overrides

symbols follow scip-python's shape:

```
scip-python python <package> <version> `pkg.module`/Class#method().
```

`<package>` and `<version>` are the project's for first-party files,
`python-stdlib` and the target python version for the stdlib, and the
top-level module and `.` for third-party packages.

locals, parameters, hover documentation and diagnostics are left out.

## building

```
nix build
```

or `cargo build --release` in `nix develop`. `nix flake check` builds, runs
clippy and checks formatting; `nix develop .#fmt` formats.

the ty crates (`ty_python_semantic` and friends) are internal ruff crates with
no stable API, so they're pinned to one exact version.

## license

[EUPL-1.2](LICENSE)
