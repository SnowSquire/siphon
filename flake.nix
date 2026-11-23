{
  description = "Siphon — a Twitch HLS client: the daemon, its UI, and the Windows cross build";

  inputs = {
    # Pinned to a commit rather than a channel so a second machine gets the same
    # libplacebo, LLVM and toolchain. `nix flake update` moves both pins.
    nixpkgs.url = "github:NixOS/nixpkgs/2786a2a72088ecf32aea5b3ec52099e6a7cff78f";
    # rust-lang's own toolchain, wrapped: nixpkgs' rustc is built from source
    # and only ships the host standard library, while this can add a target's.
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # Splits a cargo build into a dependency derivation and the crates on top,
    # so a source edit does not recompile the dependency tree. Cargo itself
    # stays the resolver: no generated lockfile-derived expressions.
    # No nixpkgs input: crane is a library over the `pkgs` it is handed.
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      crane,
    }:
    let
      inherit (nixpkgs) lib;
      # The Windows payload is cross-built from Linux (clang-cl + lld-link +
      # the pinned CRT/SDK), and the release workflow runs this flake, so the
      # only host that can produce both is a Linux one.
      systems = [ "x86_64-linux" ];

      # One Rust release for both payloads, with the Windows standard library
      # installed beside the host's. Keep `Cargo.toml`'s `rust-version` floor in
      # mind when bumping; today's stable is 1.100.0.
      rustVersion = "1.100.0";
      windowsTarget = "x86_64-pc-windows-msvc";

      perSystem = lib.genAttrs systems (
        system:
        let
          # nixpkgs' own package set, with one config knob: the CRT/SDK the
          # Windows payload compiles against is Microsoft-licensed, so the
          # unfree check has to be told that this particular derivation is meant
          # to be built. Building the Windows payload means agreeing to those
          # terms.
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
            config = {
              allowUnfreePredicate =
                pkg:
                builtins.elem (pkg.pname or pkg.name or "") [
                  "win-sdk"
                  "win-sdk-cache"
                ];
            };
          };
          sources = import ./nix/sources.nix { inherit pkgs lib; };

          rustToolchain = pkgs.rust-bin.stable.${rustVersion}.default.override {
            targets = [ windowsTarget ];
          };
          # The hooks come with it, so `bindgenHook`, the vendoring hook and the
          # build hook are the same ones nixpkgs' `rustPlatform` would give.
          rustPlatform = pkgs.makeRustPlatform {
            rustc = rustToolchain;
            cargo = rustToolchain;
          };
          version = (lib.importTOML ./Cargo.toml).workspace.package.version;
          # Everything Cargo resolves is vendored from the committed lockfile.
          # `zgui` is a git dependency; crane keys such a hash by the lockfile's
          # whole source string rather than by crate name, and the value is the
          # tree hash of the pinned commit.
          cargoHashes = {
            "git+https://github.com/zortax/zgui?rev=aacaa469ee36eccf73a19bdb83356a7b8cd803c1#aacaa469ee36eccf73a19bdb83356a7b8cd803c1" =
              "sha256-wF8aYcJvME4/YVA6UXB2vrfQXmyWtFCRwLz1h6/NWP0=";
          };
          # crane splits a cargo invocation into a dependency layer and the
          # crates on top; cargo stays the resolver. The toolchain override is
          # what makes it use rust-overlay's rustc rather than nixpkgs'.
          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          # What the Rust packages compile: the manifests, the lockfile and the
          # crates, and nothing else. Editing a note, a workflow or a README
          # then does not invalidate a build. `crates/` carries the non-Rust
          # files the crates embed (fonts, icons, the C shim libplacebo-sys
          # compiles, `index.hexpat`).
          cargoSrc = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
            ];
          };

          # The packaging derivation reads the licences, the fonts' OFL files,
          # the NSIS script and the VC++ bootstrapper; nothing else in the tree
          # belongs in its inputs.
          installerSrc = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./installer
              # The script's icon, and the fonts' OFL texts.
              ./crates/ui/icons
              ./crates/ui/fonts
              ./LICENSE-MIT
              ./LICENSE-APACHE
              ./README.md
            ];
          };

          callPackage = lib.callPackageWith (
            pkgs
            // {
              inherit
                self
                version
                cargoHashes
                cargoSrc
                craneLib
                installerSrc
                sources
                rustPlatform
                rustToolchain
                ;
            }
          );

          # `default` and `linux` are the same derivation; name it once. The
          # installer consumes the Windows payload, so that is one binding too.
          # Both Rust packages return `{ package, clippy }`: the lint shares the
          # dependency artifacts, so it does not recompile them.
          linux = callPackage ./nix/linux.nix { };
          msvc = callPackage ./nix/msvc.nix { };
          windows = callPackage ./nix/windows.nix { inherit msvc; };
          windows-installer = callPackage ./nix/windows-installer.nix {
            windows = windows.package;
          };
        in
        {
          packages = {
            linux = linux.package;
            windows = windows.package;
            inherit windows-installer;
            default = linux.package;

            # The dependency layers as their own outputs: they only change with
            # `Cargo.lock` or the toolchain, so CI caches these instead of the
            # binaries (which change on every commit).
            linux-deps = linux.deps;
            windows-deps = windows.deps;
          };

          checks = {
            linux-clippy = linux.clippy;
            windows-clippy = windows.clippy;
          };

          devShells = {
            default = callPackage ./nix/devshell.nix { };
            windows = callPackage ./nix/winshell.nix { inherit msvc; };
          };

          formatter = pkgs.nixfmt;
        }
      );
    in
    {
      packages = lib.mapAttrs (_: per: per.packages) perSystem;
      checks = lib.mapAttrs (_: per: per.checks) perSystem;
      devShells = lib.mapAttrs (_: per: per.devShells) perSystem;
      formatter = lib.mapAttrs (_: per: per.formatter) perSystem;
    };
}
