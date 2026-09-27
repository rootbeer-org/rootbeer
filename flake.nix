{
  description = "rootbeer - lua-based system configuration tool";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    systems.url = "github:nix-systems/default";

    flake-parts.url = "github:hercules-ci/flake-parts";

    crane.url = "github:ipetkov/crane";
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      systems = import inputs.systems.outPath;

      perSystem =
        {
          pkgs,
          lib,
          system,
          ...
        }:
        let
          craneLib = inputs.crane.mkLib pkgs;

          # build.rs embeds the stdlib and package definitions; `rb licenses` embeds licenses/.
          luaFilter = path: _type: builtins.match ".*(lua|packages|licenses)/.*" path != null;
          src = lib.cleanSourceWith {
            src = ./.;
            filter = path: type: (luaFilter path type) || (craneLib.filterCargoSources path type);
          };

          commonArgs = {
            inherit src;
            strictDeps = true;
            pname = "rootbeer";
            version = "0.1.0";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          rootbeer = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "--package rootbeer-cli";
              meta = { license = lib.licenses.mit; };
            }
          );
          forge = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "rootbeer-forge";
              cargoExtraArgs = "--package rootbeer-forge";
              meta = { license = lib.licenses.mit; };
            }
          );
        in
        {
          packages = {
            default = rootbeer;
            inherit rootbeer;
            rootbeer-forge = forge;
          };

          checks = {
            rootbeer-clippy = craneLib.cargoClippy (
              commonArgs
              // {
                inherit cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- -D warnings";
              }
            );
            rootbeer-fmt = craneLib.cargoFmt { inherit src; };
          };

          devShells.default = craneLib.devShell {
            checks = {
              inherit rootbeer;
            };
            packages = [ ];
          };
        };
    };
}
