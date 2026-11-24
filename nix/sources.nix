# Everything the build downloads (or materializes once and then treats as
# source) that is not in the repository. Each is pinned by a hash, so the network
# is only ever used to fetch bytes this file names, and a hash bump is the only
# way any of them can change.
{ pkgs, lib }:
let
  # nixos-unstable currently packages xwin 0.9.0; the manifest selection and the
  # splat layout below are the 0.10.0 ones, so that is the version this flake
  # builds. It is used when nixpkgs' package is any other version.
  xwinVersion = "0.10.0";

  # The Windows SDK and VC toolset to pin. Both names are manifest v18
  # package versions: the SDK is `Win11SDK_<sdk>` and the toolset is
  # `Microsoft.VisualStudio.Component.VC.<crt>.x86.x64`.
  sdkVersion = "10.0.28000";
  crtVersion = "14.52";

  # The Microsoft Software License Terms cover the CRT and the SDK. Both
  # derivations that carry those files say so, and the flake's
  # `allowUnfreePredicate` names them.
  msvcLicense = {
    fullName = "Microsoft Software License Terms";
    shortName = "msvc";
    free = false;
    url = "https://visualstudio.microsoft.com/license-terms/mt644918/";
  };

  # The Visual Studio 18 channel manifest, the same document `xwin
  # --manifest-version 18` fetches from `https://aka.ms/vs/18/stable/channel`
  # (`release` is renamed to `stable` from v18 on). Pinning the manifest pins the
  # package manifest it points at: that URL carries the file's own SHA-256.
  msvcChannel = pkgs.fetchurl {
    url = "https://aka.ms/vs/18/stable/channel";
    hash = "sha256-Q4u+AD6Nd4XpRKdkcO4XxZLqL2ySJbcNYJ5CKD8MFh0=";
  };

  xwinArgs = [
    "--accept-license"
    "--manifest=${msvcChannel}"
    "--arch=x86_64"
    "--variant=desktop"
    "--sdk-version=${sdkVersion}"
    "--crt-version=${crtVersion}"
  ];

  # xwin 0.10.0. See `xwinVersion` above; everything else here is written for
  # the `--manifest` / `--preserve-ms-arch-notation` interface.
  xwin =
    if pkgs.xwin.version == xwinVersion then
      pkgs.xwin
    else
      pkgs.rustPlatform.buildRustPackage {
        pname = "xwin";
        version = xwinVersion;

        src = pkgs.fetchFromGitHub {
          owner = "Jake-Shadle";
          repo = "xwin";
          tag = xwinVersion;
          hash = "sha256-kUbRHrM88x3ezy0+GIpbGQfEROn16dSwPtobjdYauRM=";
        };
        cargoHash = "sha256-34v/YDT0Juwjv8EbwsOsmbp/Q89Z4c+BYxcA60X7Q3A=";

        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.openssl ];

        doCheck = false;

        meta.mainProgram = "xwin";
      };

  # The CRT and SDK payloads, downloaded once and pinned by the hash of the whole
  # cache directory. `xwin download` without `--temp` keeps them under `dl/` and
  # `unpack/`, which is what `xwin splat` reads below.
  msvcCache = pkgs.stdenvNoCC.mkDerivation {
    pname = "win-sdk-cache";
    version = "${sdkVersion}-${crtVersion}";

    dontUnpack = true;
    nativeBuildInputs = [
      xwin
      pkgs.cacert
    ];

    outputHashMode = "recursive";
    outputHash = "sha256-yAmCLyRrBm+6LCnmpRjmdqOvoGjcePkw4Fv1KZZHSP0=";

    # The hash covers the downloaded payloads only, not whatever this nixpkgs'
    # fixup pass would rewrite.
    dontFixup = true;

    env = {
      SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    };

    meta.license = msvcLicense;

    buildPhase = ''
      runHook preBuild
      xwin ${lib.concatStringsSep " " xwinArgs} --cache-dir="$PWD/cache" download
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mv cache $out
      runHook postInstall
    '';
  };

  # The splat clang-cl, lld-link and the C dependencies of the Rust crates read
  # their headers and import libraries from. `--preserve-ms-arch-notation` keeps
  # MSVC's `x64` directory names.
  msvcSdk = pkgs.stdenvNoCC.mkDerivation {
    pname = "win-sdk";
    version = "${sdkVersion}-${crtVersion}";

    src = msvcCache;

    nativeBuildInputs = [ xwin ];

    meta.license = msvcLicense;

    buildPhase = ''
      runHook preBuild
      xwin ${lib.concatStringsSep " " xwinArgs} --cache-dir=. \
        splat --output="$PWD/splat" --preserve-ms-arch-notation
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mv splat $out
      runHook postInstall
    '';

    dontFixup = true;
  };

  # The UI's brand face, Chillax. Its license allows embedding the font in the
  # application but not redistributing the file from a repository, so
  # `crates/ui/build.rs` fetches it from Fontshare. The endpoint hands out a zip
  # generated per request (its entry timestamps move), so this is a `fetchzip`:
  # the hash covers the unpacked tree, not the archive bytes.
  #
  # If Fontshare revises the family the hash changes; update it by building with
  # `lib.fakeHash` and copying the hash the mismatch reports.
  chillax = pkgs.fetchzip {
    url = "https://api.fontshare.com/v2/fonts/download/chillax";
    extension = "zip";
    hash = "sha256-MUxvpgRE42wQQKa54oMjIZt9WmQNjBuK7DbS0DxtkXk=";
  };

  # Pinned to the same revision as `LIBPLACEBO_REV` in the workflows. The Windows
  # prefix is cross-built from this checkout; the Linux build uses nixpkgs'.
  libplaceboRev = "b785229836d6879249ecb21551ba098ed549b5f6";

  libplacebo = pkgs.fetchgit {
    url = "https://github.com/haasn/libplacebo";
    rev = libplaceboRev;
    # Submodules included: libplacebo vendors Vulkan-Headers, fast_float and its
    # code-generator dependencies, and fetching them here is what keeps the
    # meson build offline.
    hash = "sha256-Lmv8sryqHmWwOIztXg+JR+Pw7TlrK5vANayZXbZ4PXU=";
  };

  # shaderc has no release that carries its third_party checkouts, and its CMake
  # needs glslang, SPIRV-Tools and SPIRV-Headers side by side. `git-sync-deps`
  # clones exactly the revisions named in `DEPS`; this fixed-output derivation
  # runs it once and records the tree, so the build itself stays offline.
  shaderc = pkgs.stdenvNoCC.mkDerivation {
    pname = "shaderc-synced";
    version = "2026.4";

    src = pkgs.fetchgit {
      url = "https://github.com/google/shaderc";
      rev = "b64fd42c4a2a9d501c47aca5d3571f7673a5e9cc";
      hash = "sha256-T9+bpLgBQc3XmyMKL3l2Us6ZJ2d7LZJL/FpSCgA3OfE=";
    };

    nativeBuildInputs = [
      pkgs.git
      pkgs.python3
    ];

    outputHashMode = "recursive";
    outputHash = "sha256-2h1faV1xBQwIjEGnOb2h//adKPtDpc3ARiwFQnB7VRI=";

    # The hash covers the checked-out sources only. `fixupPhase` would rewrite
    # shebangs to this nixpkgs' bash, which changes whenever nixpkgs does.
    dontFixup = true;

    env = {
      SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      NIX_SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      # `git-sync-deps` shells out to git, which wants an identity even for the
      # checkouts it only reads.
      GIT_AUTHOR_NAME = "nix";
      GIT_AUTHOR_EMAIL = "nix@localhost";
    };

    buildPhase = ''
      runHook preBuild
      cp -a "$src" shaderc
      chmod -R u+w shaderc
      (cd shaderc && python3 utils/git-sync-deps)
      # The clones carry their own history, which the build does not read and
      # which would make the output depend on packfile layout.
      find shaderc -name .git -prune -exec rm -rf {} +
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mv shaderc $out
      runHook postInstall
    '';
  };

  # The Vulkan SDK layout libplacebo's Windows build and `libplacebo-sys` expect:
  # an `Include/` holding `vulkan/` and `vk_video/`. nixpkgs' `vulkan-headers`
  # is the same Khronos headers under the usual `include/`.
  vulkanSdk = pkgs.runCommand "vulkan-sdk-msvc" { } ''
    mkdir -p $out/Include
    ln -s ${pkgs.vulkan-headers}/include/vulkan $out/Include/vulkan
    ln -s ${pkgs.vulkan-headers}/include/vk_video $out/Include/vk_video
  '';
in
{
  inherit
    chillax
    libplacebo
    libplaceboRev
    msvcCache
    msvcSdk
    shaderc
    vulkanSdk
    xwin
    ;
}
