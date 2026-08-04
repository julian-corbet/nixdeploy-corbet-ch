# modules/adapters/systemd-scheduling.nix
#
# The `schedule` verb (`modules/default.nix`'s `activationAdapter`) for the two backends
# that have systemd: `nixos.nix` and `system-manager.nix`. Factored out rather than written
# twice because the two copies would be character-identical and the cost of them drifting is
# specific: every directive below is either load-bearing for a process that replaces the
# running system, or deliberately ABSENT for the same reason, and a correction applied to one
# copy and not the other leaves one backend's receiver running under reasoning nobody
# re-derived. The genuinely per-backend parts are NOT here -- which nix.conf a machine owns,
# what "currently running" means, which profile a rollback walks -- those stay in the two
# adapter files, where they differ.
#
# WHICH PRIVILEGES THE RECEIVER GENUINELY NEEDS
#
# A single receiver run does four things, and only one of them needs anything:
#
#   1. Read its own config. A world-readable store path; needs nothing.
#   2. Fetch the manifest, verify its signature, and fetch `.narinfo` metadata for the paths
#      it is missing. Needs outbound network and nothing else -- never a NAR body, never a
#      write.
#   3. Size the change against this machine's store. Needs to read `/nix/store` and to talk
#      to the Nix daemon socket (`/nix/var/nix/daemon-socket/socket`, world-writable by
#      design so unprivileged clients can query). Still needs no privilege.
#   4. Activate. This one is root, unavoidably and on every backend: `nix-env --set` writes
#      the system profile under `/nix/var/nix/profiles`, activation writes `/etc`, and
#      telling systemd to restart the units that changed is a privileged operation by
#      definition.
#
# So the unit runs as root because of step 4 and for no other reason, and it is worth stating
# that a privileged helper doing only step 4 would buy nothing. The thing such a helper would
# execute is `activation.activate` -- a command line named by the receiver's config file --
# so whoever can write that config can already run arbitrary code as root. The config file IS
# the root-equivalent artifact here; it is a store path produced by the same evaluation that
# produced the system itself. Splitting the process would relocate that authority, not reduce
# it, and would add an IPC surface that is a new way to get it wrong.
#
# Steps 2 and 3 also mean two things must be REACHABLE from inside this unit: `/nix/store`
# and the daemon socket. They are, because nothing below sandboxes the filesystem. Any future
# adapter that adds `ProtectSystem=`, `PrivateMounts=` or a mount namespace of its own has to
# bind both back in, or the receiver silently loses the ability to size a delta at all and
# reports every tick as a Delta-stage failure whose cause is nowhere near where it is read.
#
# WHAT HARDENING IS DELIBERATELY ABSENT, AND WHY THAT IS NOT AN OVERSIGHT
#
#   ProtectSystem=/ProtectHome=  activation writes `/etc` and creates user home directories
#                                (`users.users.<name>.createHome`). Either directive turns a
#                                working switch into a half-applied one.
#   RestrictSUIDSGID=            NixOS activation sets the setuid bits on `security.wrappers`.
#                                This directive blocks precisely that syscall.
#   NoNewPrivileges=             inherited by every process activation execs, including those
#                                wrappers once they exist.
#   PrivateDevices=,
#   ProtectKernelModules=,
#   ProtectKernelTunables=       an activation script may legitimately need a device node, a
#                                module, or a sysctl -- and finding out which one broke, from
#                                a machine that is now half-switched, is the worst possible
#                                place to learn it.
#
#   Environment=PATH=            an override was written here first, and the RENDERED unit is
#                                what showed it was wrong. NixOS already puts coreutils,
#                                findutils, gnugrep, gnused and systemd on every service's
#                                PATH, and it does that precisely because activation scripts
#                                call `grep`, `sed` and `find` by bare name. A "minimal" PATH
#                                of this file's own choosing does not harden anything -- a
#                                systemd unit's environment is defined by the manager, not
#                                inherited from whoever started it, so there was nothing to
#                                strip -- while narrowing below the backend's own floor is a
#                                way to break a switch halfway through, read downstream as
#                                "the thing being activated is broken" rather than "this unit
#                                was started wrong". nixdeploy itself resolves NOTHING off
#                                PATH: the three adapter command verbs, `receiver.nixBinary`
#                                and every `receiver.healthGate` entry are absolute store
#                                paths rendered by Nix. So the backend's own default stands.
#
# Sandboxing a process whose entire job is to replace the system is theatre: the directives
# that would meaningfully constrain it are exactly the ones that would stop it, and shipping
# them would trade a working switch for the appearance of safety. What is left is real, and
# small: exactly one command -- an absolute store path, the `receive` subcommand, `-config`
# and an absolute config path -- with no restart loop and no timeout that can kill a switch
# halfway through.
#
# No `pkgs`: this file references not one package. Everything the unit runs arrives in `argv`,
# already absolute, and the deliberate absence of a PATH override above is why nothing else is
# needed -- a scheduling verb that had to reach into a package set would be one that had
# opinions about what the receiver runs, which is `receiver.job`'s business and not this
# file's.
{ lib }:

