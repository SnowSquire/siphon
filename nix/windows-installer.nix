# The three Windows artifacts, packaged from the payload `nix/windows.nix`
# builds:
#
#   siphon-<version><label>-x64-setup.exe      the per-user NSIS installer
#   siphon-<version><label>-x64-portable.zip   the same payload, zipped
#   siphon-<version><label>-x64-symbols.zip    the release PDBs
#
# `label` is empty for a release and `-<short commit>` for any other build, so
# two builds of one version are distinguishable and the tag keeps the plain
# name the updater looks for.
#
# The setup is what the updater downloads: the release asset must keep the
# `-setup.exe` suffix, and its name carries the version. The portable zip keeps
# its payload under one directory so extracting it does not scatter files. The
# symbols zip carries the PDBs a minidump needs to resolve; they stay out of the
# installer, where a user would never need them.
#
# This used to be `scripts/package-win.sh`, which the release workflow ran with
# the runner's own xwin setup. The flake builds the payload, so the packaging
# moved in with it and there is one pipeline instead of two.
{
  lib,
  stdenvNoCC,
  nsis,
  zip,
  windows,
  installerSrc,
  version,
  label ? "",
  sources,
}:

let
  # The name every artifact, and the portable zip's inner directory, is built
  # from: `siphon-0.5.1`, or `siphon-0.5.1-18b8dd2` off a branch.
  base = "siphon-${version}${label}";
in
stdenvNoCC.mkDerivation {
  pname = "siphon-windows-installer";
  inherit version;

  # The license texts, the fonts' OFL files, the NSIS script and the VC++
  # redistributable bootstrapper: the whole of what packaging reads, and
  # nothing else, so editing a note or the flake does not rebuild the setup.
  src = installerSrc;

  nativeBuildInputs = [
    nsis
    zip
  ];

  buildPhase = ''
    runHook preBuild
    dist=$TMPDIR/dist
    stage=$dist/${base}
    mkdir -p "$stage" "$dist"

    cp ${windows}/bin/siphon.exe ${windows}/bin/siphon-daemon.exe "$stage/"

    cp LICENSE-MIT LICENSE-APACHE README.md "$stage/"
    # The bundled face is OFL and its license text has to travel with it.
    cp crates/ui/fonts/Archivo-OFL.txt "$stage/LICENSE-Archivo-OFL.txt"

    # Everything linked into the executables travels with them. libplacebo is
    # LGPL and its meson install carries no license text, and the GLSL compiler
    # meson links into it is a static `shaderc_combined`: glslang, SPIRV-Tools
    # and SPIRV-Headers are in the same image, as are the fast_float and Vulkan
    # headers libplacebo vendors. README.md names each one and its text.
    cp ${sources.libplacebo}/LICENSE "$stage/LICENSE-libplacebo.txt"
    cp ${sources.libplacebo}/3rdparty/Vulkan-Headers/LICENSE.md "$stage/LICENSE-Vulkan-Headers.txt"
    cp ${sources.libplacebo}/3rdparty/fast_float/LICENSE-APACHE "$stage/LICENSE-fast_float.txt"
    cp ${sources.shaderc}/LICENSE "$stage/LICENSE-shaderc.txt"
    cp ${sources.shaderc}/third_party/glslang/LICENSE.txt "$stage/LICENSE-glslang.txt"
    cp ${sources.shaderc}/third_party/spirv-tools/LICENSE "$stage/LICENSE-SPIRV-Tools.txt"
    cp ${sources.shaderc}/third_party/spirv-headers/LICENSE "$stage/LICENSE-SPIRV-Headers.txt"

    # NSIS records the staged files' timestamps and zip stores them per entry,
    # so pin them: without this the setup differs in a handful of bytes and the
    # zips differ in their entry headers.
    epoch=''${SOURCE_DATE_EPOCH:-315532800} # 1980-01-01, the oldest date zip stores
    find "$stage" -exec touch -h -d "@$epoch" {} +

    # `File` inside the script resolves against the script's own directory, so
    # both paths handed to `makensis` have to be absolute.
    makensis -V2 -DVERSION=${version} -DPAYLOAD="$stage" \
      -DVCREDIST="$PWD/installer/vcredist.ps1" \
      -DOUTFILE="$dist/${base}-x64-setup.exe" "$PWD/installer/siphon.nsi"

    # The portable copy is the same payload zipped under its directory: there is
    # no single exe to ship any more. The name deliberately does not end in
    # `-setup.exe`, so the updater never mistakes it for an installer. `-X`
    # drops the extra fields that carry more of the same timestamps.
    ( cd "$dist" && zip -qrX "${base}-x64-portable.zip" "${base}" )

    symbols="$dist/${base}-symbols"
    mkdir -p "$symbols"
    cp ${windows}/symbols/*.pdb "$symbols/"
    find "$symbols" -exec touch -h -d "@$epoch" {} +
    ( cd "$dist" && zip -qrX "${base}-x64-symbols.zip" "${base}-symbols" )

    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    mkdir -p $out
    cp $TMPDIR/dist/*.exe $TMPDIR/dist/*.zip $out/
    runHook postInstall
  '';

  # NSIS's own binaries and the payload are PE files, not ELF objects to patch.
  dontFixup = true;

  meta = {
    description = "Siphon Windows installer, portable zip and symbols zip";
    license = lib.licenses.mit;
    platforms = [ "x86_64-linux" ];
  };
}
