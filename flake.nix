{
  description = "ffs - fork-based deterministic concurrency testing (rr-style record/replay with checkpoints)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, flake-utils, flake-compat }:
    flake-utils.lib.eachSystem [ "x86_64-linux" ] (system:
      let
        pkgs = import nixpkgs { inherit system; };
        nativeDeps = with pkgs; [
          cargo
          rustc
          rustfmt
          clippy
          rust-analyzer
          gcc
          gnumake
          pkg-config
          perf
          gdb
          strace
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          packages = nativeDeps;
          RUST_SRC_PATH = "${pkgs.rust.packages.stable.rustPlatform.rustLibSrc}";
          shellHook = ''
            export CC=${pkgs.gcc}/bin/gcc
          '';
        };

        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "ffs";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.gcc ];
          # Tests need ptrace + perf counters; run them from the dev shell instead.
          doCheck = false;
        };
      });
}
