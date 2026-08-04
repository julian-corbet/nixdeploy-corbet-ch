# The one place this derivation is defined. Two things reach it: flake.nix's
# `packages.<system>.nixdeploy` output, and `nixdeploy.receiver.package`'s own default in
# modules/default.nix -- which is what puts the binary at an absolute store path inside the
# scheduled unit a backend adapter renders. A second copy of this expression, one per
# consumer, is how a fleet ends up running a receiver built from something other than the
# source its checks were run against.
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

  # buildRustPackage's default checkPhase runs `cargo test`, which builds and exercises BOTH
  # targets this crate defines (see Cargo.toml's `[lib]` comment): the `nixdeploy` library,
  # where all the logic lives, and the `nixdeploy` binary that parses arguments over it -- plus
  # the integration tests under `tests/`, which Cargo can only link against the library. A
  # build that skipped `cargo test` here would ship the exact thing this crate exists to
  # prevent elsewhere: something that ran without anyone having actually checked what it did.
  meta = {
    description = "Publisher and receiver in one binary: `nixdeploy publish` signs a manifest naming what each machine should run; `nixdeploy receive` sizes that change against its OWN store from narinfo metadata and refuses what would not survive activation, reporting a typed outcome -- see https://github.com/julian-corbet/nixdeploy-corbet-ch";
    homepage = "https://github.com/julian-corbet/nixdeploy-corbet-ch";
    license = lib.licenses.mit;
    mainProgram = "nixdeploy";
    platforms = lib.platforms.unix;
  };
}
