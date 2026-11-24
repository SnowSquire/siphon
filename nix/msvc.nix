# The MSVC side of the Windows cross build: clang-cl, lld-link and llvm-lib
# against the pinned CRT/SDK `nix/sources.nix` splats, the two dependencies
# that have to be cross-compiled (shaderc, then libplacebo), and the
# environment every cargo invocation for the target shares.
#
# This lives apart from `windows.nix` because the same environment has to reach
# three places: the payload build, the packaging derivation that consumes it,
# and the `.#windows` dev shell. Keeping it in one file is what makes a CI lint
# and a local `nix develop .#windows` compile exactly what a release does.
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
  vulkan-headers,
  sources,
  # `-march=` for every C/C++ translation unit and `-C target-cpu=` for rustc,
  # e.g. `"znver4"`. `null` keeps the release artifact's generic x86-64, which
  # is the one that has to run on a stranger's machine.
  cpu ? null,
  # `shared` ships `libplacebo-<api>.dll` beside the executables; `static` links
  # the archive into `siphon.exe` instead, which drops the DLL from the payload.
  libplaceboLink ? "shared",
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

  # One CPU choice, in every flag list a build reads: clang-cl's `-march` for
  # the crates' C shims, the CMake toolchain file shaderc gets, meson's cross
  # file for libplacebo, bindgen's view of the headers, and rustc's
  # `-C target-cpu`. A build that tuned only the Rust half would still call into
  # a generic libplacebo, and one that tuned only C would leave the pipeline on
  # the baseline ISA.
  march = lib.optionalString (cpu != null) " -march=${cpu}";
  cpuRust = lib.optionalString (cpu != null) " -C target-cpu=${cpu}";
  staticPlacebo = libplaceboLink == "static";

  # clang-cl takes `/imsvc`; libclang in bindgen's GNU driver mode wants
  # `-isystem` and the target triple.
  clangFlags =
    lib.concatMapStrings (dir: " /imsvc${dir}") (msvcIncludeDirs ++ [ vulkanInclude ]) + march;
  bindgenFlags =
    lib.concatMapStrings (dir: " -isystem ${dir}") (
      msvcIncludeDirs
      ++ [
        vulkanInclude
      ]
    )
    + march;
  # `-Lnative=` for the SDK's import libraries, shaderc's archive when a static
  # libplacebo has to name it, and the CPU, which rustc spells differently.
  rustLinkFlags =
    lib.concatMapStrings (dir: " -Lnative=${dir}") msvcLibDirs
    + lib.optionalString staticPlacebo " -Lnative=${shaderc}/lib"
    + cpuRust;

  # The C/C++ build scripts (shaderc's CMake, libplacebo's meson) read the SDK
  # from these, so the cross files above do not have to repeat the paths.
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
    set(CMAKE_CXX_COMPILER_TARGET x86_64-pc-windows-msvc)${
      lib.optionalString (cpu != null) ''

        # shaderc is the only CMake build here; its flags come from this file.
        set(CMAKE_C_FLAGS_INIT "-march=${cpu}")
        set(CMAKE_CXX_FLAGS_INIT "-march=${cpu}")''
    }
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
      lib.optionalString (cpu != null) ", '-march=${cpu}'"
    }${lib.concatMapStrings (dir: ", '/imsvc${dir}'") (msvcIncludeDirs ++ [ vulkanInclude ])}]
    cpp_args = ['--target=x86_64-pc-windows-msvc', '-fuse-ld=lld'${
      lib.optionalString (cpu != null) ", '-march=${cpu}'"
    }${lib.concatMapStrings (dir: ", '/imsvc${dir}'") (msvcIncludeDirs ++ [ vulkanInclude ])}]
    c_link_args = [${lib.concatMapStrings (dir: "'/libpath:${dir}', ") msvcLibDirs}]
    cpp_link_args = [${lib.concatMapStrings (dir: "'/libpath:${dir}', ") msvcLibDirs}]

    [properties]
    needs_exe_wrapper = true
  '';

  # shaderc's GLSL compiler, statically linked so libplacebo gains no second
  # DLL. Mirrors the release's old shell build: the `shaderc_combined` archive,
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

  # libplacebo against the cross-built shaderc. Same options as the release's
  # old shell build: Vulkan enabled but no platform GL/D3D11, no optional
  # codecs or lcms, demos and tests off.
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
        ${lib.optionalString staticPlacebo "-Ddefault_library=static \\\n        "}-Dvulkan=enabled \
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
{
  # `windows.nix` reads `staticPlacebo` to decide whether a DLL belongs in the
  # payload; `cpu` is only here so a caller can see what it asked for.
  inherit libplacebo cpu staticPlacebo;

  # The environment a cargo invocation for the target needs. `windows.nix`
  # turns it into the package's `env`; the dev shell exports it verbatim.
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
  }
  # `libplacebo-sys` defaults to a dynamic link, so only the static build says
  # anything; an unconditional variable would change the release derivation's
  # environment for no reason.
  // lib.optionalAttrs staticPlacebo { LIBPLACEBO_LINK = "static"; };

  nativeBuildInputs = [
    llvmBin
    lldBin
    clangBin
    # `ring` assembles its pregenerated x86_64 .asm with nasm on Windows.
    nasm
    perl
  ];
}
