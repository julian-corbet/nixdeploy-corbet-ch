# checks/emission.nix
#
# Proves the module actually PRODUCES something. `checks/assertions.nix` next door proves the
# option surface refuses configurations it should refuse -- an entirely different question,
# and one a module that emits nothing at all passes perfectly. This file is the other half:
# under each of the four backends, enabling `nixdeploy.receiver` must yield a scheduled unit
# that runs the receiver binary against a config file whose contents are exactly the schema
# `src/receive.rs` deserializes.
#
# TWO EVALUATORS, ON PURPOSE
#
# `evalWith` below uses a bare `lib.evalModules` plus `platformStub`, which declares the
# option TREES the four backends write into (`systemd`, `launchd`, `nix`, `users`, `home`,
# `xdg`) as opaque attrsets and nothing more. That is not a shortcut around a real evaluator,
# it is the only way to ask
# the question at all for three of the four: Home Manager, nix-darwin and system-manager are
# deliberately not flake inputs here (see flake.nix's own header on why), so their option
# surfaces do not exist
# in this evaluation and a launchd daemon has nowhere real to land. What the stub proves is
# precisely what this repo is responsible for -- that the adapter emits a launchd daemon with
# the right program, label handling and interval, rather than a systemd unit or nothing --
# and it deliberately proves nothing about whether nix-darwin accepts it, because this repo
# cannot know that without taking a dependency it has good reasons to refuse.
#
# `realNixos` then re-does the nixos backend through NixOS's own eval-config.nix and forces
# the RENDERED unit file text, which is the strongest statement available here: not "the
# module set an option", but "systemd's own unit generator turned it into a file, and here is
# what is in it".
{ pkgs, lib, nixpkgs, home-manager, system, nixdeployModule, backendAdapters }:

