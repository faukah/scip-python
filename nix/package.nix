# SPDX-License-Identifier: EUPL-1.2
{
  lib,
  rustPlatform,
}:
let
  cargo = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "scip-python";
  inherit (cargo.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  meta = {
    inherit (cargo.package) description;
    homepage = cargo.package.repository;
    license = lib.licenses.eupl12;
    mainProgram = "scip-python";
  };
}