{
  # mkSchedule :: { name, description, argv, intervalSeconds } -> attrs
  #
  # Exactly the signature `activationAdapter.schedule` declares. Returns a systemd
  # service/timer pair in the option vocabulary both NixOS and system-manager share (both
  # build `systemd.services` and `systemd.timers` on nixpkgs' own unit-option definitions),
  # so neither adapter has to translate anything.
  mkSchedule = { name, description, argv, intervalSeconds }: {
    systemd.services.${name} = {
      inherit description;

      # Deliberately NO `wantedBy`: the timer below is the only thing that ever starts this.
      # A service also pulled in by a target would run once at boot OUTSIDE the timer's own
      # accounting, and `OnUnitActiveSec` measures from the last activation of the unit --
      # so that stray run would silently shift every subsequent tick.
      after = [ "network-online.target" "nix-daemon.socket" ];
      wants = [ "network-online.target" ];

      serviceConfig = {
        Type = "oneshot";

        # `escapeShellArgs`, not a plain join: systemd re-splits `ExecStart` with its own
        # quoting rules, and `configPath` is an operator-settable string that is not
        # guaranteed to be a store path (see its own description). A path with a space in it
        # would otherwise arrive at the receiver as two arguments, the second of which it
        # ignores -- leaving a receiver reading its compiled-in default config path and
        # reporting a Config-stage failure that names a file nobody configured.
        ExecStart = lib.escapeShellArgs argv;

        # `Outcome::exit_code` (src/outcome.rs) deliberately does NOT collapse to POSIX's
        # 0/non-zero: converged=0, alreadyCurrent=1, reimaged=2, refused=3, failed=4. Only
        # the last is an error. Without this line the STEADY STATE of a converged fleet --
        # every machine reporting `alreadyCurrent` on every tick, which is what a working
        # deployment looks like -- would put every receiver unit into `failed`, turn
        # `systemctl --failed` permanently red fleet-wide, and make `Refused` (documented
        # everywhere in this repo as a first-class outcome and not an error) indistinguishable
        # from a broken run at the one place a supervisor looks.
        #
        # 4 is deliberately absent, and so is 64 (`EXIT_USAGE` in src/main.rs, "you invoked
        # this binary wrong") -- both must stay failures. `outcome.rs`'s own note says this
        # configuration belongs to whoever wires the binary up; this file is that.
        SuccessExitStatus = "1 2 3";

        # The timer IS the retry, and it is the only one. A `Restart=` on a run that failed
        # because the network was down would retry on systemd's cadence rather than the
        # operator's `interval`, and a run that failed because the MANIFEST is wrong would
        # retry forever at that same cadence for as long as it stays wrong.
        Restart = "no";

        # No finite value is defensible here. Anything short enough to catch a genuinely
        # wedged run is also short enough to kill a legitimate closure fetch on a slow link,
        # and a SIGTERM delivered in the middle of a switch is the one outcome worse than a
        # slow one: a half-applied system nobody asked for. A run that does wedge blocks the
        # next tick, because systemd will not start a second instance of a running oneshot --
        # which is correct, since two concurrent activations of the same machine is worse
        # than none.
        TimeoutStartSec = "infinity";

        SyslogIdentifier = name;
      };
    };

    systemd.timers.${name} = {
      inherit description;
      wantedBy = [ "timers.target" ];
      timerConfig = {
        # NOT `interval`. This is the settle margin before the FIRST check after a boot, not
        # a cadence: a machine that has just come back is disproportionately likely to be the
        # one that is behind, and waiting a full interval to discover that is exactly
        # backwards. One minute is a margin for routing to actually work
        # (`network-online.target` is a promise about ordering, not about reachability), not
        # a policy about how often to converge -- that policy is `interval`, and it is the
        # operator's.
        OnBootSec = "1min";
        OnUnitActiveSec = "${toString intervalSeconds}s";

        # No `Persistent=`: it applies only to `OnCalendar=` timers, and with
        # `OnBootSec`/`OnUnitActiveSec` a "missed" tick is not a concept -- the next one is
        # always measured from the last run. No `RandomizedDelaySec=` either: a fleet all
        # pointed at one manifest origin has a real thundering-herd question, but the right
        # spread depends on how many machines there are, which this repo deliberately does
        # not know. An operator who does adds it to this same timer, by name.
      };
    };
  };
}
