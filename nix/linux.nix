# The native Linux build: the daemon and the ZGUI window, linked against
# nixpkgs' libplacebo. This is the same binary `cargo build -p siphon-ui -p
# daemon` produces on a Fedora box with the -devel packages installed; the
# point of the derivation is that it needs none of them.
#
# Three derivations, so that editing one crate does not redo the others:
# the dependency layer, `siphon` on top of it, and `siphon-daemon` on top of
# the same artifacts. `nix run` sees the joined tree.
{
  lib,
  craneLib,
  rustPlatform,
  pkg-config,
  clang,
  symlinkJoin,
  libplacebo,
  vulkan-headers,
  libxkbcommon,
  fontconfig,
  wayland,
  python3,
  version,
  cargoHashes,
  sources,
  cargoSrc,
}:

let
  commonArgs = {
    pname = "siphon";
    inherit version;
    src = cargoSrc;

    # The tests bind loopback sockets and read the built `captures/servermock`
    # fixtures, neither of which exists in the sandbox.
    doCheck = false;

    cargoVendorDir = craneLib.vendorCargoDeps {
      src = cargoSrc;
      outputHashes = cargoHashes;
    };
    cargoExtraArgs = "--locked";

    nativeBuildInputs = [
      pkg-config
      rustPlatform.bindgenHook
      # `libplacebo-sys` asks the clang driver for its resource directory: the
      # distro libclang it prefers cannot derive it from its own path.
      clang
      # `stylo`'s build script regenerates its property tables with Python; it
      # runs `python3` from PATH and vendors MarkupSafe for the templates.
      python3
    ];

    buildInputs = [
      libplacebo
      vulkan-headers
      # zgui's text stack resolves system fonts through fontconfig, and the
      # Wayland driver needs xkbcommon. wgpu is only a build-time dependency of
      # the renderer crate; the swapchain itself is libplacebo's.
      fontconfig
      libxkbcommon
      wayland
    ];

    env = {
      # `build.rs` downloads Chillax unless this names a copy (see nix/sources.nix).
      SIPHON_CHILLAX_TTF = "${sources.chillax}/Fonts/TTF/Chillax-Variable.ttf";
    };
  };

  deps = craneLib.buildDepsOnly (
    commonArgs
    // {
      pname = "siphon-deps";
      # `buildDepsOnly` otherwise runs `cargo check` and then `cargo build` over
      # every dependency; the binaries only need the second pass.
      buildPhaseCargoCommand = "cargoWithProfile build --locked";
    }
  );

  mkBin =
    { pname, package }:
    craneLib.buildPackage (
      commonArgs
      // {
        inherit pname;
        cargoArtifacts = deps;
        cargoExtraArgs = "--locked -p ${package}";
      }
    );
in
{
  # The dependency layer on its own, so CI can cache exactly it.
  inherit deps;

  package = symlinkJoin {
    name = "siphon-${version}";
    paths = [
      (mkBin {
        pname = "siphon";
        package = "siphon-ui";
      })
      (mkBin {
        pname = "siphon-daemon-pkg";
        package = "daemon";
      })
    ];
    meta = {
      description = "Siphon daemon and UI (Linux)";
      homepage = "https://github.com/Ozeniken/siphon";
      license = lib.licenses.mit;
      mainProgram = "siphon";
      platforms = [ "x86_64-linux" ];
    };
  };

  # The lint CI runs, on the same dependency artifacts the binaries use.
  clippy = craneLib.cargoClippy (
    commonArgs
    // {
      pname = "siphon-clippy";
      cargoArtifacts = deps;
    }
  );
}
