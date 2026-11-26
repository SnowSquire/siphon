# The Windows payload: `siphon.exe`, `siphon-daemon.exe` and the release PDBs
# -- cross-built on Linux with clang-cl and lld-link. The cross environment
# links libplacebo into the UI (`libplaceboLink = "static"`), so the payload is
# two files; a `shared` one would put `libplacebo-<api>.dll` beside them.
#
# Three derivations, so that editing one crate does not redo the others:
#
#   siphon-windows-deps     every dependency, from the committed lockfile
#   siphon-windows-ui       `siphon.exe` on top of those artifacts
#   siphon-windows-daemon   `siphon-daemon.exe` on top of the same artifacts
#
# Cargo remains the resolver -- crane only splits the invocation -- so feature
# unification, build scripts and `DEP_*` plumbing behave exactly as a plain
# `cargo build`.
{
  lib,
  craneLib,
  python3,
  self,
  version,
  cargoHashes,
  symlinkJoin,
  sources,
  msvc,
  cargoSrc,
  depsSrc,
}:
let
  # The debug normalizer needs xxhash: lld derives the CodeView GUID with it.
  python = python3.withPackages (ps: [ ps.xxhash ]);

  # Baked in by `siphon-shared`, so `--version` names the commit the binary came
  # from. It goes on the binaries, not on `commonArgs`, because the dependency
  # layer must not change with the commit -- CI caches that layer under a key
  # made of the lockfile, and it would never hit if every commit produced a new
  # dependency derivation.
  commit = self.rev or self.dirtyRev or "local";

  # Shared by the dependency layer and by each binary: the cross environment,
  # the shipped profile, and the vendored sources.
  commonArgs = {
    pname = "siphon-windows";
    inherit version;
    src = cargoSrc;

    # The tests read `captures/`, which is not in the store.
    doCheck = false;

    # A PE file is not an ELF object: strip, patchelf and the fixup pass would
    # all mangle the payload or fail on it.
    dontStrip = true;
    dontPatchELF = true;
    dontFixup = true;

    # Vendored from the versionless tree: a release bump must not re-vendor
    # either. The dependency set is identical, only the members' version lines
    # are rewritten (see `depsSrc` in flake.nix).
    cargoVendorDir = craneLib.vendorCargoDeps {
      src = depsSrc;
      outputHashes = cargoHashes;
    };
    cargoExtraArgs = "--locked --target x86_64-pc-windows-msvc";

    nativeBuildInputs = msvc.nativeBuildInputs ++ [
      # `stylo`'s build script regenerates its property tables with Python;
      # the same interpreter (with xxhash) normalizes the PDB after the link.
      python3
      python
    ];

    env = msvc.env // {
      # The shipped binaries: whole-program LTO in one codegen unit. The
      # workspace's own release profile is the quick local one, for `cargo
      # build --release` on a dev box; this derivation is what gets released.
      CARGO_PROFILE_RELEASE_LTO = "fat";
      CARGO_PROFILE_RELEASE_CODEGEN_UNITS = "1";

      # Full debug info, dependency layer included. lld merges each rlib's
      # `.debug$S` into the PDB, so leaving the layer at line tables would make
      # every dependency frame in a release minidump a bare address — and a
      # minidump is a snapshot of the shipped image, so a different build's PDB
      # cannot stand in for it later.
      #
      # The data stays in the PDB: `strip = true` (Cargo.toml) still drops the
      # COFF symbol table, and on MSVC the CodeView data is a separate file.
      CARGO_PROFILE_RELEASE_DEBUG = "2";

      SIPHON_CHILLAX_TTF = "${sources.chillax}/Fonts/TTF/Chillax-Variable.ttf";

      # Nix leaves this unset for the derivation, so lld stamps the wall clock
      # into the COFF header and the debug directory and two builds of identical
      # code differ in those six bytes. lld-link does read SOURCE_DATE_EPOCH, so
      # pin it to the epoch the installer's zip already uses.
      SOURCE_DATE_EPOCH = "315532800";
    };
  };

  deps = craneLib.buildDepsOnly (
    commonArgs
    // {
      pname = "siphon-windows-deps";
      # `depsSrc` plus a fixed version: the layer must not move when only the
      # release version does (see `depsSrc` in flake.nix).
      src = depsSrc;
      version = "0";
      # `buildDepsOnly` otherwise runs `cargo check` and then `cargo build`
      # over every dependency; the check pass adds nothing to the artifacts the
      # binaries need.
      buildPhaseCargoCommand = "cargoWithProfile build --locked --target x86_64-pc-windows-msvc";
    }
  );

  mkBin =
    {
      pname,
      package,
      exe,
      pdb,
      # `libplacebo-sys` copies the DLL it links next to the executable, and a
      # Windows process only finds it there. Only the UI links libplacebo, and
      # only a shared build has a DLL to copy: `libplaceboLink = "static"` links
      # the archive in instead, so the payload is two executables.
      dll ? false,
    }:
    craneLib.buildPackage (
      commonArgs
      // {
        inherit pname;
        cargoArtifacts = deps;
        cargoExtraArgs = commonArgs.cargoExtraArgs + " -p ${package}";
        env = commonArgs.env // {
          SIPHON_COMMIT = commit;
        };

        # The install is spelled out instead of crane's log-driven default: the
        # PDB has to be normalized beside the executable and then moved out of
        # `bin/`.
        doNotPostBuildInstallCargoBinaries = true;
        installPhaseCommand = ''
          target=target/x86_64-pc-windows-msvc/release
          mkdir -p $out/bin $out/symbols
          # rustc links a temporary `symbols.o` from a per-invocation directory,
          # and lld records that path in the PDB it hashes into the CodeView
          # GUID. The normalizer pins the path, recomputes the GUID in both
          # files and prints the PDB it touched, so the pair is reproducible and
          # still matches for a debugger. The PDB is already beside the
          # executable, which is where the normalizer looks for it.
          pdb=$(${python}/bin/python3 ${./normalize-pe-debug.py} "$target/${exe}")
          cp "$target/${exe}" $out/bin/
          cp "$pdb" $out/symbols/
        ''
        + lib.optionalString (dll && !msvc.staticPlacebo) ''
          cp "$target"/libplacebo-*.dll $out/bin/
        '';
      }
    );
  ui = mkBin {
    pname = "siphon-windows-ui";
    package = "siphon-ui";
    exe = "siphon.exe";
    pdb = "siphon.pdb";
    dll = true;
  };

  # rustc names a PDB after its crate, so the daemon's file is
  # `siphon_daemon.pdb`, not `siphon-daemon.pdb`.
  daemon = mkBin {
    pname = "siphon-windows-daemon";
    package = "daemon";
    exe = "siphon-daemon.exe";
    pdb = "siphon_daemon.pdb";
  };
in
{
  # The dependency layer on its own, so CI can cache exactly it.
  inherit deps;

  # One output, as before: `bin/` holds the payload and `symbols/` the PDBs.
  package = symlinkJoin {
    name = "siphon-windows-${version}";
    paths = [
      ui
      daemon
    ];
    meta = {
      description = "Siphon daemon and UI (Windows x64, cross-built)";
      license = lib.licenses.mit;
      platforms = [ "x86_64-linux" ];
    };
  };

  # Linting runs in the same cross environment and on the same dependency
  # artifacts, so CI can gate on `nix build .#checks.windows-clippy` instead of
  # setting up a second toolchain. `--all-targets` is crane's default; the
  # workspace root is what gets linted, not one package.
  clippy = craneLib.cargoClippy (
    commonArgs
    // {
      pname = "siphon-windows-clippy";
      cargoArtifacts = deps;
    }
  );
}