let
  check = name: ok: detail: { inherit name ok detail; };

  # Forces `expr` to weak head normal form inside `tryEval`. Deliberately NOT `deepSeq`: some
  # values under test hold whole derivations (`systemd.services.<n>.path`), and deep-forcing a
  # derivation is both slow and a way to fail a test for a reason that has nothing to do with
  # what it is asking. Every use below forces something -- `builtins.attrNames`, a string
  # comparison -- that is already enough to reach the code being tested.
  throwsOnForce = expr: !(builtins.tryEval (builtins.seq expr true)).success;

  # Substring test that works on strings carrying STRING CONTEXT, which is nearly everything
  # under test here: an `ExecStart` interpolating a package, a rendered unit file naming half
  # a dozen store paths. `lib.hasInfix` is `builtins.match` underneath, and `builtins.match`
  # refuses a pattern that references a store path -- "the string '...' is not allowed to refer
  # to a store path". Dropping the context is exactly right for a comparison: nothing here
  # BUILDS what it is looking at, it only asks what the text says.
  #
  # Being `builtins.match` underneath also means the needle is a REGEX, so keep needles free of
  # `[ ] ( ) * + ?`. A `"[Timer]"` needle, for instance, is a character class matching any one
  # of T, i, m, e or r -- it would pass against almost any text, which is the shape of test
  # that looks strictest and proves least.
  contains = needle: haystack:
    lib.hasInfix
      (builtins.unsafeDiscardStringContext needle)
      (builtins.unsafeDiscardStringContext haystack);

  # The one name `modules/default.nix` gives the receiver's scheduled unit on every backend.
  unitName = "nixdeploy-receiver";
  publisherUnitName = "nixdeploy-publisher";

  # `example.org` and a syntactically plausible but entirely fake cache key -- never a value
  # that could resolve to anything real.
  manifestUrl = "https://cache.example.org/manifest.json";
  manifestKey = "cache.example.org-1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
  intervalSeconds = 900;
  ceilingBytes = 500 * 1024 * 1024;
  healthGate = [ "/nix/store/00000000000000000000000000000000-example/bin/health-check" ];

  # Distinctive numbers, not round ones: if either knob were being read from the wrong option
  # (or defaulted), a 4/64MiB pair could plausibly match by accident. These cannot.
  httpConnections = 3;
  downloadBufferSize = 33554432;

  # `reimage` and `metrics` fixtures. Fake-but-plausible, same as `manifestUrl`/`manifestKey`
  # above -- never a value that could resolve to anything real.
  reimageCommand = "/nix/store/00000000000000000000000000000000-example/bin/reimage";
  reimageRequest = { command = reimageCommand; role = "primary"; };
  bootReconcileCommand = "/nix/store/00000000000000000000000000000000-example/bin/reconcile";
  bootReconcileRequest = { command = bootReconcileCommand; role = "nixrescue"; };
  metricsTextfile = "/var/lib/collector/nixdeploy.prom";
  metricsPushUrl = "https://pushgateway.example.org/metrics/job/nixdeploy";

  receiverFixture = {
    nixdeploy.provider = "example-provider";
    nixdeploy.receiver = {
      enable = true;
      manifest = { url = manifestUrl; publicKey = manifestKey; };
      maxInplaceDeltaBytes = ceilingBytes;
      interval = intervalSeconds;
      inherit healthGate;
    };
  };

  # Home Manager's adapter has one extra authority boundary: the signed plane identity must
  # be the same account whose module evaluation and user service perform the activation.
  # These are ordinary Home Manager values in a real composition; the opaque evaluator below
  # declares only the names because Home Manager deliberately is not a flake input here.
  homeReceiverFixture = lib.recursiveUpdate receiverFixture {
    nixdeploy.receiver.plane.identity = "alice";
    # Home Manager's nix.package is nullable: null means the host, not this user module,
    # owns the installation. The generic nixBinary default must still select a usable client.
    nix.package = null;
    home = {
      username = "alice";
      homeDirectory = "/home/alice";
      activationGenerateGcRoot = true;
    };
    xdg = {
      stateHome = "/home/alice/.local/state";
      cacheHome = "/home/alice/.cache";
    };
  };

  publisherFixture = {
    nixdeploy.publisher = {
      enable = true;
      targetsFile = "/nix/store/00000000000000000000000000000000-targets.json";
      revision = "0123456789abcdef";
      signingKeyFile = "/run/secrets/nixdeploy/signing-key";
      baseManifest = "/var/lib/nixdeploy-publisher/manifest.json";
      select.hosts = [ "host-a" "host-b" ];
      select.planes = [ "nixos" "home-manager" ];
      interval = 777;
    };
  };

  # Declares the option trees the adapters in this repo write into, as opaque attrsets.
  # Nothing here validates their contents -- that is the point: this stub must not accidentally
  # become a second, worse implementation of systemd's or launchd's own option surface, or the
  # checks would start proving things about the stub.
  platformStub = { ... }: {
    options = {
      systemd = lib.mkOption { type = lib.types.attrs; default = { }; };
      launchd = lib.mkOption { type = lib.types.attrs; default = { }; };
      nix = lib.mkOption { type = lib.types.attrs; default = { }; };
      users = lib.mkOption { type = lib.types.attrs; default = { }; };
      home = lib.mkOption { type = lib.types.attrs; default = { }; };
      xdg = lib.mkOption { type = lib.types.attrs; default = { }; };
      assertions = lib.mkOption { type = lib.types.listOf lib.types.unspecified; default = [ ]; };
    };
  };

  evalWith = backend: extra:
    (lib.evalModules {
      specialArgs = { inherit pkgs; };
      modules = [
        nixdeployModule
        backendAdapters.${backend}
        platformStub
        { nixdeploy.backend = backend; }
        extra
      ];
    }).config;

  nixosOut = evalWith "nixos" receiverFixture;
  smOut = evalWith "system-manager" receiverFixture;
  darwinOut = evalWith "nix-darwin" receiverFixture;
  homeOut = evalWith "home-manager" homeReceiverFixture;
  wrongHomeIdentityOut = evalWith "home-manager" (lib.recursiveUpdate homeReceiverFixture {
    nixdeploy.receiver.plane.identity = "bob";
  });
  homeWithoutGcRootOut = evalWith "home-manager" (lib.recursiveUpdate homeReceiverFixture {
    home.activationGenerateGcRoot = false;
  });

  # The opaque evaluator above passes pkgs through specialArgs deliberately, because it tests
  # the adapter in isolation. A normal homeManagerConfiguration does not: Home Manager supplies
  # pkgs later through config._module.args. This real constructor is therefore a separate
  # regression boundary for top-level module shapes that accidentally force pkgs while option
  # names are still being collected (which manifests as an infinite recursion through config).
  realHomeManagerWith = extra: home-manager.lib.homeManagerConfiguration {
    inherit pkgs;
    modules = [
      nixdeployModule
      backendAdapters.home-manager
      {
        home = {
          username = "alice";
          homeDirectory = "/home/alice";
          stateVersion = "25.05";
        };
        nixdeploy.backend = "home-manager";
      }
      extra
    ];
  };
  realHomeManager = realHomeManagerWith homeReceiverFixture;
  realHomeManagerPublisher = realHomeManagerWith publisherFixture;
  publisherNixosOut = evalWith "nixos" publisherFixture;
  publisherSmOut = evalWith "system-manager" publisherFixture;
  publisherDarwinOut = evalWith "nix-darwin" publisherFixture;
  fullPublisherOut = evalWith "nixos" {
    nixdeploy.publisher = {
      enable = true;
      targetsFile = "/nix/store/00000000000000000000000000000000-targets.json";
      revision = "fedcba9876543210";
      signingKeyFile = "/run/secrets/nixdeploy/signing-key";
    };
  };
  disabledOut = evalWith "nixos" { };

  svcOf = out: out.systemd.services.${unitName};
  timerOf = out: out.systemd.timers.${unitName};
  daemonOf = out: out.launchd.daemons.${unitName}.serviceConfig;
  homeSvcOf = out: out.systemd.user.services.${unitName}.Service;
  homeTimerOf = out: out.systemd.user.timers.${unitName}.Timer;
  homeAgentOf = out: out.launchd.agents.${unitName};
  publisherSvcOf = out: out.systemd.services.${publisherUnitName};
  publisherTimerOf = out: out.systemd.timers.${publisherUnitName};
  publisherExecStartOf = out: (publisherSvcOf out).serviceConfig.ExecStart;

  # The rendered config file's own `text` attribute -- the exact string that becomes the file,
  # read at EVAL time. Deliberately not `builtins.readFile` on the built path: that would be
  # import-from-derivation, which would make these checks build the file (and, transitively,
  # decide things about the store) to answer a question the expression already knows the
  # answer to.
  #
  # Context discarded for the same reason `contains` above discards it, and it is the same
  # refusal with a different builtin's name on it: this text names every adapter script and the
  # `nix` the receiver runs, so it carries context, and `builtins.fromJSON` will not parse a
  # string that refers to a store path. Nothing here builds the file; it only reads what the
  # expression already says.
  configJsonOf = out:
    builtins.fromJSON (builtins.unsafeDiscardStringContext out.nixdeploy.receiver.configFile.text);
  nixosConfig = configJsonOf nixosOut;
  homeConfig = configJsonOf homeOut;

  execStartOf = out: (svcOf out).serviceConfig.ExecStart;

  # ---- the real NixOS evaluator, for the one backend whose option surface actually exists
  #      in this evaluation ---------------------------------------------------------------
  bareStubs = {
    boot.loader.grub.enable = false;
    fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
    system.stateVersion = "25.05";
  };

  realNixos = (import (nixpkgs + "/nixos/lib/eval-config.nix") {
    inherit system;
    modules = [
      nixdeployModule
      backendAdapters.nixos
      bareStubs
      { nixdeploy.backend = "nixos"; }
      receiverFixture
    ];
  }).config;

  realServiceText = realNixos.systemd.units."${unitName}.service".text;
  realTimerText = realNixos.systemd.units."${unitName}.timer".text;

  realNixosPublisher = (import (nixpkgs + "/nixos/lib/eval-config.nix") {
    inherit system;
    modules = [
      nixdeployModule
      backendAdapters.nixos
      bareStubs
      { nixdeploy.backend = "nixos"; }
      publisherFixture
    ];
  }).config;

  realPublisherServiceText =
    realNixosPublisher.systemd.units."${publisherUnitName}.service".text;
  realPublisherTimerText =
    realNixosPublisher.systemd.units."${publisherUnitName}.timer".text;
