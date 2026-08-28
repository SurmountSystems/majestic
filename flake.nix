{
  description = "A personal memex with agentic support for lossless ingest, efficient encoding, and ripgrep-style search of account data exports";

  # Small input surface (BitMagi-style, single crate): nixpkgs + fenix + crane.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      fenix,
      crane,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      perSystem = forAllSystems (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          inherit (pkgs) lib;

          # Match rust-toolchain.toml (channel 1.98.0). Update sha256 when changing channel.
          # sha256 is the rustup channel manifest (channel-rust-1.98.0.toml).
          rustToolchain = fenix.packages.${system}.fromToolchainFile {
            file = ./rust-toolchain.toml;
            sha256 = "sha256-P30Tm3O7vQAE725YtDCDHGjNrSsfZO4us11UwJGZSJo=";
          };

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          src = lib.cleanSourceWith {
            src = self;
            filter =
              path: type:
              let
                pathStr = toString path;
                base = baseNameOf path;
                # Host .cargo/config.toml passes -fuse-ld=wild. That linker is not
                # in the Nix sandbox; omit the cargo config from the flake source.
                isCargoConfig = lib.hasInfix "/.cargo/" pathStr || lib.hasSuffix "/.cargo" pathStr;
              in
              !isCargoConfig
              && (
                (craneLib.filterCargoSources path type) || base == "rust-toolchain.toml" || base == "rustfmt.toml"
              );
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
          ];

          buildInputs = with pkgs; [
            sqlite
            # libpcre2 for grep-pcre2 (lookahead AND). Not optional.
            pcre2
          ];

          commonArgs = {
            inherit src nativeBuildInputs buildInputs;
            strictDeps = true;
            pname = "memex";
            version = "0.1.0";
          };

          cargoArtifacts = craneLib.buildDepsOnly commonArgs;

          memex = craneLib.buildPackage (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoExtraArgs = "--bin memex";
              # Operator runs `just check` (fmt, clippy, nextest). Binary-only here.
              doCheck = false;
              meta = {
                description = "A personal memex with agentic support for lossless ingest, efficient encoding, and ripgrep-style search of account data exports";
                license = lib.licenses.unlicense;
                mainProgram = "memex";
              };
            }
          );

          # Optional AVX2-class baseline on x86_64 only. Do not use
          # target-cpu=native (this laptop is not other machines). Dev
          # shell only; not the nix package build for all systems.
          rustflagsX86 = lib.optionalString (lib.hasPrefix "x86_64-" system) "-C target-cpu=x86-64-v3";

          devShell = pkgs.mkShell (
            {
              packages = [
                rustToolchain
                pkgs.just
                pkgs.cargo-nextest
                pkgs.pkg-config
                pkgs.sqlite
                pkgs.pcre2
              ];
              inherit nativeBuildInputs buildInputs;
              RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            }
            // lib.optionalAttrs (rustflagsX86 != "") {
              RUSTFLAGS = rustflagsX86;
            }
          );
        in
        {
          packages = {
            default = memex;
            inherit memex;
          };
          devShells.default = devShell;
        }
      );
    in
    {
      packages = forAllSystems (system: perSystem.${system}.packages);
      devShells = forAllSystems (system: perSystem.${system}.devShells);
    };
}
