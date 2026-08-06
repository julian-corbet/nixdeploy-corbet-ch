# modules/adapters/nixos.nix
#
# The reference activation adapter for `nixdeploy.backend = "nixos"`. Sets all five verbs
# `modules/default.nix`'s `activationAdapter` submodule declares on
# `nixdeploy.receiver.activation`, and applies the two of them that return configuration.
# Everything below the header is the THREE COMMAND verbs -- `activate`, `currentPath`,
# `rollback` -- built on NixOS's own generation/activation machinery. Nothing this file
# invents: all of it verified against real NixOS/Nixpkgs source rather than assumed, because a
# wrong guess here does not fail loudly, it activates the wrong thing or reports success for a
# machine that didn't change.
#
# THE EXIT-CODE BUG THIS FILE IS BUILT AROUND
#
# `$closure/bin/switch-to-configuration switch` (the wrapped `switch-to-configuration-ng`
# every NixOS closure ships at that path -- see
# `nixos/modules/system/activation/switchable-system.nix` in Nixpkgs) updates
# `/run/current-system` to the new closure BEFORE it starts stopping, reloading or
# restarting a single systemd unit -- confirmed directly in its own source, a comment on
# the per-user activation path in `switch-to-configuration-ng`'s `main.rs`: "By the time
# this child runs, /etc (and /run/current-system) have already been switched to the new
# configuration." Unit stop/reload/restart/start failures that happen AFTER that point
# each set the process's own exit code to a non-zero "jobs failed" value (4, in the
# current implementation) -- so a switch that fully applied the requested configuration,
# with `/run/current-system` correctly pointing at it, can still exit non-zero because one
# unrelated unit (a user's own broken service, a flaky third-party daemon) failed to
# restart. The reverse also happens in principle: an exit 0 proves nothing if the tool
# never actually ran (missing binary, wrong permissions caught before exec).
#
# This is EXACTLY the ambiguity `activationAdapter.activate`'s own description warns
# about, and the reason it requires the adapter to "exit non-zero if and only if the
# machine did not end up running that closure" rather than simply forwarding whatever the
# underlying tool returned. `applyAndVerify` below is where that gets resolved: it always
# lets `switch-to-configuration` run to completion, then throws its exit code away and
# asks the machine itself (a fresh read of `/run/current-system`) whether the requested
# closure is what's actually running now. `src/activate.rs` (the receiver binary) performs
# this exact same re-read independently, as its own defense in depth -- but this script's
# own exit code is still a documented part of `activationAdapter.activate`'s contract, and
# a human running it by hand, or any future caller that isn't `src/activate.rs`, is
# entitled to a correct answer without having to know that history.
#
# WHY `nix-env --set` HAPPENS TOO, NOT JUST `switch-to-configuration`
#
# `switch-to-configuration` on its own never touches `/nix/var/nix/profiles/system` (the
# generation-numbered profile `nixos-rebuild --rollback` and `--list-generations` read).
# Real `nixos-rebuild switch` sets that profile itself, via a plain `nix-env --profile
# /nix/var/nix/profiles/system --set <path>`, immediately before invoking
# `switch-to-configuration` -- confirmed in `nixos-rebuild-ng`'s own
# `nixos_rebuild/services.py` (`_activate_system`'s `Action.SWITCH | Action.BOOT if not
# args.rollback` branch calls `nix.set_profile(...)` then `nix.switch_to_configuration(...)`
# in that order) and `nixos_rebuild/nix.py` (`set_profile` -> `["nix-env", "-p",
# profile.path, "--set", path_to_config]`). Skipping this step would still make the
# machine run the right closure right now, but would leave `rollback` below (and any human
# later running `nixos-rebuild --rollback` on this same machine) unable to get back to
# whatever this adapter activated -- the generation history would simply never include it.
# This adapter deliberately writes to the SAME profile path real `nixos-rebuild` uses, so
# the two share one generation history rather than keeping two that silently disagree.
#
# `nixos-rebuild-ng`'s own rollback path (`Action.SWITCH | Action.BOOT | Action.TEST |
# Action.DRY_ACTIVATE` branch, i.e. whenever `args.rollback` is true) deliberately does
# NOT call `set_profile` again -- `nix.rollback()` already moved the profile pointer via
# `nix-env --rollback`, and re-running `--set` on top of that would mint a brand new
# generation identical to the one just rolled back to, instead of preserving the "went
# back" the profile's own generation list otherwise shows. `rollback` below follows the
# same asymmetry: roll the profile back, then apply+verify against whatever it now points
# at, without registering anything new.
#
# THE OTHER TWO VERBS: `schedule` AND `nixSettings`
#
# `modules/default.nix`'s `activationAdapter` asks a backend for five things, not three. The
# scheduling half lives in `systemd-scheduling.nix` in this same directory, shared with
# `system-manager.nix` because both backends' answer is the same systemd service/timer pair
# -- read that file for which privileges the receiver genuinely needs and which hardening
# directives would break the one thing it exists to do. `nixSettings` lives in `nix-conf.nix`,
# shared with `nix-darwin.nix`: on both of those backends the machine's `nix.conf` is part of
# the very closure this module replaces, so the two memory ceilings are ordinary
# `nix.settings` entries and a switch restarts `nix-daemon` for them by itself.
# `system-manager.nix` imports neither of those two lines and throws instead, because on a
# foreign distro that file belongs to somebody else.
{ config, lib, pkgs, ... }:
let
  cfg = config.nixdeploy;

  systemProfile = "/nix/var/nix/profiles/system";

  scheduling = import ./systemd-scheduling.nix { inherit lib; };
  publisherScheduling = import ./publisher-systemd-scheduling.nix { inherit lib; };
  publisherSchedule = job:
    lib.recursiveUpdate
      (publisherScheduling.mkSchedule (job // { dynamicUser = false; }))
      {
        users.groups.nixdeploy-publisher = { };
        users.users.nixdeploy-publisher = {
          isSystemUser = true;
          group = "nixdeploy-publisher";
          home = "/var/lib/nixdeploy-publisher";
          createHome = false;
        };
      };
  nixConf = import ./nix-conf.nix { inherit lib; };

  # The two option trees a NixOS machine has that nixdeploy writes into, listed literally --
  # see apply.nix for why the list has to be a literal here rather than anything read out of
  # `config`, and why that is the reason these two verbs are applied by an adapter at all.
  forward = (import ./apply.nix { inherit lib; }).forward {
    adapter = "nixos";
    trees = [ "systemd" "nix" "users" ];
  };

  # Every external tool below is referenced by absolute Nix store path
  # (`${pkgs.foo}/bin/foo`), never a bare name resolved off whatever PATH the receiver's
  # own systemd unit happens to have. A bare name that silently isn't on that PATH doesn't
  # fail this script at build time -- it fails at run time with "command not found", read
  # by everything downstream as "the thing this command was checking is broken" rather
  # than "this command itself couldn't even start." That exact confusion has already
  # produced a real multi-day silent outage in this family; it is not a hypothetical here.
  # `$target/bin/switch-to-configuration` is the one exception, and it is not really an
  # exception: it is a store path too, just one supplied at RUN time (the argument this
  # adapter is handed) rather than baked in at eval time.
  nixEnv = "${pkgs.nix}/bin/nix-env";
  readlink = "${pkgs.coreutils}/bin/readlink";

  # The one place "what is this machine running right now" is defined, reused verbatim by
  # the standalone `currentPath` command AND by `applyAndVerify`'s own disambiguation --
  # nixnet's `identityHealthCheckBash` is the precedent for factoring a check out this way,
  # after the same check was once found duplicated (and independently buggy) in more than
  # one place. Two definitions of "current" that could ever disagree would be a correctness
  # bug, not a style nit: `src/receive.rs` trusts `currentPath` alone to decide `AlreadyCurrent`
  # vs. "needs to converge" before this adapter's `activate` ever runs.
  #
  # Never empty, never non-zero: `src/activate.rs`'s `run_capturing` (the receiver's own
  # caller of this command) treats a non-zero exit OR empty trimmed stdout as a hard error
  # that aborts the whole run, including on a receiver's very first-ever tick -- which is
  # exactly the tick a genuinely correct answer here matters most for. On a real NixOS
  # machine `/run/current-system` cannot be missing while this script is executing: the
  # receiver invoking it is itself a process running from the very system that symlink
  # names, so the "never activated yet" case this guard exists for cannot actually arise on
  # this backend. It stays anyway, at the same cost as leaving it out, so this file matches
  # `system-manager.nix` and `nix-darwin.nix` (where the guarantee genuinely does not
  # hold) instead of being the one adapter that silently assumes a guarantee the others
  # can't make.
  readCurrentSystem = ''
    if [ -e /run/current-system ]; then
      ${readlink} -f /run/current-system
    else
      echo nixdeploy-uninitialized
    fi
  '';

  currentPathScript = pkgs.writeShellScript "nixdeploy-nixos-current-path" ''
    set -u
    ${readCurrentSystem}
  '';

  # Applies `$1` and reports whether the machine ended up running it -- shared by
  # `activate` (which registers the generation first) and `rollback` (which does not, see
  # this file's header). Deliberately no `set -e`: this needs `switch-to-configuration`'s
  # own exit code as a diagnostic without letting it abort the script before the
  # disambiguation below gets to run.
  applyAndVerifyScript = pkgs.writeShellScript "nixdeploy-nixos-apply-and-verify" ''
    set -u
    target="''${1:?nixdeploy-nixos-apply-and-verify: no store path given}"

    if [ ! -x "$target/bin/switch-to-configuration" ]; then
      echo "nixdeploy: nixos: $target/bin/switch-to-configuration is missing or not executable -- not a NixOS system closure?" >&2
      exit 1
    fi

    "$target/bin/switch-to-configuration" switch
    switch_status=$?

    current="$(${readCurrentSystem})"
    if [ "$current" = "$target" ]; then
      # See this file's header: a non-zero $switch_status here does not mean the machine
      # is not running $target -- it means some unrelated unit failed to (re)start after
      # /run/current-system had already been repointed. That is a real problem worth
      # having failed loudly on its own stderr above (switch-to-configuration already
      # printed it), but it is not THIS command's contract to report -- the contract is
      # "did the machine end up running $target," and it did.
      exit 0
    fi

    if [ "$switch_status" -eq 0 ]; then
      echo "nixdeploy: nixos: switch-to-configuration exited 0 but /run/current-system ($current) is not $target -- treating as failed" >&2
    else
      echo "nixdeploy: nixos: switch-to-configuration exited $switch_status and /run/current-system ($current) is still not $target" >&2
    fi
    exit 1
  '';

  activateScript = pkgs.writeShellScript "nixdeploy-nixos-activate" ''
    set -u
    target="''${1:?nixdeploy-nixos-activate: no store path given}"

    # Best-effort, not fatal: see this file's header for why this is the same profile
    # `nixos-rebuild switch` itself writes, and why a failure here should not stop the
    # machine from still becoming $target -- what this adapter's own contract cares about
    # is decided by applyAndVerifyScript below, not by whether the generation got recorded.
    if ! ${nixEnv} -p ${systemProfile} --set "$target"; then
      echo "nixdeploy: nixos: nix-env --set on ${systemProfile} failed -- proceeding to activate anyway, but a later rollback will not be able to return to this generation" >&2
    fi

    exec ${applyAndVerifyScript} "$target"
  '';

  rollbackScript = pkgs.writeShellScript "nixdeploy-nixos-rollback" ''
    set -u

    if ! ${nixEnv} --rollback -p ${systemProfile}; then
      echo "nixdeploy: nixos: nix-env --rollback -p ${systemProfile} failed -- likely no previous generation to roll back to" >&2
      exit 1
    fi

    target="$(${readlink} -f ${systemProfile})"
    exec ${applyAndVerifyScript} "$target"
  '';
in
{
  # Guarded on `cfg.backend` rather than assumed from being imported at all: a plain
  # assignment below means two adapters imported into the same evaluation by mistake fail
  # loudly with Nix's own "conflicting definitions" error instead of one silently winning --
  # exactly the failure mode worth keeping loud.
  config = lib.mkIf (cfg.backend == "nixos") (lib.mkMerge [
    {
      nixdeploy.receiver.activation = {
        activate = "${activateScript}";
        currentPath = "${currentPathScript}";
        rollback = "${rollbackScript}";

        schedule = scheduling.mkSchedule;
        nixSettings = nixConf.mkNixSettings;
      };

      nixdeploy.publisher.schedule = publisherSchedule;
    }

    # Applying the two verbs is deliberately separate from defining them, and deliberately
    # goes through `cfg.receiver.activation.*` rather than the local bindings above: those
    # are DEFAULTS this adapter contributes, and an operator who replaces either one --
    # `nixdeploy.receiver.activation.schedule = ...` to spread a fleet's ticks, say -- must
    # get their version applied, not this file's.
    (lib.mkIf cfg.receiver.enable (lib.mkMerge [
      (forward "schedule" (cfg.receiver.activation.schedule cfg.receiver.job))
      (forward "nixSettings" (cfg.receiver.activation.nixSettings {
        inherit (cfg.receiver) httpConnections downloadBufferSize;
      }))
    ]))

    (lib.mkIf cfg.publisher.enable
      (forward "publisherSchedule" (cfg.publisher.schedule cfg.publisher.job)))
  ]);
}
