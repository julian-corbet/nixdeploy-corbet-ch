# modules/adapters/nix-darwin.nix
#
# The activation adapter for `nixdeploy.backend = "nix-darwin"`. Structurally almost
# identical to nixos.nix in this same directory -- nix-darwin models its own
# generation/activation machinery directly on NixOS's, down to reusing the SAME profile
# path -- but the closure-side entry point and the receiver's own build locality both
# differ, and both differences are load-bearing, not cosmetic.
#
# ALWAYS LOCAL-BUILD -- THIS ADAPTER NEVER RECEIVES A CLOSURE IT DID NOT BUILD ITSELF
#
# `modules/default.nix` already asserts `cfg.backend == "nix-darwin" -> cfg.receiver.
# buildLocality == "local"`: a darwin closure cannot be produced on a Linux builder, so a
# Mac can only ever build its own and this adapter's `activate` is always handed a store
# path this same machine just built, never one fetched from a remote cache on the strength
# of a manifest alone. That assertion is enforced once, centrally, in `modules/default.nix`
# -- this file does not repeat it, but every comment below assumes it holds.
#
# `$1/activate` -- AT THE CLOSURE ROOT, NOT UNDER `bin/`
#
# nix-darwin's own `darwin-rebuild switch` (`pkgs/nix-tools/darwin-rebuild.sh` in
# nix-darwin/nix-darwin, read directly while writing this file) does, for action=switch:
# `nix-env -p "$profile" --set "$systemConfig"` followed by `"$systemConfig/activate"` --
# where `$profile` defaults to `/nix/var/nix/profiles/system` (`pkgs/nix-tools/default.nix`'s
# `profile ? "/nix/var/nix/profiles/system"`), the EXACT profile path NixOS's own
# `nixos-rebuild` uses. `activate` sits at the closure's own top level (produced by
# `system.build.toplevel`'s activation-script derivation, the same mechanism NixOS uses,
# just placed at `$out/activate` instead of `$out/bin/switch-to-configuration`) -- this is
# what the task's own framing means by "the closure's own activation script."
#
# For `--rollback`, `darwin-rebuild.sh` does NOT call `nix-env --set` again: it runs
# `nix-env -p "$profile" --rollback`, reads the now-current `$systemConfig` back out, and
# runs `"$systemConfig/activate"` -- the same asymmetry nixos.nix documents for
# `nixos-rebuild`, applied by nix-darwin itself. `rollbackScript` below reproduces it.
#
# THE SAME EXIT-CODE CAUTION AS NIXOS, APPLIED WITHOUT AN EQUIVALENT DOCUMENTED INCIDENT
#
# `switch-to-configuration-ng`'s "current-system-switches-before-units-do" behaviour is
# documented directly in its own source (see nixos.nix's header for the citation); no
# equivalent public documentation or source comment was found for nix-darwin's own
# `activate` making the same guarantee, or failing to. `applyAndVerify` below applies the
# identical defensive pattern anyway -- re-read `/nix/var/nix/profiles/system` after
# `activate` runs and trust THAT over the exit code -- because `activationAdapter.activate`'s
# contract in `modules/default.nix` requires this "if and only if" property unconditionally,
# not only where a specific tool is already known to violate it. Applying it uniformly costs
# nothing when the underlying tool's exit code happens to already be trustworthy, and is the
# only correct choice when it might not be.
{ config, lib, pkgs, ... }:
let
  cfg = config.nixdeploy;

  systemProfile = "/nix/var/nix/profiles/system";

  # Same absolute-store-path convention as nixos.nix and system-manager.nix in this
  # directory -- see either file's identical comment for why a bare tool name resolved off
  # an unknown PATH is never acceptable here.
  nixEnv = "${pkgs.nix}/bin/nix-env";
  readlink = "${pkgs.coreutils}/bin/readlink";

  # Single definition of "what generation is registered right now" -- see nixos.nix's
  # identical comment on `readCurrentSystem` for why factoring this out once, reused by
  # both the standalone `currentPath` command and `applyAndVerify`'s own disambiguation,
  # is load-bearing rather than a style choice.
  #
  # The "never activated yet" guard is defensive here, the same way it is on nixos.nix and
  # for the same reason: `buildLocality == "local"` means this adapter's own receiver
  # process is itself running on a Mac that nix-darwin already activated at least once (it
  # is how the receiver got there), so `systemProfile` should always resolve. It stays
  # anyway, at zero extra cost, rather than being the one adapter in this directory that
  # quietly assumes a guarantee `system-manager.nix` genuinely cannot make -- and
  # `src/activate.rs`'s `run_capturing` (this command's real caller) treats a non-zero exit
  # or empty output as a hard error regardless of which backend it's talking to, so there is
  # no reward for skipping it.
  readCurrentSystem = ''
    if [ -e ${systemProfile} ]; then
      ${readlink} -f ${systemProfile}
    else
      echo nixdeploy-uninitialized
    fi
  '';

  currentPathScript = pkgs.writeShellScript "nixdeploy-nix-darwin-current-path" ''
    set -u
    ${readCurrentSystem}
  '';

  # Applies `$1` and reports whether the machine ended up running it -- shared by
  # `activate` (which registers the generation first) and `rollback` (which does not, per
  # `darwin-rebuild.sh`'s own asymmetry, see this file's header). No `set -e`: `activate`'s
  # own exit code is wanted as a diagnostic, not as something allowed to abort this script
  # before the disambiguation below runs.
  applyAndVerifyScript = pkgs.writeShellScript "nixdeploy-nix-darwin-apply-and-verify" ''
    set -u
    target="''${1:?nixdeploy-nix-darwin-apply-and-verify: no store path given}"

    if [ ! -x "$target/activate" ]; then
      echo "nixdeploy: nix-darwin: $target/activate is missing or not executable -- not a nix-darwin system closure?" >&2
      exit 1
    fi

    "$target/activate"
    activate_status=$?

    current="$(${readCurrentSystem})"
    if [ "$current" = "$target" ]; then
      exit 0
    fi

    if [ "$activate_status" -eq 0 ]; then
      echo "nixdeploy: nix-darwin: activate exited 0 but ${systemProfile} ($current) is not $target -- treating as failed" >&2
    else
      echo "nixdeploy: nix-darwin: activate exited $activate_status and ${systemProfile} ($current) is still not $target" >&2
    fi
    exit 1
  '';

  activateScript = pkgs.writeShellScript "nixdeploy-nix-darwin-activate" ''
    set -u
    target="''${1:?nixdeploy-nix-darwin-activate: no store path given}"

    # Best-effort, not fatal -- same reasoning as nixos.nix's identical step: skipping this
    # would still let applyAndVerifyScript below make the machine run $target right now, it
    # would just leave the generation history (and therefore `rollback`, and a human's own
    # `darwin-rebuild --rollback` on this same machine) unable to get back to it.
    if ! ${nixEnv} -p ${systemProfile} --set "$target"; then
      echo "nixdeploy: nix-darwin: nix-env --set on ${systemProfile} failed -- proceeding to activate anyway, but a later rollback will not be able to return to this generation" >&2
    fi

    exec ${applyAndVerifyScript} "$target"
  '';

  rollbackScript = pkgs.writeShellScript "nixdeploy-nix-darwin-rollback" ''
    set -u

    if ! ${nixEnv} --rollback -p ${systemProfile}; then
      echo "nixdeploy: nix-darwin: nix-env --rollback -p ${systemProfile} failed -- likely no previous generation to roll back to" >&2
      exit 1
    fi

    target="$(${readlink} -f ${systemProfile})"
    exec ${applyAndVerifyScript} "$target"
  '';
in
{
  # Guarded on `cfg.backend` rather than assumed from being imported at all -- see
  # nixos.nix's identical comment for why a plain assignment (not mkDefault) is the point:
  # two adapters imported into the same evaluation by mistake should fail loudly with
  # Nix's own "conflicting definitions" error, not silently pick one.
  config = lib.mkIf (cfg.backend == "nix-darwin") {
    nixdeploy.receiver.activation = {
      activate = "${activateScript}";
      currentPath = "${currentPathScript}";
      rollback = "${rollbackScript}";
    };
  };
}
