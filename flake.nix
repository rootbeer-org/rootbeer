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

          # Cargo embeds these. build.rs embeds the stdlib, `rb licenses` embeds
          # licenses/, tests embed fixtures/, and `rb drv install` embeds the
          # trusted root.json.
          embedFilter = path: _type: builtins.match ".*/(lua|licenses|fixtures)/.*|.*/root\\.json" path != null;
          src = lib.cleanSourceWith {
            src = ./.;
            filter = path: type: (embedFilter path type) || (craneLib.filterCargoSources path type);
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
        in
        {
          packages = {
            default = rootbeer;
            inherit rootbeer;
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
