# checks/emission.nix
#
# Proves the module actually PRODUCES something. `checks/assertions.nix` next door proves the
# option surface refuses configurations it should refuse -- an entirely different question,
# and one a module that emits nothing at all passes perfectly. This file is the other half:
# under each of the three backends, enabling `nixdeploy.receiver` must yield a scheduled unit
# that runs the receiver binary against a config file whose contents are exactly the schema
# `src/receive.rs` deserializes.
#
# TWO EVALUATORS, ON PURPOSE
#
# `evalWith` below uses a bare `lib.evalModules` plus `platformStub`, which declares the
# option TREES the three backends write into (`systemd`, `launchd`, `nix`) as opaque attrsets
# and nothing more. That is not a shortcut around a real evaluator, it is the only way to ask
# the question at all for two of the three: nix-darwin and system-manager are deliberately not
# flake inputs here (see flake.nix's own header on why), so their option surfaces do not exist
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
{ pkgs, lib, nixpkgs, system, nixdeployModule, backendAdapters }:

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

  # Declares the three option trees the adapters in this repo write into, as opaque attrsets.
  # Nothing here validates their contents -- that is the point: this stub must not accidentally
  # become a second, worse implementation of systemd's or launchd's own option surface, or the
  # checks would start proving things about the stub.
  platformStub = { ... }: {
    options = {
      systemd = lib.mkOption { type = lib.types.attrs; default = { }; };
      launchd = lib.mkOption { type = lib.types.attrs; default = { }; };
      nix = lib.mkOption { type = lib.types.attrs; default = { }; };
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
  disabledOut = evalWith "nixos" { };

  svcOf = out: out.systemd.services.${unitName};
  timerOf = out: out.systemd.timers.${unitName};
  daemonOf = out: out.launchd.daemons.${unitName}.serviceConfig;

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
    (disabledOut.systemd == { } && disabledOut.launchd == { } && disabledOut.nix == { })
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
  # also declares `reimage` and `metrics`, both `#[serde(default)]`, and `receiverFixture`
  # below sets neither, so this is also the check that proves the negative half of
  # `receiverConfig`'s own comment in modules/default.nix -- an unconfigured receiver's config
  # carries neither key at all. The positive half (that setting them reaches the file) is
  # proved further down, next to the config-file checks above it.
  (check "emission/config-file/carries-exactly-the-keys-this-module-renders"
    (builtins.attrNames nixosConfig == [ "activation" "healthGate" "manifest" "maxInplaceDeltaBytes" "nixBinary" ]
      && builtins.attrNames nixosConfig.activation == [ "activate" "currentPath" "rollback" ]
      && builtins.attrNames nixosConfig.manifest == [ "publicKey" "url" ])
    "expected the rendered config to be exactly the five fields this module derives, with activation carrying only the three COMMAND verbs -- schedule and nixSettings are eval-time functions and must never reach this file")

  (check "emission/config-file/transcribes-the-options-verbatim"
    (nixosConfig.manifest.url == manifestUrl
      && nixosConfig.manifest.publicKey == manifestKey
      && nixosConfig.maxInplaceDeltaBytes == ceilingBytes
      && nixosConfig.healthGate == healthGate)
    "expected manifest.url, manifest.publicKey, maxInplaceDeltaBytes and healthGate to appear in the config exactly as configured")

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

  # Same option surface, different backend, different commands: the seam is per-machine, not
  # a fleet-wide constant that happens to be rendered three times.
  (check "emission/config-file/differs-per-backend-because-the-adapter-does"
    (nixosConfig.activation.activate != (configJsonOf smOut).activation.activate
      && nixosConfig.activation.activate != (configJsonOf darwinOut).activation.activate
      && (configJsonOf smOut).activation.activate != (configJsonOf darwinOut).activation.activate)
    "expected each backend's adapter to contribute its own activate command; two backends sharing one means an adapter guard is not firing")

  # `reimage` and `metrics` -- proved in both directions. The exact-key check above already
  # proves the negative (neither key exists when neither option is set); these prove the
  # positive, that setting them reaches the file verbatim, and the narrower negative that one
  # sink configured does not render the other as an explicit null.
  (check "emission/config-file/renders-reimage-and-metrics-when-configured"
    (
      let
        json = configJsonOf (evalWith "nixos" (lib.recursiveUpdate receiverFixture {
          nixdeploy.receiver = {
            reimage = reimageCommand;
            metrics = { textfile = metricsTextfile; pushUrl = metricsPushUrl; };
          };
        }));
      in
      json.reimage == reimageCommand
      && json.metrics.textfile == metricsTextfile
      && json.metrics.pushUrl == metricsPushUrl
      && builtins.attrNames json ==
        [ "activation" "healthGate" "manifest" "maxInplaceDeltaBytes" "metrics" "nixBinary" "reimage" ]
      && builtins.attrNames json.metrics == [ "pushUrl" "textfile" ]
    )
    "expected receiver.reimage and receiver.metrics.{textfile,pushUrl} to reach the rendered config verbatim, alongside the five fields every receiver already carries")

  (check "emission/config-file/omits-reimage-and-metrics-entirely-when-unset"
    (!(nixosConfig ? reimage)
      && !(nixosConfig ? metrics)
      && builtins.attrNames nixosConfig == [ "activation" "healthGate" "manifest" "maxInplaceDeltaBytes" "nixBinary" ])
    "expected an unconfigured receiver's rendered config to carry neither key at all -- not \"reimage\":null and not \"metrics\":{} -- since ReceiverConfig::metrics is a bare struct that a JSON null cannot deserialize into")

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
    )
    "expected a receiver with only metrics.textfile set to omit metrics.pushUrl from the rendered object rather than writing it as null, and to still omit reimage entirely")

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
]
