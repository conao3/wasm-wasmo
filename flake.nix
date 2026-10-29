{
  description = "wasmo - stateful in-process AWS mock for every language, powered by a single WebAssembly module";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, nixpkgs, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-darwin"
      ];

      perSystem =
        { system, ... }:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };
          rust = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "clippy"
              "rustfmt"
            ];
            targets = [ "wasm32-unknown-unknown" ];
          };
        in
        {
          formatter = pkgs.nixfmt;

          devShells.default = pkgs.mkShell {
            packages = [
              rust
              pkgs.go
              pkgs.nodejs
              pkgs.bun
              pkgs.wabt
              pkgs.gnumake
              pkgs.jq
              pkgs.jdk17
              pkgs.maven
              pkgs.dotnet-sdk_8
              (pkgs.python3.withPackages (ps: [
                ps.boto3
                ps.build
                ps.hatchling
                ps.pytest
                ps.wasmtime
              ]))
              pkgs.ruff
            ];
            DOTNET_CLI_TELEMETRY_OPTOUT = "1";
            DOTNET_NOLOGO = "1";
          };
        };
    };
}
