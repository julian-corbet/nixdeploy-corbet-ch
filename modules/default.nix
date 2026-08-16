# nixdeploy -- the option surface.
#
# Loadable under FOUR backends (NixOS, system-manager, Home Manager, nix-darwin) from one
# file. It
# therefore names no backend-specific primitive anywhere -- not in its options, not in its
# config block, not even as a string. Everything a platform needs to do differently is
# reached through an ADAPTER (see `activation` and `provisioning` below), never through a
# conditional in this file.
#
# Taken seriously, that rule has a consequence worth stating up front, because it explains why
# this file emits only assertions despite being the file that defines a receiver: a module can
# only contribute configuration under option names it can write down, and the names in
# question -- `systemd`, `launchd`, `nix` -- are exactly the ones this file may not know. So
# this file assembles everything a receiver needs (`receiver.job`, `receiver.configFile`, the
# ceilings) and the backend adapter splices it in. `modules/adapters/apply.nix` states the
# module-system property that makes any other arrangement impossible, not merely inadvisable.
#
# The split this module maintains, and the reason it exists:
#
#   * The PUBLISHER knows what every machine SHOULD run. It can evaluate and build, so it
#     is the only component allowed to.
#   * The RECEIVER knows what its own machine CAN safely become. It cannot evaluate
#     anything, and it does not need to -- it is told a store path and sizes it against
#     its own store from narinfo metadata alone.
#
# Nothing else decides anything. In particular there is no controller: a component that
# must be correct, reachable and holding accurate state before any machine can converge is
# a component whose failure means nothing converges at all.
{ config, lib, pkgs, ... }:

