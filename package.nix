# The receiver build, shared by flake.nix's `packages` output and by whatever module ends
# up rendering a systemd unit for `nixdeploy.receiver.enable` (out of scope for this repo's
# current file set -- see modules/default.nix) so there is exactly one place this
# derivation is defined.
{ lib, rustPlatform }:

rustPlatform.buildRustPackage {
  pname = "nixdeploy";
  version = "0.1.0";
  src = ./.;

  # Cargo.lock is committed so this builds fully offline and reproducibly -- importCargoLock
  # derives its own fixed-output fetch hash straight from the lockfile, no separate
  # vendorHash/cargoHash to keep in sync by hand. Re-run `cargo build` (or `cargo
  # generate-lockfile`) after any dependency bump to refresh it.
  cargoLock.lockFile = ./Cargo.lock;

  # buildRustPackage's default checkPhase runs `cargo test`, which builds and exercises
  # BOTH targets this crate defines (see Cargo.toml's `[lib]` comment): the `nixdeploy`
  # library (outcome.rs's own unit tests, plus tests/outcome_test.rs) and the `nixdeploy`
  # binary (main.rs plus its `mod`-included manifest.rs/delta.rs/activate.rs, each with
  # their own in-file unit tests). A build that skipped `cargo test` here would ship the
  # exact thing this crate exists to prevent elsewhere: something that ran without anyone
  # having actually checked what it did.
  meta = {
    description = "The receiver: sizes a change against its OWN store from narinfo metadata, refuses what would not survive activation, and reports a typed outcome -- see https://github.com/julian-corbet/nixdeploy";
    homepage = "https://github.com/julian-corbet/nixdeploy";
    license = lib.licenses.mit;
    mainProgram = "nixdeploy";
    platforms = lib.platforms.unix;
  };
}
