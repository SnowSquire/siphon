# The Windows installer and the portable zip, built from the payload
# `nix/windows.nix` produces: `scripts/package-win.sh --no-build` with the
# staging directory pointed at the store, so the setup carries exactly the
# binaries the flake built and nothing from a local `target/`.
{
  lib,
  stdenvNoCC,
  nsis,
  zip,
  jq,
  rustToolchain,
  windows,
  self,
  version,
  sources,
}:

stdenvNoCC.mkDerivation {
  pname = "siphon-windows-installer";
  inherit version;

  src = self;

  # `cargo` is here for `cargo metadata --no-deps`, which is how the packaging
  # script reads the workspace version; it reads manifests only.
  nativeBuildInputs = [
    nsis
    zip
    jq
    rustToolchain
  ];

  env = {
    # The payload, not a local `target/x86_64-pc-windows-msvc/release`.
    SIPHON_TARGET_DIR = "${windows}/bin";
    # The meson install carries no licence text, so the script takes libplacebo's
    # from the checkout when it is told where it is.
    LIBPLACEBO_SRC = "${sources.libplacebo}";
  };

  buildPhase = ''
    runHook preBuild
    # `cargo metadata` insists on a cache directory, and `$HOME` is the
    # deliberately missing /homeless-shelter.
    export CARGO_HOME="$TMPDIR/cargo"
    bash scripts/package-win.sh --no-build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp dist/*.exe dist/*.zip $out/
    runHook postInstall
  '';

  # NSIS's own binaries and the payload are PE files, not ELF objects to patch.
  dontFixup = true;

  meta = {
    description = "Siphon Windows installer and portable zip";
    license = lib.licenses.mit;
    platforms = [ "x86_64-linux" ];
  };
}