let
  inherit (lib) mkOption mkEnableOption types mkIf literalExpression optionalAttrs filterAttrs;
  cfg = config.nixdeploy;
  manifestSchema = import ../lib/manifest.nix { inherit lib; };

  # Host FACTS are read defensively BY NAME from whatever namespace the operator uses to
  # declare them, never taken as a flake input (see flake.nix). `or null` throughout: this
  # module must stay loadable on a host that declares no facts at all, in which case the
  # operator states the two values it actually needs directly on nixdeploy.
  factClass = config.nixhost.stance.class or null;
  factProvider = config.nixhost.stance.provider or null;

  # The one name the receiver's scheduled unit is known by, on every backend. Fixed rather
  # than an option: an operator who needs two receivers on one machine needs two manifests
  # and two ceilings as well, which is a second nixdeploy, not a second unit name -- and a
  # name that varies per machine is a name nobody can grep the fleet's journals for.
  receiverName = "nixdeploy-receiver";

  activationAdapter = types.submodule {
    options = {
      activate = mkOption {
        type = types.str;
        example = literalExpression ''"''${pkgs.myBackend}/bin/activate"'';
        description = ''
          Command that makes this machine BECOME the closure it is given. Receives the
          store path as its single argument. Must be idempotent, and must exit non-zero
          if and only if the machine did not end up running that closure.

          That "if and only if" is the whole contract, and it is the one most
          implementations get wrong: a backend whose switch command returns non-zero
          because some unrelated unit failed, while the configuration applied perfectly,
          will report a healthy activation as a failure -- and one that returns zero
          without having applied anything reports the opposite. Where the underlying tool
          conflates these, the adapter is responsible for disambiguating (typically by
          re-reading `currentPath` afterwards) rather than passing the ambiguity up.
        '';
      };

      currentPath = mkOption {
        type = types.str;
        description = ''
          Command printing the store path this machine is running RIGHT NOW, on stdout,
          with no trailing content. This is the ground truth for every convergence
          question, and it is deliberately asked of the machine rather than remembered by
          anyone else: a record of what a machine "should" be running is wrong exactly
          when it matters most.
        '';
      };

      rollback = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          Command returning this machine to its previous closure, used when the health
          gate fails after an otherwise successful activation. `null` means this backend
          cannot roll back, which is a legitimate answer -- the receiver then reports a
          failed activation it could not undo, rather than pretending it did.
        '';
      };

      # ---------------------------------------------------------------------------------
      # The two verbs below are EVAL-TIME FUNCTIONS returning configuration fragments, not
      # command lines the receiver ever runs. They are deliberately never written into the
      # receiver's JSON config: `receiverConfig` further down names the three command
      # fields above one at a time rather than serializing this submodule wholesale, both
      # because `builtins.toJSON` on a function is a hard error and because a config file
      # that silently grew a fourth key would be a schema the Rust half never agreed to
      # (`src/receive.rs`'s `ReceiverConfig` is the other side of that agreement).
      #
      # They live in the SAME registry as the three above because they are keyed by the
      # same fact -- `nixdeploy.backend` -- and answered by the same file. Splitting them
      # into a second per-backend registry would mean two places to forget to update when
      # a fourth backend arrives, keyed identically, for no separation anyone benefits
      # from.
      # ---------------------------------------------------------------------------------

      schedule = mkOption {
        type = types.functionTo types.attrs;
        example = literalExpression ''
          { name, description, argv, intervalSeconds }: {
            systemd.services.''${name} = { /* ... */ };
            systemd.timers.''${name} = { /* ... */ };
          }
        '';
        description = ''
          How this machine runs a command repeatedly, on its own, forever -- the mechanism
          behind `interval`, and the reason a receiver keeps converging with no publisher
          in the loop at all.

          This is an adapter verb for the same reason `activate` is: "run this every N
          seconds" has no cross-platform spelling. NixOS and system-manager have systemd;
          nix-darwin has launchd, whose vocabulary shares not one word with systemd's. A
          conditional in this file choosing between them would put a backend-specific
          primitive in the one file that must stay loadable under all four, which is the
          exact failure the adapter registry exists to prevent.

          Called once with `receiver.job` -- `{ name, description, argv, intervalSeconds }`
          -- and must return a configuration fragment, in whatever option vocabulary this
          backend's own module system speaks, that runs `argv` at least every
          `intervalSeconds`.

          `argv` is a LIST of already-absolute strings, not a command line, so that no
          adapter has to parse one: launchd wants a real argument vector
          (`ProgramArguments`) and systemd wants a string it re-splits with its own quoting
          rules, and a single pre-joined string would force one of the two to guess where
          the word boundaries were.

          The fragment also owns the run's privileges and environment, because those are
          per-platform too, and the adapter is the only thing that knows what its own
          backend's activation tooling needs. `modules/adapters/nixos.nix` is the reference
          implementation and states, at length, which privileges the receiver genuinely
          needs and which hardening directives would break the one thing it exists to do.

          This option is CALLED by the backend adapter, not by this file, and the reason is
          a hard property of the module system rather than a preference -- see
          `modules/adapters/apply.nix`, which is where the call happens and where the
          "infinite recursion encountered ... while evaluating the module argument `config'"
          it avoids is spelled out in full.
        '';
      };

      nixSettings = mkOption {
        type = types.functionTo types.attrs;
        example = literalExpression ''
          { httpConnections, downloadBufferSize }: {
            nix.settings = { /* ... */ };
          }
        '';
        description = ''
          How this machine's Nix substitution limits get set. Called with
          `{ httpConnections, downloadBufferSize }` -- each `null` when the operator left it
          alone -- and must return a configuration fragment that applies them.

          Per-backend because OWNERSHIP of nix.conf is per-backend, not because the setting
          names differ. On NixOS and nix-darwin a machine's Nix configuration is part of the
          very closure this module is already replacing. On system-manager it is not: that
          backend manages a slice of a foreign distro, whose Nix installation configured
          itself before nixdeploy existed and will be reconfigured by its own installer
          again later. Home Manager likewise cannot control a multi-user daemon through the
          receiving user's nix.conf, so its adapter refuses either knob and names the
          host-level place that owns it.

          Both knobs are memory ceilings on machines that have none to spare (see their own
          descriptions). An adapter with nowhere to put them must therefore FAIL rather than
          accept and drop them: a silently-ignored ceiling is worse than no ceiling at all,
          because it reads as protection to whoever set it.

          Called by the backend adapter, for the same module-system reason `schedule` is --
          see `modules/adapters/apply.nix`.
        '';
      };
    };
  };

  reimageRequest = types.submodule {
    options = {
      command = mkOption {
        type = types.str;
        description = ''
          Provider replacement command. It receives three positional arguments: the exact
          boot role (`primary` or `nixrescue`), the signed nixboot artifact store path for
          that role, and the signed provider image reference. Private Infra supplies the
          command and every provider-specific value.
        '';
      };
      role = mkOption {
        type = types.enum manifestSchema.bootRoles;
        description = ''
          Exact signed boot role this request may materialise. The current on-target,
          over-ceiling actuator implements `primary`; selecting `nixrescue` is accepted as
          typed policy but returns a typed reimage failure until its actuator is implemented.
        '';
      };
    };
  };

  bootRoleReconcileRequest = types.submodule {
    options = {
      command = mkOption {
        type = types.str;
        description = ''
          Idempotent local boot actuator. It receives one positional argument: the exact
          immutable artifact store path selected from the verified manifest. Role selection
          remains typed configuration and is cross-checked before this command runs.
        '';
      };
      role = mkOption {
        type = types.enum manifestSchema.bootRoles;
        description = "Signed boot role this machine reconciles.";
      };
    };
  };

  # The receiver's on-disk config, as an attrset, ready for `builtins.toJSON`. Every field
  # here exists on `src/receive.rs`'s `ReceiverConfig` under exactly this camelCase name; the
  # three `activation` fields are named ONE AT A TIME rather than inheriting the submodule
  # wholesale, because that submodule also carries two function-valued verbs that must never
  # reach JSON (see `activationAdapter` above).
  #
  # `reimage`, `bootRoleReconcile` and `metrics` are the optional fields `ReceiverConfig`
  # declares `#[serde(default)]`, and all are rendered here from their receiver options
  # directly, not from `publisher.provisioning` (that registry stays the PUBLISHER's, read by
  # nothing at all; see its own doc above). Both are OMITTED, not rendered as `null`, when
  # unset, and for a reason that is not merely stylistic: `metrics` on `ReceiverConfig` is a
  # bare `MetricsConfig` struct, not an `Option<MetricsConfig>`, so a literal `"metrics":null`
  # is a shape its deserializer refuses outright -- `#[serde(default)]` only ever fires on a
  # MISSING key, never on a present null one. The two Option fields tolerate either spelling
  # (`Option<ReimageConfig>` accepts both null and absence), but omitting it too keeps this module to
  # one rule instead of two. Within `metrics` itself, an unset sink is likewise omitted rather
  # than written as `null`, so a receiver with one sink configured says only that.
  # `checks/emission.nix` proves both directions: a receiver with neither configured renders
  # neither key, and one with either configured renders it verbatim.
  receiverConfig = {
    manifest = {
      inherit (cfg.receiver.manifest) url publicKey;
    };
    plane = {
      inherit (cfg.receiver.plane) name;
      inherit (cfg) backend;
    }
    // optionalAttrs (cfg.receiver.plane.identity != null) {
      inherit (cfg.receiver.plane) identity;
    };
    stateDirectory = cfg.receiver.stateDirectory;
    maxInplaceDeltaBytes = cfg.receiver.maxInplaceDeltaBytes;
    activation = {
      inherit (cfg.receiver.activation) activate currentPath rollback;
    };
    healthGate = cfg.receiver.healthGate;
    nixBinary = cfg.receiver.nixBinary;
  }
  // optionalAttrs (cfg.receiver.reimage != null) {
    inherit (cfg.receiver) reimage;
  }
  // optionalAttrs (cfg.receiver.bootRoleReconcile != null) {
    inherit (cfg.receiver) bootRoleReconcile;
  }
  // optionalAttrs (cfg.receiver.metrics.textfile != null || cfg.receiver.metrics.pushUrl != null) {
    metrics = filterAttrs (_: v: v != null) {
      inherit (cfg.receiver.metrics) textfile pushUrl;
    };
  };
