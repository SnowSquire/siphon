# A shell with the same toolchain the Linux package builds with, plus the
# runtime libraries the window needs under `nix run`. Building and running on
# Fedora with the distro's -devel packages is still the normal dev loop; this
# exists so a machine without them can build, and so the flake's own inputs can
# be checked without a full build.
{
  lib,
  mkShell,
  rustToolchain,
  pkg-config,
  clang,
  libclang,
  libplacebo,
  vulkan-headers,
  vulkan-loader,
  libxkbcommon,
  fontconfig,
  wayland,
  libGL,
  nixfmt,
  sources,
}:

mkShell {
  nativeBuildInputs = [
    # The same rust-lang build the packages use, Windows target included.
    rustToolchain
    pkg-config
    clang
    libclang
    nixfmt
  ];

  buildInputs = [
    libplacebo
    vulkan-headers
    vulkan-loader
    libxkbcommon
    fontconfig
    wayland
    libGL
  ];

  env = {
    SIPHON_CHILLAX_TTF = "${sources.chillax}/Fonts/TTF/Chillax-Variable.ttf";
    LIBCLANG_PATH = "${lib.getLib libclang}/lib";
    # `ash` loads vulkan-1.so at run time, and the window needs a driver.
    LD_LIBRARY_PATH = lib.makeLibraryPath [
      vulkan-loader
      libplacebo
      wayland
      libxkbcommon
      libGL
    ];
  };
}
