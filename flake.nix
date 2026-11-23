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
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;
      # The Windows payload is cross-built from Linux (clang-cl + lld-link +
      # the xwin CRT/SDK), the same way the release workflow does it, so the
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
          # Everything Cargo resolves, vendored from the committed lockfile.
          # `zgui` is a git dependency; the hash names the one commit the lock
          # pins (`nix flake prefetch 'git+https://github.com/zortax/zgui?rev=…'`).
          cargoLock = {
            lockFile = ./Cargo.lock;
            outputHashes = {
              "zgui-0.1.0" = "sha256-wF8aYcJvME4/YVA6UXB2vrfQXmyWtFCRwLz1h6/NWP0=";
            };
          };
          callPackage = lib.callPackageWith (
            pkgs
            // {
              inherit
                self
                version
                cargoLock
                sources
                rustPlatform
                rustToolchain
                ;
            }
          );

          # `default` and `linux` are the same derivation; name it once. The
          # installer consumes the Windows payload, so that is one binding too.
          linux = callPackage ./nix/linux.nix { };
          windows = callPackage ./nix/windows.nix { };
          windows-installer = callPackage ./nix/windows-installer.nix { inherit windows; };
        in
        {
          packages = {
            inherit
              linux
              windows
              windows-installer
              ;
            default = linux;
          };

          devShells.default = callPackage ./nix/devshell.nix { };

          formatter = pkgs.nixfmt;
        }
      );
    in
    {
      packages = lib.mapAttrs (_: per: per.packages) perSystem;
      devShells = lib.mapAttrs (_: per: per.devShells) perSystem;
      formatter = lib.mapAttrs (_: per: per.formatter) perSystem;
    };
}