in
{
  imports = [ ./publisher.nix ];

  options.nixdeploy = {
    backend = mkOption {
      type = types.enum manifestSchema.backends;
      example = "nixos";
      description = ''
        Which flake output composed this module. Required, with no default, and stated by
        the caller rather than detected: this module cannot ask which backend loaded it
        without reading a backend-specific primitive, which is precisely what would make
        it fail to load under the other three.
      '';
    };

    class = mkOption {
      type = types.nullOr types.str;
      default = factClass;
      defaultText = literalExpression "config.nixhost.stance.class or null";
      description = ''
        The capability tier of this machine, in the operator's own vocabulary. Read from
        the sibling fact namespace by default; state it here directly if facts are
        declared elsewhere or not at all.

        nixdeploy derives exactly one thing from it -- whether this machine builds its own
        closure or receives one -- and it does so through `localBuildClasses`, which is the
        operator's own list of which tier names mean "capable", not this repo's. The tier
        itself is a fact belonging to whoever declares it; deriving further policy from it
        here would make every consumer inherit this repo's opinion of what a tier means.
      '';
    };

    localBuildClasses = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "workstation" "builder" ];
      description = ''
        Which `class` values name machines that build their own closures. A machine whose
        `class` appears here defaults `receiver.buildLocality` to `"local"`; every other
        machine defaults to `"remote"`.

        Empty by default, and that emptiness is the point: `class` is "in the operator's own
        vocabulary", so this repo cannot know which of an operator's tier names means
        capable without inventing a vocabulary of its own and quietly imposing it. Naming
        the capable tiers once, fleet-wide, is the whole translation -- and it is a list
        rather than a predicate so that the derivation stays visible in
        `buildLocality`'s own default, where someone reading one machine's options can see
        why it came out the way it did.

        With this left empty, every non-darwin machine defaults to `remote`, which is the
        safe direction: a machine wrongly told to build locally is a machine asked to do the
        expensive thing this repo exists to keep off it.
      '';
    };

    provider = mkOption {
      type = types.nullOr types.str;
      default = factProvider;
      defaultText = literalExpression "config.nixhost.stance.provider or null";
      description = ''
        Where this machine runs, in the operator's own vocabulary. This is the key for the
        off-target provisioning registry; that registry currently has no caller. The live
        on-target route is wired explicitly through `receiver.reimage` instead.
      '';
    };

    receiver = {
      enable = mkEnableOption "the nixdeploy receiver on this machine";

      plane = {
        name = mkOption {
          type = types.enum manifestSchema.backends;
          readOnly = true;
          default = cfg.backend;
          defaultText = literalExpression "config.nixdeploy.backend";
          description = ''
            Canonical name of the one manifest plane this receiver instance activates.
            Schema version 3 defines the plane name to equal its backend, so this is a
            read-only derivation rather than a second spelling an operator could make
            disagree.
          '';
        };

        identity = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "alice";
          description = ''
            Identity owned by this plane. Required only for a home-manager plane and
            forbidden for system planes. It is copied into the receiver config so the
            signed target can be cross-checked before an activation command runs.
          '';
        };
      };

      package = mkOption {
        type = types.package;
        default = pkgs.callPackage ../package.nix { };
        defaultText = literalExpression "pkgs.callPackage ./package.nix { }";
        description = ''
          The receiver binary. Defaults to this repo's own `package.nix`, which is the one
          place that derivation is defined -- the flake's `packages.<system>.nixdeploy`
          output is the same expression, not a second copy of it.

          "Installed" here means reached at an absolute store path from the scheduled unit
          the adapter renders, which is what actually puts it in this machine's closure and
          under the same GC root as everything else in that closure. It is deliberately NOT
          added to any system-wide package set: whether an operator also wants this on their
          own `$PATH` is a convenience question this module has no business answering, and
          the option is exposed here precisely so they can answer it themselves.
        '';
      };

      buildLocality = mkOption {
        type = types.enum [ "remote" "local" ];
        default =
          if cfg.backend == "nix-darwin" then "local"
          else if cfg.class != null && builtins.elem cfg.class cfg.localBuildClasses then "local"
          else "remote";
        defaultText = literalExpression ''
          if backend == "nix-darwin" then "local"
          else if class != null && builtins.elem class localBuildClasses then "local"
          else "remote"
        '';
        description = ''
          Whether this machine's closure is built elsewhere and fetched (`remote`) or built
          here (`local`).

          `remote` is the fallback on purpose: a builder with a warm store and a populated
          cache exists precisely so that machines do not each repeat that work, and the
          machines least able to build are the ones most likely to be forgotten when
          choosing. `local` is correct for genuinely capable machines -- which is what
          `localBuildClasses` translates this machine's `class` into -- and unavoidable
          where a closure cannot be cross-built for this platform at all: a nix-darwin
          system cannot be produced on Linux, so a Mac always builds its own, which is why
          the darwin case is decided here rather than left to a class list an operator could
          forget to populate.

          Still settable per machine. The derivation above is a default, not a rule: a
          capable machine that must nonetheless receive (a builder being rebuilt, a machine
          temporarily unable to evaluate) states `remote` here and the class list is not
          consulted.
        '';
      };

      maxInplaceDeltaBytes = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        example = 500 * 1024 * 1024;
        description = ''
          The largest change, in bytes of NEW store paths this machine would have to fetch,
          that may be applied in place. Over this, the receiver refuses and the machine is
          reimaged instead.

          `null` means no ceiling -- correct for a machine large enough that activation
          size is not a survival question, and NOT a placeholder for "not yet tuned".

          The number is bytes-to-fetch, not closure size: an unchanged path already in the
          store costs nothing. It is measured here, against this machine's actual store,
          from narinfo metadata -- no download, no evaluation. A ceiling enforced anywhere
          else is enforced against a model of this store rather than the store.

          Choosing it: this bounds concurrent peak memory during fetch, decompression,
          store registration and the unit restarts that follow, on a machine that may have
          very little. When unsure, prefer a lower ceiling. Refusing costs a reimage;
          guessing high costs the machine.
        '';
      };

      httpConnections = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        example = 4;
        description = ''
          Substituter connection concurrency while fetching. Each in-flight connection
          carries its own decompression state, so on a small machine this is a memory knob
          wearing a throughput costume. `null` leaves the system default alone.

          Applied through this backend's `activation.nixSettings` adapter verb, because it
          lands in the machine's Nix configuration -- and which file that is, and who owns
          it, is exactly what differs between the four backends. It is set on the MACHINE
          rather than passed to the receiver on a command line because the fetch is not the
          receiver's: `activate` hands a store path to the backend's own switch tool, which
          asks the Nix daemon to substitute it, and a daemon does not take substitution
          limits from whoever asked.
        '';
      };

      downloadBufferSize = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        example = 64 * 1024 * 1024;
        description = ''
          Substituter download buffer. Same reasoning as `httpConnections`: a throughput
          setting that is really a memory ceiling on machines that have none to spare, and
          applied the same way, through `activation.nixSettings`, for the same reason.
        '';
      };

      nixBinary = mkOption {
        type = types.str;
        default = "${if (config.nix.package or null) != null then config.nix.package else pkgs.nix}/bin/nix";
        defaultText = literalExpression ''"''${if (config.nix.package or null) != null then config.nix.package else pkgs.nix}/bin/nix"'';
        description = ''
          The `nix` the receiver itself runs, for local store queries and for reading this
          machine's own substituter list out of `nix show-config` (`src/receive.rs`). An
          absolute store path, so the receiver's unit needs nothing on its `PATH` to find
          it -- the binary's own built-in fallback is a bare `nix`, which is right for a
          hand-written config and wrong for a scheduled unit whose environment this module
          deliberately keeps empty.

          Read defensively from `config.nix.package` (the same by-name convention this
          module uses for host facts) so a NixOS or nix-darwin machine gets the exact `nix`
          it already runs its daemon from. It falls back to `pkgs.nix` when the option is
          absent (system-manager) or explicitly `null` (Home Manager's "do not manage a Nix
          package" value). Point this at the host installation's own `nix` if its version
          and this `pkgs`'s differ enough to matter.
        '';
      };

      reimage = mkOption {
        type = types.nullOr reimageRequest;
        default = null;
        example = literalExpression ''{
          command = "''${pkgs.myProvisioner}/bin/reimage";
          role = "primary";
        }'';
        description = ''
          Guarded request asking this machine's provider to replace it wholesale with the
          exact signed boot-role artifact and image, run by the RECEIVER itself when it
          decides it needs one -- not the
          publisher-side `publisher.provisioning.<provider>.reimage` registry above, which
          today has no caller at all. The command receives role, artifact store path and
          image reference as separate arguments, matching `src/receive.rs`'s
          `route_over_ceiling`; the configured role is cross-checked against the signed
          manifest, and its exact signed artifact and image are selected before the command
          can run.

          Called when -- and only when -- a run's delta comes back over
          `maxInplaceDeltaBytes` AND the manifest names an image for this host; with no
          image named there is nothing to replace it with, and the run fails rather than
          inventing one. `null` is a complete answer for a machine whose operator has
          decided it is never replaced automatically: the receiver then refuses and stops,
          which is `Outcome::Refused` on the record, not silence.

          On many providers this call REPLACES THE MACHINE, which means it may kill the
          very process making it before that process can observe how the call ended. So
          `Outcome::Reimaged` claims exactly one thing -- that a replacement was requested
          and the provider accepted the request -- and never that the replacement LANDED;
          that claim belongs to a later run reporting `Converged` or `AlreadyCurrent` from
          the machine that comes back. See `route_over_ceiling`'s own doc comment for the
          full accounting of what each of the three ways this call can end does and does
          not prove.
        '';
      };

      bootRoleReconcile = mkOption {
        type = types.nullOr bootRoleReconcileRequest;
        default = null;
        example = literalExpression ''{
          command = "''${pkgs.nixrescue-reconcile}/bin/nixrescue-reconcile";
          role = "nixrescue";
        }'';
        description = ''
          Reconcile one exact signed boot role on every run where the target was already
          current when the receiver started. A run which changes the system target defers
          reconciliation until the next scheduled tick, because that activation may replace
          both this request and its command; the old process must not use the previous
          closure's actuator against the new closure's signed artifact. This is the receiver's
          self-correction hook: the command must be idempotent and must verify media and
          signatures before returning success. It never grants firmware ownership or enrolls
          Secure Boot keys.

          Only system planes may own boot artifacts. Leave this null for Home Manager and
          nix-darwin user planes.
        '';
      };

      metrics = {
        textfile = mkOption {
          type = types.nullOr types.path;
          default = null;
          example = "/var/lib/node_exporter/textfile_collector/nixdeploy.prom";
          description = ''
            A Prometheus textfile-collector path this run's outcome is written to,
            atomically, after every run -- converged, refused, failed, whichever it was.

            A failing write here never changes what the run itself decided: this is a
            reporting sink, not a gate, and letting it fail a convergence would make a
            monitoring system the most fragile dependency in a system built to have no
            single point of failure. The one metric written here on EVERY outcome,
            including failures, is the run's own timestamp -- because that is what makes
            staleness alertable: a machine that has stopped reporting is otherwise
            indistinguishable from one that simply has nothing to do.
          '';
        };

        pushUrl = mkOption {
          type = types.nullOr types.str;
          default = null;
          example = "https://pushgateway.example.org/metrics/job/nixdeploy";
          description = ''
            A URL the same exposition text is POSTed to, for machines a scraper cannot
            reach -- which is most of the machines this repo exists for, since a
            receiver's whole premise is that reachability is not something it can depend
            on.

            Same guarantee as `textfile`: a failing push never changes the run's outcome,
            and the run's timestamp is still emitted here on every outcome including
            failures, so an unreachable push endpoint shows up as staleness rather than as
            silence.
          '';
        };
      };

      stateDirectory = mkOption {
        type = types.str;
        default = "/var/lib/nixdeploy";
        description = ''
          Persistent, service-owned receiver state. This is rendered into the JSON config,
          unlike systemd's `STATE_DIRECTORY` environment variable, so every scheduler and
          manual invocation has the same explicit answer. System receivers default to the
          `StateDirectory=nixdeploy` path their adapters create; a user-plane adapter
          overrides it with that user's XDG state location.

          The receiver stores health-rejected immutable targets here, scoped by plane, so
          the directory must survive timer runs and must be writable by the scheduled
          receiver identity. It must be absolute; the Rust config validator refuses a
          relative value before touching machine state.
        '';
      };

      configFile = mkOption {
        type = types.path;
        readOnly = true;
        default = pkgs.writeText "nixdeploy-config.json" (builtins.toJSON receiverConfig);
        defaultText = literalExpression ''pkgs.writeText "nixdeploy-config.json" (builtins.toJSON <the options above>)'';
        description = ''
          The rendered receiver config, in the store. Read-only because it is a mechanical
          transcription of the options above and nothing else -- `src/receive.rs`'s own
          `ReceiverConfig` doc states that both halves are deliberately one schema, and an
          overridable rendering is how they would stop being one.

          Exposed so that an operator who sets `configPath` somewhere else can point their
          own copy mechanism at the same bytes, e.g.
          `environment.etc."nixdeploy/config.json".source = config.nixdeploy.receiver.configFile;`
        '';
      };

      configPath = mkOption {
        type = types.str;
        default = "${cfg.receiver.configFile}";
        defaultText = literalExpression "config.nixdeploy.receiver.configFile";
        description = ''
          Where the receiver reads its config from. Defaults to the rendered file's own
          store path, so nothing has to be written outside the store to make a receiver
          work.

          That default is not a shortcut around `/etc`, it is the only location all four
          backends own identically. NixOS owns `/etc` outright; nix-darwin owns a curated
          part of a macOS install that Apple also writes to; system-manager owns whichever
          slice of a foreign distro's `/etc` it was told to manage and nothing else; Home
          Manager owns user configuration, not the host's `/etc`. Picking
          `/etc/nixdeploy/config.json` here would have been an ownership claim three of the
          four backends cannot make, and it would have added a second thing that must
          already have been placed before the unit's first tick -- a receiver whose config
          arrives one activation later than its timer is a receiver that fails its first run
          for a reason nobody will connect to this option.

          Set it to a real path (and place `configFile` there yourself) when the config must
          be editable in place -- reading it out of the store means changing it is a
          rebuild, which is correct for a managed machine and inconvenient for a machine
          being debugged by hand.
        '';
      };

      job = mkOption {
        type = types.attrs;
        readOnly = true;
        default = {
          name = receiverName;
          description = "nixdeploy receiver: converge this machine's named plane to its signed target";
          # An argument VECTOR, absolute on both elements that are paths.
          #
          # `receive` is a required subcommand, not a nicety: the same binary also carries
          # `publish`, and `src/main.rs` deliberately refuses to default to either -- a bare
          # `nixdeploy` that quietly meant `receive` would be a footgun on the publisher,
          # which is the one machine that must never activate anything.
          #
          # `-config` is spelled the way `src/receive.rs`'s own `parse_args` spells it, and
          # the path is passed explicitly rather than left to that binary's compiled-in
          # default, so `configPath` stays the single answer to "which file is this receiver
          # reading" even when it happens to hold the default.
          argv = [ "${cfg.receiver.package}/bin/nixdeploy" "receive" "-config" cfg.receiver.configPath ];
          intervalSeconds = cfg.receiver.interval;
        };
        defaultText = literalExpression ''
          {
            name = "nixdeploy-receiver";
            description = "nixdeploy receiver: ...";
            argv = [ "''${receiver.package}/bin/nixdeploy" "receive" "-config" receiver.configPath ];
            intervalSeconds = receiver.interval;
          }
        '';
        description = ''
          The one job every backend must arrange to run, assembled once here so that all
          four arrange the SAME job: this is exactly the argument `activation.schedule` is
          called with.

          Read-only. It is a derivation from `package`, `configPath` and `interval` and
          nothing else -- an overridable version of it would be a second place a machine's
          receiver could be told to run something different from what its config file
          describes, which is the one disagreement nothing downstream could detect.

          `name` is fixed rather than an option: a machine needing two receivers needs two
          manifests and two ceilings as well, which is a second nixdeploy rather than a
          second unit name -- and a name that varied per machine is a name nobody could grep
          a fleet's journals for.
        '';
      };

      manifest = {
        url = mkOption {
          type = types.str;
          description = ''
            Where to fetch the manifest naming this machine's target closure.

            This should be reachable WITHOUT any overlay, VPN or mesh this deployment also
            deploys. Delivery that depends on the network it delivers cannot deliver the
            fix for a broken network, and a machine that has lost its overlay is exactly
            the machine that needs to converge.
          '';
        };

        publicKey = mkOption {
          type = types.str;
          description = ''
            Key the manifest signature is verified against. The manifest names store paths
            this machine will then run, so an unverified manifest is arbitrary code
            execution with extra steps. Verified before any path in it is fetched.
          '';
        };
      };

      interval = mkOption {
        type = types.ints.positive;
        default = 3600;
        description = ''
          How often this machine checks the manifest on its own, in SECONDS. This repo
          ships no push mechanism, so this interval is the whole delivery guarantee -- it
          is a floor that an operator's own out-of-band "check now" poke could improve on,
          and a poke that fails would cost nothing, because this interval still runs.

          Seconds, as a plain integer, rather than a systemd-flavoured duration string,
          because this value has to survive the trip through `activation.schedule` to a
          backend that may not have systemd at all: launchd's `StartInterval` is an integer
          number of seconds with no calendar vocabulary whatsoever. The alternative -- a
          duration parser written in Nix -- would be a second implementation of systemd's
          own grammar, and the day the two disagree about what "1h30" means, they disagree
          silently.
        '';
      };

      healthGate = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = ''
          Commands run after activation to decide whether it is keeping. All must exit
          zero. Any failure triggers `rollback` where the backend has one.

          A check that cannot RUN must be distinguished from a check that FAILED. An
          unreachable interpreter, a missing binary or a command not found is a broken
          probe, not an unhealthy machine, and treating the two alike converts a typo into
          an outage -- or, worse, into a silent rollback loop that reverts healthy work
          forever. The receiver classifies these separately; adapters and gate commands
          must not swallow the distinction by exiting zero on their own errors.
        '';
      };

      activation = mkOption {
        type = activationAdapter;
        description = ''
          Everything about this machine that depends on which Nix module system built it:
          the three command verbs the receiver runs (`activate`, `currentPath`, `rollback`)
          and the two configuration verbs this module calls at eval time (`schedule`,
          `nixSettings`). Contributed by whichever adapter implements this machine's
          backend, not hand-written per host: the point of an adapter registry is that the
          same verbs are implemented once per platform rather than once per machine.
        '';
      };
    };

  };

  # Assertions, and deliberately nothing else. Everything this module PRODUCES -- the
  # scheduled unit and the machine's Nix settings -- is spliced in by the backend adapter,
  # which calls `activation.schedule` and `activation.nixSettings` with the values assembled
  # above (`receiver.job`, `receiver.httpConnections`, `receiver.downloadBufferSize`). That is
  # not this file declining to do its job: a fragment whose OPTION NAMES come from reading
  # `config` cannot be spliced in by any module, and only a backend adapter can name those
  # trees statically. `modules/adapters/apply.nix` carries the full statement of that
  # constraint; a host therefore composes this file AND its backend's adapter, and that pair
  # is what `nixdeploy.receiver.enable = true` turns into a running receiver.
  config = mkIf cfg.receiver.enable {
    assertions = [
      {
        assertion = cfg.receiver.enable -> (if cfg.backend == "home-manager" then
            cfg.receiver.plane.identity != null && cfg.receiver.plane.identity != ""
          else
            cfg.receiver.plane.identity == null);
        message = ''
          nixdeploy: receiver.plane.identity is required only for home-manager and forbidden
          for every system plane.
        '';
      }
      {
        assertion = cfg.receiver.enable -> cfg.receiver.buildLocality == "local"
          || cfg.receiver.manifest.url != "";
        message = "nixdeploy: a receiver that does not build locally must be told where its manifest is.";
      }
      {
        assertion = cfg.backend == "nix-darwin" -> cfg.receiver.buildLocality == "local";
        message = ''
          nixdeploy: backend "nix-darwin" requires buildLocality = "local" -- a darwin
          system closure cannot be produced on a Linux builder, so a Mac cannot receive one.
        '';
      }
      {
        assertion = (cfg.receiver.enable && cfg.receiver.maxInplaceDeltaBytes != null)
          -> cfg.provider != null;
        message = ''
          nixdeploy: a ceiling is set but this machine declares no provider, so a refusal
          has no declared off-target recovery domain. Either declare the provider fact or
          drop the ceiling deliberately; `receiver.reimage` remains the separate live
          on-target route.
        '';
      }
      {
        assertion = cfg.receiver.reimage == null || cfg.backend == "nixos";
        message = "nixdeploy: receiver.reimage is valid only for the nixos plane.";
      }
      {
        assertion = cfg.receiver.bootRoleReconcile == null
          || builtins.elem cfg.backend [ "nixos" "system-manager" ];
        message = "nixdeploy: receiver.bootRoleReconcile is valid only for system planes.";
      }
    ];
  };
}
