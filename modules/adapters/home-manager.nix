# modules/adapters/home-manager.nix
#
# Receiver adapter for a standalone Home Manager generation. Unlike the system backends,
# this receiver belongs to one explicit user identity and runs in that user's service
# manager. It must not run as root: Home Manager's activation package checks USER, HOME and
# (when declared) UID before touching the home, and bypassing those checks would turn a
# signed user-plane target into authority over a different account.
#
# Home Manager's current driver (`home-manager/home-manager`, `doSwitch`) performs a switch
# in two steps: advance the standard per-user `home-manager` nix-env profile, then run
# `$generation/activate --driver-version 1`. Its generated activation script derives the
# paths in `modules/lib-bash/activation-init.sh` and updates
# `$XDG_STATE_HOME/home-manager/gcroots/current-home` only after its activation DAG completes
# (`modules/home-environment.nix`). That GC root is therefore the observable `currentPath`;
# the profile alone is not, because it moves before activation and can point at a generation
# whose files and user units were never successfully applied.
{ config, lib, pkgs, ... }:
let
  cfg = config.nixdeploy;
  identity = cfg.receiver.plane.identity;
  homeDirectory = toString config.home.homeDirectory;
  stateHome = toString config.xdg.stateHome;
  cacheHome = toString config.xdg.cacheHome;

  q = lib.escapeShellArg;
  nixEnv = "${pkgs.nix}/bin/nix-env";
  nixBin = builtins.dirOf cfg.receiver.nixBinary;
  readlink = "${pkgs.coreutils}/bin/readlink";
  install = "${pkgs.coreutils}/bin/install";

  globalProfilesDir = "/nix/var/nix/profiles/per-user/${config.home.username}";
  globalCurrentRoot = "/nix/var/nix/gcroots/per-user/${config.home.username}/current-home";
  xdgProfilesDir = "${stateHome}/nix/profiles";
  xdgCurrentRoot = "${stateHome}/home-manager/gcroots/current-home";

  # Keep the same profile-selection decision in both profile-mutating verbs. Home
  # Manager prefers the XDG profile directory once it exists and otherwise uses Nix's
  # global per-user directory. On a fresh standalone installation neither may exist before
  # the first query, so create the XDG parent rather than treating first activation as an
  # error.
  selectProfile = ''
    xdg_profiles=${q xdgProfilesDir}
    global_profiles=${q globalProfilesDir}
    if [ -d "$xdg_profiles" ]; then
      profiles_dir="$xdg_profiles"
    elif [ -d "$global_profiles" ]; then
      profiles_dir="$global_profiles"
    else
      ${install} -d -m 700 "$xdg_profiles"
      profiles_dir="$xdg_profiles"
    fi
    profile="$profiles_dir/home-manager"
  '';

  # The XDG root is what current Home Manager writes. The global root is accepted only as
  # a migration fallback for a generation activated by an older Home Manager driver.
  readCurrentHome = ''
    xdg_current=${q xdgCurrentRoot}
    global_current=${q globalCurrentRoot}
    if [ -e "$xdg_current" ]; then
      ${readlink} -f "$xdg_current"
    elif [ -e "$global_current" ]; then
      ${readlink} -f "$global_current"
    else
      echo nixdeploy-uninitialized
    fi
  '';

  currentPathScript = pkgs.writeShellScript "nixdeploy-home-manager-current-path" ''
    set -u
    ${readCurrentHome}
  '';

  # Used after both a forward switch and a rollback. It deliberately does not move the
  # generation profile: the caller has already done so, and re-registering a rollback
  # target would mint a new generation instead of preserving the rollback.
  applyAndVerifyScript = pkgs.writeShellScript "nixdeploy-home-manager-apply-and-verify" ''
    set -u
    target="''${1:?nixdeploy-home-manager-apply-and-verify: no store path given}"

    # Home Manager's generated activation script still invokes the legacy Nix entry points
    # (`nix-build` for its sanity check and `nix-env` for profile setup) by bare name. A
    # foreign host may keep its Nix installation outside the user manager's PATH, while
    # receiver.nixBinary already names the exact working client on that host. Give the
    # activation that client's complete bin directory without replacing the host's baseline
    # PATH, so every sibling entry point resolves from the same installation.
    export PATH=${q nixBin}:''${PATH:-}

    if [ ! -x "$target/activate" ]; then
      echo "nixdeploy: home-manager: $target/activate is missing or not executable -- not a Home Manager activation package?" >&2
      exit 1
    fi

    "$target/activate" --driver-version 1
    activate_status=$?
    current="$(${readCurrentHome})"

    if [ "$current" = "$target" ]; then
      exit 0
    fi
    if [ "$activate_status" -eq 0 ]; then
      echo "nixdeploy: home-manager: activate exited 0 but current-home ($current) is not $target" >&2
    else
      echo "nixdeploy: home-manager: activate exited $activate_status and current-home remains $current" >&2
    fi
    exit 1
  '';

  activateScript = pkgs.writeShellScript "nixdeploy-home-manager-activate" ''
    set -u
    target="''${1:?nixdeploy-home-manager-activate: no store path given}"
    ${selectProfile}

    # Registration is part of a switch, not a best-effort convenience: without it the
    # previous generation cannot be selected by rollback after a failed health gate.
    if ! ${nixEnv} --profile "$profile" --set "$target"; then
      echo "nixdeploy: home-manager: could not register $target in $profile; refusing to activate without rollback history" >&2
      exit 1
    fi

    exec ${applyAndVerifyScript} "$target"
  '';

  rollbackScript = pkgs.writeShellScript "nixdeploy-home-manager-rollback" ''
    set -u
    ${selectProfile}

    if [ ! -e "$profile" ] || ! ${nixEnv} --profile "$profile" --rollback; then
      echo "nixdeploy: home-manager: rollback failed for $profile -- likely no previous generation" >&2
      exit 1
    fi
    target="$(${readlink} -f "$profile")"
    exec ${applyAndVerifyScript} "$target"
  '';

  # Linux Home Manager owns per-user systemd units. StateDirectory et al are valid in a
  # user manager and expand below XDG_STATE_HOME, XDG_CACHE_HOME and XDG_RUNTIME_DIR,
  # respectively; they do not create or consult a privileged account's home.
  systemdSchedule = { name, description, argv, intervalSeconds }: {
    systemd.user = {
      enable = true;
      services.${name} = {
        Unit.Description = description;
        Service = {
          Type = "oneshot";
          StateDirectory = "nixdeploy";
          CacheDirectory = "nixdeploy";
          RuntimeDirectory = "nixdeploy";
          Environment = [
            "HOME=${homeDirectory}"
            "USER=${config.home.username}"
            "XDG_STATE_HOME=${stateHome}"
            "XDG_CACHE_HOME=${cacheHome}"
          ];
          ExecStart = lib.escapeShellArgs argv;
          SuccessExitStatus = "1 2 3";
          Restart = "no";
          TimeoutStartSec = "infinity";
          SyslogIdentifier = name;
        };
      };
      timers.${name} = {
        Unit.Description = description;
        Timer = {
          OnBootSec = "1min";
          OnUnitActiveSec = "${toString intervalSeconds}s";
        };
        Install.WantedBy = [ "timers.target" ];
      };
    };
  };

  # Home Manager also evaluates on Darwin. A background (`user`, not GUI) LaunchAgent is
  # the corresponding unprivileged scheduler there. launchd has no StateDirectory
  # primitive, so a tiny wrapper creates the same dedicated XDG state/cache directories
  # before execing the receiver and exposes their paths through the same environment names
  # systemd supplies.
  launchdSchedule = { name, description, argv, intervalSeconds }:
    let
      stateDirectory = "${stateHome}/nixdeploy";
      cacheDirectory = "${cacheHome}/nixdeploy";
      runner = pkgs.writeShellScript "${name}-home-manager-run" ''
        set -eu
        runtime_directory="''${TMPDIR:?nixdeploy: launchd did not provide a per-user TMPDIR}/nixdeploy"
        ${install} -d -m 700 ${q stateDirectory} ${q cacheDirectory} "$runtime_directory"
        export STATE_DIRECTORY=${q stateDirectory}
        export CACHE_DIRECTORY=${q cacheDirectory}
        export RUNTIME_DIRECTORY="$runtime_directory"
        export XDG_RUNTIME_DIR="$runtime_directory"
        exec ${lib.escapeShellArgs argv}
      '';
    in
    {
      launchd = {
        enable = true;
        agents.${name} = {
          enable = true;
          domain = "user";
          config = {
            ProgramArguments = [ "${runner}" ];
            StartInterval = intervalSeconds;
            RunAtLoad = true;
            EnvironmentVariables = {
              HOME = homeDirectory;
              USER = config.home.username;
              XDG_STATE_HOME = stateHome;
              XDG_CACHE_HOME = cacheHome;
            };
            StandardOutPath = "${homeDirectory}/Library/Logs/${name}.log";
            StandardErrorPath = "${homeDirectory}/Library/Logs/${name}.log";
          };
        };
      };
    };

  schedule = if pkgs.stdenv.hostPlatform.isDarwin then launchdSchedule else systemdSchedule;

  forward = (import ./apply.nix { inherit lib; }).forward {
    adapter = "home-manager";
    # Home Manager declares BOTH trees on every platform (modules/modules.nix imports
    # launchd/default.nix and systemd.nix unconditionally), then defaults only the native one
    # on. Keeping this list literal is load-bearing: homeManagerConfiguration supplies `pkgs`
    # through config._module.args rather than specialArgs, so using `pkgs` to choose a top-level
    # name while the module system is still collecting those names recurses through `config`.
    # The schedule value below may choose lazily once config exists; this registry may not.
    trees = [ "launchd" "systemd" ];
  };