in
[
  # =========================================================================================
  # The service itself
  # =========================================================================================
  # `receive` is asserted explicitly because it is the difference between a working unit and
  # one that exits 64 on every tick forever: `src/main.rs` refuses to default to a subcommand,
  # deliberately, since a bare `nixdeploy` meaning `receive` would be a footgun on the
  # publisher -- and "unknown subcommand" is a failure a timer repeats silently.
  (check "emission/nixos/runs-the-receiver-binary-against-the-rendered-config"
    (contains "/bin/nixdeploy" (execStartOf nixosOut)
      && contains "receive" (execStartOf nixosOut)
      && contains "-config" (execStartOf nixosOut)
      && contains (toString nixosOut.nixdeploy.receiver.configFile) (execStartOf nixosOut)
      && nixosOut.nixdeploy.receiver.job.argv ==
      [
        "${nixosOut.nixdeploy.receiver.package}/bin/nixdeploy"
        "receive"
        "-config"
        nixosOut.nixdeploy.receiver.configPath
      ])
    "expected ExecStart to invoke `nixdeploy receive -config <rendered config>` -- the subcommand is required, and a missing one is EX_USAGE on every tick")

  # `configPath` defaults to the rendered file's store path precisely so a receiver works with
  # nothing placed outside the store. If that default ever silently became /etc/..., the
  # receiver's first tick would fail on a file no activation had written yet.
  (check "emission/nixos/config-path-defaults-to-the-rendered-file-in-the-store"
    (lib.hasPrefix "/nix/store/" nixosOut.nixdeploy.receiver.configPath
      && nixosOut.nixdeploy.receiver.configPath == toString nixosOut.nixdeploy.receiver.configFile)
    "expected receiver.configPath to default to receiver.configFile's own store path")

  # An operator who does point configPath elsewhere must actually get that path in the unit --
  # otherwise the option is decorative and the receiver reads a file nobody configured.
  (check "emission/nixos/an-overridden-config-path-reaches-the-unit"
    (contains "/etc/nixdeploy/config.json"
      (execStartOf (evalWith "nixos" (lib.recursiveUpdate receiverFixture {
        nixdeploy.receiver.configPath = "/etc/nixdeploy/config.json";
      }))))
    "expected an overridden receiver.configPath to be what the unit passes to -config")

  (check "emission/nixos/service-is-a-oneshot-that-never-retries-on-its-own"
    ((svcOf nixosOut).serviceConfig.Type == "oneshot"
      && (svcOf nixosOut).serviceConfig.Restart == "no"
      && (svcOf nixosOut).serviceConfig.TimeoutStartSec == "infinity")
    "expected a oneshot with Restart=no (the timer is the only retry) and no finite start timeout (a SIGTERM mid-switch is worse than a slow run)")

  # The receiver is also the caller of switch-to-configuration. If its own new unit differs,
  # the default restart-if-changed policy would terminate that caller during its activation,
  # before it can re-read the current path, run the health gate, or roll back. Both systemd
  # backends must carry the option, and the real NixOS unit must render the directive consumed
  # by switch-to-configuration rather than merely retaining an inert module attribute.
  (check "emission/systemd/receiver-is-not-restarted-by-its-own-activation"
    ((svcOf nixosOut).restartIfChanged == false
      && (svcOf smOut).restartIfChanged == false
      && contains "X-RestartIfChanged=false" realServiceText)
    "expected both systemd receivers to survive a switch that changes their own unit, with X-RestartIfChanged=false in the rendered NixOS service")

  # `Outcome::exit_code` uses 0..4, one per outcome, and only 4 (`failed`) is an error. Both
  # systemd backends must say so, or the steady state of a converged fleet -- `alreadyCurrent`
  # every tick, exit 1 -- leaves every receiver unit in `failed` forever. 4 and 64 (EX_USAGE)
  # must NOT be listed: those are the two answers that genuinely are failures.
  (check "emission/systemd/non-failure-outcome-exit-codes-are-not-unit-failures"
    (
      let
        listed = svc: lib.splitString " " (svc.serviceConfig.SuccessExitStatus or "");
      in
      listed (svcOf nixosOut) == [ "1" "2" "3" ]
      && listed (svcOf smOut) == [ "1" "2" "3" ]
    )
    "expected SuccessExitStatus to cover alreadyCurrent(1), reimaged(2) and refused(3) but not failed(4) or EX_USAGE(64) -- see src/outcome.rs's exit_code")

  # Do not narrow the backend's PATH: activation scripts legitimately use the baseline tools
  # NixOS/system-manager place there. HOME is different: UID 0 is required for activation,
  # but application state must not fall into the privileged account's home. The systemd pair
  # therefore gets exact service-owned state/cache/runtime directories and only HOME-related
  # environment entries. nixdeploy's own argv remains absolute, so PATH is not load-bearing.
  (check "emission/systemd/uses-service-owned-state-instead-of-the-privileged-home"
    (
      let
        correct = out:
          (svcOf out).serviceConfig.StateDirectory == "nixdeploy"
          && (svcOf out).serviceConfig.CacheDirectory == "nixdeploy"
          && (svcOf out).serviceConfig.RuntimeDirectory == "nixdeploy"
          && (svcOf out).serviceConfig.Environment == [
            "HOME=/var/lib/nixdeploy"
            "XDG_CACHE_HOME=/var/cache/nixdeploy"
          ];
      in
      correct nixosOut
      && correct smOut
      && !((daemonOf darwinOut) ? EnvironmentVariables)
      && lib.hasPrefix "/nix/store/" (builtins.head nixosOut.nixdeploy.receiver.job.argv)
    )
    "expected both systemd receivers to use /var/lib, /var/cache and /run service directories, without replacing the backend PATH")

  # A Home Manager plane is not a fourth spelling for a privileged system switch. The
  # scheduler belongs to the declared user, and its mutable receiver state follows that
  # user's XDG directories. On Linux this is a user service/timer; on Darwin it is a
  # background user LaunchAgent with an adapter-owned runner that creates the same paths.
  (check "emission/home-manager/schedules-only-in-the-declared-user-manager"
    (if pkgs.stdenv.hostPlatform.isDarwin then
      homeOut.systemd == { }
      && (homeAgentOf homeOut).domain == "user"
      && (homeAgentOf homeOut).enable == true
      && (homeAgentOf homeOut).config.StartInterval == intervalSeconds
      && (homeAgentOf homeOut).config.RunAtLoad == true
    else
      homeOut.launchd == { }
      && homeOut.systemd.user.enable == true
      && (homeSvcOf homeOut).Type == "oneshot"
      && (homeSvcOf homeOut).Restart == "no"
      && (homeSvcOf homeOut).TimeoutStartSec == "infinity"
      && (homeTimerOf homeOut).OnUnitActiveSec == "${toString intervalSeconds}s"
      && homeOut.systemd.user.timers.${unitName}.Install.WantedBy == [ "timers.target" ])
    "expected Home Manager to emit an unprivileged user receiver, never a system service or launch daemon")

  (check "emission/home-manager/in-flight-receiver-survives-its-own-unit-update"
    (if pkgs.stdenv.hostPlatform.isDarwin then true else
      homeOut.systemd.user.services.${unitName}.Unit.X-SwitchMethod == "keep-old")
    "expected Home Manager's sd-switch activation to keep the running receiver alive while installing its replacement unit; stopping it before current-home advances creates an endless partial-activation loop")

  (check "emission/home-manager/real-constructor-does-not-recurse"
    (builtins.seq realHomeManager.activationPackage true
      && realHomeManager.config.nixdeploy.backend == "home-manager"
      && realHomeManager.config.nixdeploy.receiver.enable
      && builtins.hasAttr unitName realHomeManager.config.systemd.user.services
      && builtins.hasAttr unitName realHomeManager.config.systemd.user.timers)
    "expected the exported modules to evaluate and emit the receiver under a standard homeManagerConfiguration without passing pkgs through extraSpecialArgs")

  (check "emission/home-manager/real-constructor-refuses-publisher"
    (!(builtins.tryEval realHomeManagerPublisher.activationPackage).success)
    "expected Home Manager's real assertion gate to refuse publisher authority in a user plane")

  (check "emission/home-manager/uses-the-user-s-home-and-service-owned-xdg-state"
    (if pkgs.stdenv.hostPlatform.isDarwin then
      let agent = (homeAgentOf homeOut).config;
      in
      agent.EnvironmentVariables == {
        HOME = "/home/alice";
        USER = "alice";
        XDG_STATE_HOME = "/home/alice/.local/state";
        XDG_CACHE_HOME = "/home/alice/.cache";
      }
      && agent.StandardOutPath == "/home/alice/Library/Logs/${unitName}.log"
      && agent.StandardErrorPath == "/home/alice/Library/Logs/${unitName}.log"
    else
      let service = homeSvcOf homeOut;
      in
      service.StateDirectory == "nixdeploy"
      && service.CacheDirectory == "nixdeploy"
      && service.RuntimeDirectory == "nixdeploy"
      && service.Environment == [
        "HOME=/home/alice"
        "USER=alice"
        "XDG_STATE_HOME=/home/alice/.local/state"
        "XDG_CACHE_HOME=/home/alice/.cache"
      ])
    "expected Home Manager receiver state under the declared user's XDG state/cache/runtime roots, with the actual home and user exported to activation")

  # A service ALSO pulled in by a target would run at boot outside the timer's accounting, and
  # OnUnitActiveSec measures from the unit's last activation -- so that stray run would shift
  # every subsequent tick without anyone changing `interval`.
  (check "emission/nixos/only-the-timer-starts-the-service"
    (!((svcOf nixosOut) ? wantedBy)
      && (timerOf nixosOut).wantedBy == [ "timers.target" ])
    "expected the service to have no wantedBy of its own and the timer to be the thing pulled into timers.target")

  (check "emission/nixos/timer-fires-on-the-configured-interval-in-seconds"
    ((timerOf nixosOut).timerConfig.OnUnitActiveSec == "${toString intervalSeconds}s"
      && (timerOf nixosOut).timerConfig.OnBootSec == "1min")
    "expected OnUnitActiveSec to be receiver.interval in seconds, and OnBootSec to be the post-boot settle margin rather than a second copy of the interval")

  # The negative that makes every positive above mean something: with the receiver off, the
  # module must add nothing at all to this machine -- no unit, no timer, no nix.conf setting.
  (check "emission/nixos/emits-nothing-when-the-receiver-is-disabled"
    (disabledOut.systemd == { }
      && disabledOut.launchd == { }
      && disabledOut.nix == { }
      && disabledOut.users == { })
    "expected a machine with receiver.enable = false to get no systemd unit, no launchd daemon and no nix settings")

  # =========================================================================================
  # The rendered config file -- the seam with src/receive.rs
  # =========================================================================================

  # `activationAdapter` carries five verbs; `src/receive.rs`'s `ReceiverConfig` knows three of
  # them. This is the check that keeps the other two out of the JSON: `schedule` and
  # `nixSettings` are functions, and `builtins.toJSON` on a function is a hard error, so a
  # module that serialized the submodule wholesale would not fail subtly -- but a module that
  # added a SIXTH string-valued verb would, silently, hand the Rust half a schema it never
  # agreed to.
  #
  # An exact key list, not a subset test, and that cuts both ways deliberately: `ReceiverConfig`
  # also declares `reimage`, `bootRoleReconcile` and `metrics` with serde defaults, and `receiverFixture`
  # below sets neither, so this is also the check that proves the negative half of
  # `receiverConfig`'s own comment in modules/default.nix -- an unconfigured receiver's config
  # carries neither key at all. The positive half (that setting them reaches the file) is
  # proved further down, next to the config-file checks above it.
  (check "emission/config-file/carries-exactly-the-keys-this-module-renders"
    (builtins.attrNames nixosConfig == [ "activation" "healthGate" "manifest" "maxInplaceDeltaBytes" "nixBinary" "plane" "stateDirectory" ]
      && builtins.attrNames nixosConfig.activation == [ "activate" "currentPath" "rollback" ]
      && builtins.attrNames nixosConfig.manifest == [ "publicKey" "url" ]
      && nixosConfig.plane == { name = "nixos"; backend = "nixos"; })
    "expected the rendered config to include the exact selected plane and only the three command-valued activation verbs")

  (check "emission/config-file/transcribes-the-options-verbatim"
    (nixosConfig.manifest.url == manifestUrl
      && nixosConfig.manifest.publicKey == manifestKey
      && nixosConfig.maxInplaceDeltaBytes == ceilingBytes
      && nixosConfig.healthGate == healthGate
      && nixosConfig.stateDirectory == "/var/lib/nixdeploy")
    "expected manifest, ceiling, health gate, and service-owned state directory to appear in the config exactly as configured")

  # `null` here is not "unset by accident": src/receive.rs reads maxInplaceDeltaBytes as an
  # Option<u64>, and `null` is how "no ceiling" -- a deliberate answer, per the option's own
  # description -- crosses the seam.
  (check "emission/config-file/renders-an-absent-ceiling-as-null-rather-than-omitting-it"
    ((configJsonOf (evalWith "nixos" (lib.recursiveUpdate receiverFixture {
      nixdeploy.receiver.maxInplaceDeltaBytes = null;
    }))).maxInplaceDeltaBytes == null)
    "expected an unset ceiling to render as JSON null")

  # The receiver's own built-in fallback for this is a bare `nix` off PATH, which is right for
  # a hand-written config and wrong for a unit whose environment this repo deliberately keeps
  # to coreutils. If this ever stopped being absolute, the receiver would fail at the delta
  # stage on every tick, on a machine where `nix` is installed and working.
  (check "emission/config-file/pins-an-absolute-nix-binary"
    (lib.hasPrefix "/nix/store/" nixosConfig.nixBinary
      && lib.hasSuffix "/bin/nix" nixosConfig.nixBinary)
    "expected nixBinary to be an absolute store path ending in /bin/nix, not a bare PATH lookup")

  # The config must name the ADAPTER's commands, not a placeholder -- and the three must be
  # three different scripts, since a rollback that is secretly the activate command would
  # re-apply the failing closure the health gate just rejected.
  (check "emission/config-file/names-this-backend-s-own-adapter-commands"
    (nixosConfig.activation.activate == nixosOut.nixdeploy.receiver.activation.activate
      && lib.hasPrefix "/nix/store/" nixosConfig.activation.activate
      && lib.hasPrefix "/nix/store/" nixosConfig.activation.currentPath
      && nixosConfig.activation.rollback != null
      && lib.hasPrefix "/nix/store/" nixosConfig.activation.rollback
      && nixosConfig.activation.activate != nixosConfig.activation.currentPath
      && nixosConfig.activation.activate != nixosConfig.activation.rollback)
    "expected all three command verbs to be distinct absolute store paths contributed by the nixos adapter")

  (check "emission/home-manager/config-carries-the-explicit-user-plane-and-real-commands"
    (homeConfig.plane == {
      name = "home-manager";
      backend = "home-manager";
      identity = "alice";
    }
    && lib.hasPrefix "/nix/store/" homeConfig.activation.activate
    && lib.hasPrefix "/nix/store/" homeConfig.activation.currentPath
    && lib.hasPrefix "/nix/store/" homeConfig.activation.rollback
    && homeConfig.nixBinary == "${pkgs.nix}/bin/nix"
    && homeConfig.stateDirectory == "/home/alice/.local/state/nixdeploy"
    && contains "home-manager" homeConfig.activation.activate
    && homeConfig.activation.activate != homeConfig.activation.currentPath
    && homeConfig.activation.activate != homeConfig.activation.rollback)
    "expected the Home Manager config to pin identity alice and use independently generated switch, current-home and rollback commands")

  (check "emission/home-manager/rejects-an-identity-different-from-home-username"
    (lib.any
      (assertion: !assertion.assertion && contains "identity" assertion.message)
      wrongHomeIdentityOut.assertions
    && lib.all (assertion: assertion.assertion) homeOut.assertions)
    "expected a mismatched signed-plane identity to fail an adapter assertion, while identity = home.username satisfies every receiver assertion")

  (check "emission/home-manager/requires-the-post-activation-current-home-root"
    (lib.any
      (assertion: !assertion.assertion && contains "activationGenerateGcRoot" assertion.message)
      homeWithoutGcRootOut.assertions)
    "expected disabling Home Manager's current-home GC root to fail because profile registration alone advances before activation has completed")

  # Same option surface, different backend, different commands: the seam is per-machine, not
  # a fleet-wide constant that happens to be rendered three times.
  (check "emission/config-file/differs-per-backend-because-the-adapter-does"
    (nixosConfig.activation.activate != (configJsonOf smOut).activation.activate
      && nixosConfig.activation.activate != (configJsonOf darwinOut).activation.activate
      && nixosConfig.activation.activate != homeConfig.activation.activate
      && (configJsonOf smOut).activation.activate != (configJsonOf darwinOut).activation.activate
      && (configJsonOf smOut).activation.activate != homeConfig.activation.activate
      && (configJsonOf darwinOut).activation.activate != homeConfig.activation.activate)
    "expected each backend's adapter to contribute its own activate command; two backends sharing one means an adapter guard is not firing")

  # Optional receiver actuators and metrics -- proved in both directions. The exact-key check
  # above proves the negative; these prove the
  # positive, that setting them reaches the file verbatim, and the narrower negative that one
  # sink configured does not render the other as an explicit null.
  (check "emission/config-file/renders-actuators-and-metrics-when-configured"
    (
      let
        json = configJsonOf (evalWith "nixos" (lib.recursiveUpdate receiverFixture {
          nixdeploy.receiver = {
            reimage = reimageRequest;
            bootRoleReconcile = bootReconcileRequest;
            metrics = { textfile = metricsTextfile; pushUrl = metricsPushUrl; };
          };
        }));
      in
      json.reimage == reimageRequest
      && json.bootRoleReconcile == bootReconcileRequest
      && json.metrics.textfile == metricsTextfile
      && json.metrics.pushUrl == metricsPushUrl
      && builtins.attrNames json ==
        [ "activation" "bootRoleReconcile" "healthGate" "manifest" "maxInplaceDeltaBytes" "metrics" "nixBinary" "plane" "reimage" "stateDirectory" ]
      && builtins.attrNames json.metrics == [ "pushUrl" "textfile" ]
    )
    "expected receiver actuators and metrics sinks to reach the rendered config verbatim")

  (check "emission/config-file/omits-optional-actuators-and-metrics-when-unset"
    (!(nixosConfig ? reimage)
      && !(nixosConfig ? bootRoleReconcile)
      && !(nixosConfig ? metrics)
      && builtins.attrNames nixosConfig == [ "activation" "healthGate" "manifest" "maxInplaceDeltaBytes" "nixBinary" "plane" "stateDirectory" ])
    "expected an unconfigured receiver's rendered config to omit both actuator keys and metrics")

  (check "emission/config-file/a-single-metrics-sink-does-not-render-the-other-as-null"
    (
      let
        json = configJsonOf (evalWith "nixos" (lib.recursiveUpdate receiverFixture {
          nixdeploy.receiver.metrics.textfile = metricsTextfile;
        }));
      in
      json.metrics.textfile == metricsTextfile
      && builtins.attrNames json.metrics == [ "textfile" ]
      && !(json ? reimage)
      && !(json ? bootRoleReconcile)
    )
    "expected a receiver with only metrics.textfile set to omit metrics.pushUrl from the rendered object rather than writing it as null, and to still omit reimage entirely")

  (check "emission/system-manager/allows-boot-role-reconciliation"
    (lib.all (assertion: assertion.assertion)
      (evalWith "system-manager" (lib.recursiveUpdate receiverFixture {
        nixdeploy.receiver.bootRoleReconcile = bootReconcileRequest;
      })).assertions)
    "expected the host-level system-manager plane to be allowed to own boot reconciliation")

  (check "emission/home-manager/refuses-boot-role-reconciliation"
    (lib.any
      (assertion: !assertion.assertion && contains "bootRoleReconcile" assertion.message)
      (evalWith "home-manager" (lib.recursiveUpdate homeReceiverFixture {
        nixdeploy.receiver.bootRoleReconcile = bootReconcileRequest;
      })).assertions)
    "expected a user plane to be refused boot authority")

  # =========================================================================================
  # The memory knobs -- receiver.httpConnections / receiver.downloadBufferSize
  # =========================================================================================
  (check "emission/nixos/memory-knobs-reach-this-machine-s-nix-settings"
    (
      let
        out = evalWith "nixos" (lib.recursiveUpdate receiverFixture {
          nixdeploy.receiver = { inherit httpConnections downloadBufferSize; };
        });
      in
      out.nix.settings.http-connections == httpConnections
      && out.nix.settings.download-buffer-size == downloadBufferSize
    )
    "expected httpConnections and downloadBufferSize to land in nix.settings under nix.conf's own key names -- they bound the daemon's fetch, which is the memory peak the whole small-machine argument is about")

  # `null` means "leave the system default alone", which is not the same as writing an empty
  # value into nix.conf.
  (check "emission/nixos/unset-memory-knobs-do-not-touch-nix-settings-at-all"
    (nixosOut.nix.settings == { })
    "expected both knobs left null to produce no nix.settings entries whatsoever")

  (check "emission/nix-darwin/memory-knobs-reach-nix-settings-there-too"
    (
      let
        out = evalWith "nix-darwin" (lib.recursiveUpdate receiverFixture {
          nixdeploy.receiver = { inherit httpConnections downloadBufferSize; };
        });
      in
      out.nix.settings.http-connections == httpConnections
      && out.nix.settings.download-buffer-size == downloadBufferSize
    )
    "expected a Mac's own nix.conf to take these too -- nix-darwin owns it exactly as NixOS does")

  # The system-manager case is the reason this verb is per-backend at all. That backend manages
  # a slice of a foreign distro whose /etc/nix/nix.conf belongs to that distro's own Nix
  # installation, so it must FAIL rather than accept a ceiling it cannot enforce -- a
  # silently-ignored ceiling reads as protection to whoever set it.
  # Forced through `systemd`, not through `nix`, and that is the point rather than an
  # accident: this adapter forwards ONLY `systemd`, so `nix` is a tree nothing on this backend
  # ever reads -- if the refusal were only reachable there, it would be a refusal nobody ever
  # received. `modules/adapters/apply.nix` emits every tree in its list on every call for
  # exactly this reason.
  (check "emission/system-manager/refuses-memory-knobs-it-cannot-apply"
    (throwsOnForce
      (builtins.attrNames (evalWith "system-manager" (lib.recursiveUpdate receiverFixture {
        nixdeploy.receiver = { inherit httpConnections; };
      })).systemd)
    && throwsOnForce (builtins.attrNames (evalWith "system-manager" (lib.recursiveUpdate receiverFixture {
      nixdeploy.receiver = { inherit downloadBufferSize; };
    })).systemd))
    "expected setting either memory knob under backend = \"system-manager\" to fail the evaluation, naming the file to edit instead")

  (check "emission/system-manager/leaves-nix-settings-completely-alone-when-neither-knob-is-set"
    (smOut.nix == { })
    "expected a system-manager receiver with both knobs unset to evaluate cleanly and write nothing into nix settings")

  (check "emission/home-manager/refuses-daemon-memory-knobs-it-cannot-own"
    (
      let
        forcedTree = out:
          builtins.attrNames
            (if pkgs.stdenv.hostPlatform.isDarwin then out.launchd else out.systemd);
      in
      throwsOnForce (forcedTree (evalWith "home-manager" (lib.recursiveUpdate homeReceiverFixture {
        nixdeploy.receiver = { inherit httpConnections; };
      })))
      && throwsOnForce (forcedTree (evalWith "home-manager" (lib.recursiveUpdate homeReceiverFixture {
        nixdeploy.receiver = { inherit downloadBufferSize; };
      })))
    )
    "expected a user plane to refuse host-daemon substitution settings instead of pretending its user nix.conf controls nix-daemon")

  # =========================================================================================
  # system-manager and nix-darwin, structurally
  # =========================================================================================
  (check "emission/system-manager/produces-the-same-systemd-service-timer-pair"
    (contains "/bin/nixdeploy" (execStartOf smOut)
      && (svcOf smOut).serviceConfig.Type == "oneshot"
      && (timerOf smOut).timerConfig.OnUnitActiveSec == "${toString intervalSeconds}s"
      && smOut.launchd == { })
    "expected system-manager to schedule through systemd (the foreign distro already runs it) and to emit no launchd daemon")

  (check "emission/nix-darwin/produces-a-launchd-daemon-and-no-systemd-unit"
    (darwinOut.systemd == { }
      && (daemonOf darwinOut).StartInterval == intervalSeconds
      && (daemonOf darwinOut).RunAtLoad == true)
    "expected nix-darwin to schedule through launchd, with StartInterval in plain seconds (launchd has no duration grammar to translate into) and RunAtLoad so a freshly-booted Mac checks promptly")

  # `argv` is a LIST across the whole adapter contract precisely so this one can pass it
  # straight into ProgramArguments -- launchd has no shell and no quoting rules, so a
  # pre-joined command line would have to be re-split here by something guessing at word
  # boundaries.
  (check "emission/nix-darwin/passes-a-real-argument-vector-not-a-command-line"
    (builtins.isList (daemonOf darwinOut).ProgramArguments
      && builtins.length (daemonOf darwinOut).ProgramArguments == 4
      && lib.hasSuffix "/bin/nixdeploy" (builtins.elemAt (daemonOf darwinOut).ProgramArguments 0)
      && builtins.elemAt (daemonOf darwinOut).ProgramArguments 1 == "receive"
      && builtins.elemAt (daemonOf darwinOut).ProgramArguments 2 == "-config"
      && builtins.elemAt (daemonOf darwinOut).ProgramArguments 3 == darwinOut.nixdeploy.receiver.configPath)
    "expected ProgramArguments to be exactly [ <binary> \"receive\" \"-config\" <config path> ] as four separate argv entries")

  # nix-darwin derives both the daemon's Label and the generated plist's FILENAME from the
  # attribute name. Setting one of the two here would mean guessing how the other is derived,
  # and a label that disagrees with the file launchd was asked to load is a daemon launchd
  # simply does not run.
  (check "emission/nix-darwin/leaves-the-launchd-Label-to-nix-darwin"
    (!((daemonOf darwinOut) ? Label))
    "expected the adapter not to set Label -- nix-darwin names the plist from it, and a label/filename mismatch is a daemon that never runs")

  (check "emission/nix-darwin/sends-the-receiver-s-output-somewhere-readable"
    ((daemonOf darwinOut).StandardOutPath == "/var/log/${unitName}.log"
      && (daemonOf darwinOut).StandardErrorPath == "/var/log/${unitName}.log")
    "expected StandardOutPath/StandardErrorPath to be set: launchd has no journal, and without them the receiver's one JSON line per run -- the entire record of what it decided -- goes to /dev/null")

  # =========================================================================================
  # The real NixOS evaluator: not "an option was set", but "a unit file was generated"
  # =========================================================================================
  (check "emission/real-nixos/generates-an-actual-service-unit-file"
    (contains "Type=oneshot" realServiceText
      && contains "ExecStart=" realServiceText
      && contains "/bin/nixdeploy" realServiceText
      && contains "TimeoutStartSec=infinity" realServiceText
      && contains "SuccessExitStatus=1 2 3" realServiceText
      && contains "receive -config /nix/store/" realServiceText)
    "expected NixOS's own unit generator to render nixdeploy-receiver.service invoking `nixdeploy receive -config <store path>` as its ExecStart")

  (check "emission/real-nixos/generates-an-actual-timer-unit-file"
    (contains "OnUnitActiveSec=${toString intervalSeconds}s" realTimerText
      && contains "OnBootSec=1min" realTimerText
      && contains "WantedBy=timers.target" realTimerText)
    "expected NixOS's own unit generator to render nixdeploy-receiver.timer carrying the configured interval and installed into timers.target")

  (check "emission/real-nixos/writes-the-memory-knobs-into-the-machine-s-nix-conf"
    (
      let
        out = (import (nixpkgs + "/nixos/lib/eval-config.nix") {
          inherit system;
          modules = [
            nixdeployModule
            backendAdapters.nixos
            bareStubs
            { nixdeploy.backend = "nixos"; }
            (lib.recursiveUpdate receiverFixture {
              nixdeploy.receiver = { inherit httpConnections downloadBufferSize; };
            })
          ];
        }).config;
      in
      out.nix.settings.http-connections == httpConnections
      && out.nix.settings.download-buffer-size == downloadBufferSize
    )
    "expected the knobs to survive into a REAL NixOS nix.settings, where the switch that applies them also restarts nix-daemon for them")

  # =========================================================================================
  # The publisher: a real scheduled, unprivileged invocation of the same binary
  # =========================================================================================
  (check "emission/publisher/systemd-backends-produce-a-service-and-timer"
    (
      let
        correct = out:
          (publisherSvcOf out).serviceConfig.Type == "oneshot"
          && (publisherTimerOf out).wantedBy == [ "timers.target" ]
          && (publisherTimerOf out).timerConfig.OnUnitActiveSec == "777s"
          && !((publisherSvcOf out) ? wantedBy);
      in
      correct publisherNixosOut && correct publisherSmOut
    )
    "expected NixOS and system-manager to schedule nixdeploy-publisher through a timer, with no second boot-time service start")

  (check "emission/publisher/job-passes-v3-input-revision-and-independent-selectors"
    (
      let argv = publisherExecStartOf publisherNixosOut;
      in
      contains "publish" argv
      && contains "--targets" argv
      && contains "/nix/store/00000000000000000000000000000000-targets.json" argv
      && contains "--revision" argv
      && contains "0123456789abcdef" argv
      && contains "--base-manifest" argv
      && contains "--host" argv
      && contains "host-a" argv
      && contains "host-b" argv
      && contains "--plane" argv
      && contains "nixos" argv
      && contains "home-manager" argv
      && contains "--out" argv
      && contains "/var/lib/nixdeploy-publisher/manifest.json" argv
    )
    "expected the timer to call the v3 publisher with repeatable host and plane axes; the Rust publisher owns their intersection semantics")

  (check "emission/publisher/full-replacement-omits-base-and-selectors"
    (
      let argv = publisherExecStartOf fullPublisherOut;
      in
      contains "publish" argv
      && contains "--targets" argv
      && !contains "--base-manifest" argv
      && !contains "--host" argv
      && !contains "--plane" argv
    )
    "expected a bootstrap/full publication to be an explicit replacement, with no merge base or partial selectors")

  # The source path may be visible in the unit, but only as LoadCredential input. ExecStart
  # receives systemd's private credential path and the key contents reach neither place.
  (check "emission/publisher/signing-key-is-a-private-systemd-credential"
    ((publisherSvcOf publisherNixosOut).serviceConfig.LoadCredential == [
      "signing-key:/run/secrets/nixdeploy/signing-key"
    ]
    && contains "--signing-key-file" (publisherExecStartOf publisherNixosOut)
    && contains "%d/signing-key" (publisherExecStartOf publisherNixosOut)
    && !contains "/run/secrets/nixdeploy/signing-key" (publisherExecStartOf publisherNixosOut))
    "expected systemd to broker the signing key into the unprivileged unit instead of granting that transient UID access to the source secret")

  (check "emission/publisher/is-unprivileged-and-owns-only-service-directories"
    (
      let service = (publisherSvcOf publisherNixosOut).serviceConfig;
      in
      service.User == "nixdeploy-publisher"
      && service.Group == "nixdeploy-publisher"
      && !(service ? DynamicUser)
      && service.StateDirectory == "nixdeploy-publisher"
      && service.CacheDirectory == "nixdeploy-publisher"
      && service.RuntimeDirectory == "nixdeploy-publisher"
      && service.WorkingDirectory == "/var/lib/nixdeploy-publisher"
      && service.Environment == [
        "HOME=/var/lib/nixdeploy-publisher"
        "XDG_CACHE_HOME=/var/cache/nixdeploy-publisher"
      ]
      && publisherNixosOut.users.users.nixdeploy-publisher.isSystemUser == true
      && publisherNixosOut.users.users.nixdeploy-publisher.home == "/var/lib/nixdeploy-publisher"
      && (publisherSvcOf publisherSmOut).serviceConfig.DynamicUser == true
      && !((publisherSvcOf publisherSmOut).serviceConfig ? User)
    )
    "expected NixOS publication to use a dedicated non-root account, system-manager to use DynamicUser, and both to own only service state, cache, runtime and HOME paths")

  (check "emission/publisher/hardens-the-static-file-writer"
    (
      let service = (publisherSvcOf publisherNixosOut).serviceConfig;
      in
      service.NoNewPrivileges == true
      && service.PrivateDevices == true
      && service.PrivateNetwork == true
      && service.PrivateTmp == true
      && service.ProtectHome == true
      && service.ProtectSystem == "strict"
      && service.CapabilityBoundingSet == ""
    )
    "expected the publisher, unlike the privileged activation receiver, to have no network, home, devices, capabilities or writable system tree")

  (check "emission/publisher/nix-darwin-refuses-an-unsafe-root-fallback"
    (throwsOnForce (builtins.attrNames publisherDarwinOut.launchd))
    "expected nix-darwin publisher.enable to fail until launchd has a real unprivileged credential-bearing scheduler")

  (check "emission/real-nixos/generates-publisher-service-and-timer-units"
    (contains "User=nixdeploy-publisher" realPublisherServiceText
      && contains "LoadCredential=signing-key:/run/secrets/nixdeploy/signing-key" realPublisherServiceText
      && contains "ExecStart=" realPublisherServiceText
      && contains "publish" realPublisherServiceText
      && contains "--targets" realPublisherServiceText
      && contains "PrivateNetwork=true" realPublisherServiceText
      && contains "ProtectHome=true" realPublisherServiceText
      && contains "OnUnitActiveSec=777s" realPublisherTimerText
      && contains "WantedBy=timers.target" realPublisherTimerText)
    "expected NixOS's own generators to accept and render the publisher service/timer, not merely the opaque test stub")
]
