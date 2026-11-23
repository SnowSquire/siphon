# The Windows payload, cross-built on Linux with clang-cl, lld-link and the
# MSVC CRT/Windows SDK that `nix/sources.nix` splats from a pinned Visual
# Studio 18 manifest. Nothing in this file runs on Windows.
#
# The shape follows `scripts/build-shaderc-win.sh` + `scripts/build-libplacebo-win.sh`
# + `scripts/cargo-win.sh`, which is the loop the dev box and the release
# workflow use: shaderc is built as a static archive so its GLSL compiler can be
# linked into libplacebo, libplacebo is a DLL the UI links and load-time needs
# beside the executable, and both are cross-compiled rather than built on a
# Windows machine.
{
  lib,
  stdenvNoCC,
  writeText,
  cmake,
  ninja,
  meson,
  python3,
  pkg-config,
  nasm,
  perl,
  llvmPackages_23,
  rustPlatform,
  vulkan-headers,
  self,
  version,
  cargoLock,
  sources,
}:
let
  # The MSVC STL in the pinned toolset refuses anything older than clang 22
  # (`error STL1000: Unexpected compiler version, expected Clang 22 or newer`),
  # which is newer than nixpkgs' default `llvmPackages`, so pin the latest.
  inherit (llvmPackages_23)
    clang-unwrapped
    libclang
    llvm
    lld
    ;

  # `clang-cl` lives in the clang package, the MSVC-style archiver, resource
  # compiler and lld in llvm/lld.
  clangBin = lib.getBin clang-unwrapped;
  llvmBin = lib.getBin llvm;
  lldBin = lib.getBin lld;

  sdk = sources.msvcSdk;
  crt = "${sdk}/crt";
  winSdk = "${sdk}/sdk";
  # The splat keeps `--preserve-ms-arch-notation`, so the library
  # directories carry MSVC's `x64`, not LLVM's `x86_64`.
  arch = "x64";

  # The MSVC CRT and Windows SDK headers and import libraries. The flattened
  # layout is the one `xwin splat` produces on a case-sensitive filesystem.
  msvcIncludeDirs = [
    "${crt}/include"
    "${winSdk}/include/ucrt"
    "${winSdk}/include/um"
    "${winSdk}/include/shared"
  ];
  msvcLibDirs = [
    "${crt}/lib/${arch}"
    "${winSdk}/lib/um/${arch}"
    "${winSdk}/lib/ucrt/${arch}"
  ];

  vulkanInclude = "${sources.vulkanSdk}/Include";

  # clang-cl takes `/imsvc`; libclang in bindgen's GNU driver mode wants
  # `-isystem` and the target triple.
  clangFlags = lib.concatMapStrings (dir: " /imsvc${dir}") (msvcIncludeDirs ++ [ vulkanInclude ]);
  bindgenFlags = lib.concatMapStrings (dir: " -isystem ${dir}") (
    msvcIncludeDirs
    ++ [
      vulkanInclude
    ]
  );
  rustLinkFlags = lib.concatMapStrings (dir: " -Lnative=${dir}") msvcLibDirs;

  msvcEnv = {
    INCLUDE = lib.concatStringsSep ";" (msvcIncludeDirs ++ [ vulkanInclude ]);
    LIB = lib.concatStringsSep ";" msvcLibDirs;
  };

  # The CMake cross file: clang-cl and lld-link, with the SDK flags on the
  # compile line so CMake's compiler checks work without an environment.
  msvcCmake = writeText "msvc-cross.cmake" ''
    set(CMAKE_SYSTEM_NAME Windows)
    set(CMAKE_SYSTEM_PROCESSOR x86_64)
    set(CMAKE_C_COMPILER ${clangBin}/bin/clang-cl)
    set(CMAKE_CXX_COMPILER ${clangBin}/bin/clang-cl)
    set(CMAKE_LINKER ${lldBin}/bin/lld-link)
    set(CMAKE_AR ${llvmBin}/bin/llvm-lib)
    set(CMAKE_RC_COMPILER ${llvmBin}/bin/llvm-rc)
    set(CMAKE_MT ${llvmBin}/bin/llvm-mt)
    set(CMAKE_C_COMPILER_TARGET x86_64-pc-windows-msvc)
    set(CMAKE_CXX_COMPILER_TARGET x86_64-pc-windows-msvc)
  '';

  # The meson cross file. `needs_exe_wrapper` is here because nothing the build
  # produces is run during the build.
  msvcMeson = writeText "msvc-cross.ini" ''
    [binaries]
    c = 'clang-cl'
    cpp = 'clang-cl'
    ar = 'llvm-lib'
    strip = 'llvm-strip'
    windres = 'llvm-rc'
    pkg-config = 'pkg-config'

    [host_machine]
    system = 'windows'
    subsystem = 'windows'
    kernel = 'nt'
    cpu_family = 'x86_64'
    cpu = 'x86_64'
    endian = 'little'

    [built-in options]
    c_args = ['--target=x86_64-pc-windows-msvc', '-fuse-ld=lld'${
      lib.concatMapStrings (dir: ", '/imsvc${dir}'") (msvcIncludeDirs ++ [ vulkanInclude ])
    }]
    cpp_args = ['--target=x86_64-pc-windows-msvc', '-fuse-ld=lld'${
      lib.concatMapStrings (dir: ", '/imsvc${dir}'") (msvcIncludeDirs ++ [ vulkanInclude ])
    }]
    c_link_args = [${lib.concatMapStrings (dir: "'/libpath:${dir}', ") msvcLibDirs}]
    cpp_link_args = [${lib.concatMapStrings (dir: "'/libpath:${dir}', ") msvcLibDirs}]

    [properties]
    needs_exe_wrapper = true
  '';

  # shaderc's GLSL compiler, statically linked so libplacebo gains no second
  # DLL. Mirrors `scripts/build-shaderc-win.sh`: the `shaderc_combined` archive,
  # the public headers, and a pkg-config file naming the archive rather than
  # the shared target shaderc's own `.pc` describes.
  shaderc = stdenvNoCC.mkDerivation {
    pname = "shaderc-msvc";
    version = "2026.4";

    src = sources.shaderc;

    nativeBuildInputs = [
      cmake
      ninja
      python3
      llvmBin
      lldBin
      clangBin
    ];

    env = msvcEnv;

    # The cmake setup hook would put this build's own `$CC`/`$CXX` (the Linux
    # compiler) and install directories on the line, overriding the cross
    # toolchain file. The whole configure run is spelled out below instead.
    dontUseCmakeConfigure = true;

    buildPhase = ''
      runHook preBuild
      cmake -S . -B build -G Ninja \
        -DCMAKE_TOOLCHAIN_FILE=${msvcCmake} \
        -DCMAKE_BUILD_TYPE=Release \
        -DBUILD_SHARED_LIBS=OFF \
        -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreadedDLL \
        -DSHADERC_SKIP_TESTS=ON \
        -DSHADERC_SKIP_EXAMPLES=ON \
        -DSHADERC_SKIP_EXECUTABLES=ON \
        -DSHADERC_SKIP_COPYRIGHT_CHECK=ON \
        -DSHADERC_ENABLE_WERROR_COMPILE=OFF \
        -DSHADERC_ENABLE_SHARED_CRT=ON
      cmake --build build --target shaderc_combined
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      archive=$(find build -name 'shaderc_combined.lib' -o -name 'libshaderc_combined.a' | head -1)
      test -n "$archive"
      mkdir -p $out/lib/pkgconfig $out/include/shaderc
      cp "$archive" $out/lib/shaderc_combined.lib
      cp libshaderc/include/shaderc/*.h libshaderc/include/shaderc/*.hpp $out/include/shaderc/
      # Absolute paths rather than pkg-config's usual prefix indirection: the
      # here-document is expanded by the shell, so a dollar-brace variable would
      # be eaten before pkg-config ever saw it.
      cat > $out/lib/pkgconfig/shaderc.pc <<EOF
      prefix=$out
      libdir=$out/lib
      includedir=$out/include

      Name: shaderc
      Description: Shaderc static SPIR-V compiler (combined archive)
      Version: 2026.4
      Libs: -L$out/lib -lshaderc_combined
      Cflags: -I$out/include
      EOF
      runHook postInstall
    '';
  };

  # libplacebo against the cross-built shaderc. Same options as
  # `scripts/build-libplacebo-win.sh`: Vulkan enabled but no platform GL/D3D11,
  # no optional codecs or lcms, demos and tests off.
  libplacebo = stdenvNoCC.mkDerivation {
    pname = "libplacebo-msvc";
    version = "unstable-${lib.substring 0 8 sources.libplaceboRev}";

    src = sources.libplacebo;

    # `pkg-config` in nativeBuildInputs plus the dependency here is all it takes:
    # nixpkgs' wrapper puts every input's `lib/pkgconfig` on PKG_CONFIG_PATH, so
    # the meson cross file's `pkg-config` resolves shaderc without a hand-wired
    # path. shaderc's own `.pc` names the static archive, because the combined
    # target it ships is not what upstream's file describes.
    buildInputs = [ shaderc ];

    nativeBuildInputs = [
      meson
      ninja
      pkg-config
      python3
      llvmBin
      lldBin
      clangBin
    ];

    env = msvcEnv;

    configurePhase = ''
      runHook preConfigure
      meson setup build \
        --cross-file ${msvcMeson} \
        --prefix=$out \
        --libdir=lib \
        --bindir=bin \
        --buildtype=release \
        -Dvulkan=enabled \
        -Dvk-proc-addr=disabled \
        -Dvulkan-registry=${vulkan-headers}/share/vulkan/registry/vk.xml \
        -Db_vscrt=md \
        -Dopengl=disabled \
        -Dd3d11=disabled \
        -Dglslang=disabled \
        -Dshaderc=enabled \
        -Ddovi=disabled \
        -Dlibdovi=disabled \
        -Dlcms=disabled \
        -Dxxhash=disabled \
        -Dunwind=disabled \
        -Ddemos=false \
        -Dtests=false \
        -Dbench=false \
        -Dfuzz=false
      runHook postConfigure
    '';

    buildPhase = ''
      runHook preBuild
      meson compile -C build
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      meson install -C build --no-rebuild
      runHook postInstall
    '';
  };

in
rustPlatform.buildRustPackage {
  pname = "siphon-windows";
  inherit version cargoLock;

  src = self;

  doCheck = false;

  # A PE file is not an ELF object: strip, patchelf and the fixup pass would all
  # mangle the payload or fail on it.
  dontStrip = true;
  dontPatchELF = true;
  dontFixup = true;

  nativeBuildInputs = [
    llvmBin
    lldBin
    clangBin
    # `ring` assembles its pregenerated x86_64 .asm with nasm on Windows.
    nasm
    perl
    # `stylo`'s build script regenerates its property tables with Python.
    python3
  ];

  # `buildRustPackage`'s build hook pins `--target` to the *stdenv's* target,
  # which is the Linux host: it would silently build Linux executables. The
  # cargo invocation is spelled out so the Windows triple is what gets built.
  buildPhase = ''
    runHook preBuild
    cargo build --offline --locked --release \
      --target x86_64-pc-windows-msvc \
      --package siphon-ui --package daemon
    runHook postBuild
  '';

  env = {
    CARGO_BUILD_TARGET = "x86_64-pc-windows-msvc";
    CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = "lld-link";

    CC_x86_64_pc_windows_msvc = "clang-cl";
    CXX_x86_64_pc_windows_msvc = "clang-cl";
    AR_x86_64_pc_windows_msvc = "llvm-lib";
    CFLAGS_x86_64_pc_windows_msvc = "--target=x86_64-pc-windows-msvc${clangFlags}";
    CXXFLAGS_x86_64_pc_windows_msvc = "--target=x86_64-pc-windows-msvc${clangFlags}";

    RUSTFLAGS = rustLinkFlags;

    # `libplacebo-sys` in prefix mode, rather than pkg-config (there is no
    # cross-aware pkg-config for the target).
    LIBPLACEBO_SOURCE = "prefix";
    LIBPLACEBO_PREFIX = "${libplacebo}";
    VULKAN_SDK = "${sources.vulkanSdk}";
    LIBPLACEBO_SPLAT = "${sdk}";

    LIBCLANG_PATH = "${lib.getLib libclang}/lib";
    BINDGEN_EXTRA_CLANG_ARGS = "--target=x86_64-pc-windows-msvc${bindgenFlags}";

    SIPHON_CHILLAX_TTF = "${sources.chillax}/Fonts/TTF/Chillax-Variable.ttf";
  };

  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    target=target/x86_64-pc-windows-msvc/release
    cp "$target/siphon.exe" "$target/siphon-daemon.exe" $out/bin/
    # `libplacebo-sys` copies the DLL it links next to the executable; a Windows
    # process only finds it there.
    cp "$target"/libplacebo-*.dll $out/bin/
    runHook postInstall
  '';

  meta = {
    description = "Siphon daemon and UI (Windows x64, cross-built)";
    license = lib.licenses.mit;
    platforms = [ "x86_64-linux" ];
  };
}
