# The native Linux build: the daemon and the ZGUI window, linked against
# nixpkgs' libplacebo. This is the same binary `cargo build -p siphon-ui -p
# daemon` produces on a Fedora box with the -devel packages installed; the
# point of the derivation is that it needs none of them.
{
  lib,
  rustPlatform,
  pkg-config,
  clang,
  libplacebo,
  vulkan-headers,
  libxkbcommon,
  fontconfig,
  wayland,
  python3,
  self,
  version,
  cargoLock,
  sources,
}:

rustPlatform.buildRustPackage {
  pname = "siphon";
  inherit version cargoLock;

  # The flake source itself: exactly the git/jj-tracked files, so `captures/`
  # (several GB, gitignored) and a local `target/` never enter the build.
  src = self;

  cargoBuildFlags = [
    "--package"
    "siphon-ui"
    "--package"
    "daemon"
  ];

  # The tests bind loopback sockets and read the built `captures/servermock`
  # fixtures, neither of which exists in the sandbox.
  doCheck = false;

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

  # `build.rs` downloads Chillax unless this names a copy (see nix/sources.nix).
  env.SIPHON_CHILLAX_TTF = "${sources.chillax}/Fonts/TTF/Chillax-Variable.ttf";

  meta = {
    description = "Siphon daemon and UI (Linux)";
    homepage = "https://github.com/Ozeniken/siphon";
    license = lib.licenses.mit;
    mainProgram = "siphon";
    platforms = [ "x86_64-linux" ];
  };
}
