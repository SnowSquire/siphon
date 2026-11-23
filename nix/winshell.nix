# The Windows cross shell: the same rust toolchain, MSVC CRT/SDK, libplacebo
# prefix and environment the `.#windows` package builds with, exported so a
# plain cargo command targets Windows.
#
# This is what replaced `scripts/cargo-win.sh`: a shell script cannot be
# referenced by CI as a flake output, and the environment it hand-exported had
# to be kept in sync with the derivation by hand. `CARGO_BUILD_TARGET` is set
# here, so `cargo build`/`clippy`/`test` cross-compile without a `--target`:
#
#   nix develop .#windows -c cargo clippy --workspace --all-targets
#   nix develop .#windows -c cargo build --release -p siphon-ui
{
  mkShell,
  rustToolchain,
  pkg-config,
  msvc,
}:

mkShell {
  nativeBuildInputs = [
    rustToolchain
    pkg-config
  ]
  ++ msvc.nativeBuildInputs;

  # `mkShell` exports every `env` entry, so cargo, `cc` and bindgen see exactly
  # what the package derivation puts in front of them.
  inherit (msvc) env;
}