in
{
  config = lib.mkIf (cfg.backend == "home-manager") (lib.mkMerge [
    {
      nixdeploy.receiver.stateDirectory = "${stateHome}/nixdeploy";

      assertions = [
        {
          assertion = !cfg.receiver.enable || identity == config.home.username;
          message = ''
            nixdeploy: a home-manager receiver's plane identity must equal home.username.
            The scheduled receiver runs as that user and may not activate another identity's
            signed home generation.
          '';
        }
        {
          assertion = !cfg.receiver.enable || config.home.activationGenerateGcRoot;
          message = ''
            nixdeploy: backend "home-manager" requires home.activationGenerateGcRoot = true.
            Its currentPath is the current-home GC root Home Manager advances only after a
            successful activation; disabling that root would make convergence unobservable.
          '';
        }
        {
          assertion = !cfg.publisher.enable;
          message = ''
            nixdeploy: the scheduled publisher is not available in a home-manager user plane.
            Publication is a host/service responsibility, not authority granted to a user's
            configuration activation.
          '';
        }
      ];

      nixdeploy.receiver.activation = {
        activate = "${activateScript}";
        currentPath = "${currentPathScript}";
        rollback = "${rollbackScript}";
        inherit schedule;

        # A Home Manager module can write a user's nix.conf, but a multi-user nix-daemon
        # does not take substitution limits from it. Refuse rather than claim a receiver
        # memory ceiling is active when the daemon that downloads paths never read it.
        nixSettings = { httpConnections, downloadBufferSize }:
          lib.throwIf (httpConnections != null || downloadBufferSize != null) ''
            nixdeploy: backend "home-manager" cannot apply receiver.httpConnections or
            receiver.downloadBufferSize. Configure those daemon-side settings in the host's
            NixOS, nix-darwin, system-manager, or foreign Nix installation instead, then
            leave both receiver options unset here.
          '' { };
      };

      # The assertion above is the refusal. Keep this total because Home Manager normalizes
      # both declared option trees and can force a schedule result even while publisher.enable
      # is false. An eager throw here made an inert receiver configuration impossible to load.
      nixdeploy.publisher.schedule = _: {
        launchd = { };
        systemd = { };
      };
    }

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
