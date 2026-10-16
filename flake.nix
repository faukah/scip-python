# SPDX-License-Identifier: EUPL-1.2

{
  description = "SCIP indexer for Python, built on ty";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      forAllSystems = lib.genAttrs (lib.systems.doubles.linux ++ lib.systems.doubles.darwin);
      pkgsFor = system: nixpkgs.legacyPackages.${system} or (import nixpkgs { inherit system; });

      # .rustfmt.toml sets unstable options
      nightlyRustfmt = pkgs: pkgs.rustfmt.override { asNightly = true; };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = self.packages.${system}.scip-python;

          scip-python = pkgs.callPackage ./nix/package.nix { };
        }
      );

      apps = forAllSystems (
        system:
        let
          scip-python = self.packages.${system}.scip-python;
        in
        {
          default = {
            type = "app";
            program = lib.getExe scip-python;
            inherit (scip-python) meta;
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.rustc
              pkgs.clippy
              (nightlyRustfmt pkgs)
              pkgs.rust-analyzer
              pkgs.taplo
              pkgs.nixfmt
            ];

            env = {
              RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
            };
          };

          # `nix develop .#fmt` formats on entry
          fmt = pkgs.mkShellNoCC {
            packages = [
              pkgs.cargo
              (nightlyRustfmt pkgs)
              pkgs.taplo
              pkgs.nixfmt
            ];
            shellHook = ''
              cargo fmt
              taplo fmt
              find . -name '*.nix' -not -path './target/*' -exec nixfmt {} +
            '';
          };
        }
      );

      checks = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          inherit (self.packages.${system}) scip-python;

          fmt =
            pkgs.runCommand "scip-python-fmt-check"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  (nightlyRustfmt pkgs)
                  pkgs.taplo
                  pkgs.nixfmt
                ];
                src = ./.;
              }
              ''
                cp -r $src ./tree
                chmod -R +w ./tree
                cd ./tree
                cargo fmt -- --check
                taplo fmt --check
                find . -name '*.nix' -exec nixfmt --check {} +
                touch $out
              '';

          clippy = self.packages.${system}.scip-python.overrideAttrs (old: {
            pname = "scip-python-clippy";
            nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.clippy ];
            buildPhase = ''
              runHook preBuild
              cargo clippy --all-targets --offline -- -D warnings
              runHook postBuild
            '';
            doCheck = false;
            installPhase = ''
              runHook preInstall
              touch $out
              runHook postInstall
            '';
          });
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt-tree);
    };
}
