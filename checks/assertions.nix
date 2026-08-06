# checks/assertions.nix
#
# Eval-time checks for modules/default.nix's OPTION SURFACE: what it refuses, and what it
# derives. Each evaluates a real configuration through NixOS's own eval-config.nix and either
# asks whether forcing `system.build.toplevel` fails (the `config.assertions` half -- assertions
# are only enforced when something forces them, which NixOS's top-level derivation does
# unconditionally, and a bare read of `config.assertions` does not) or reads a derived option
# back out and compares it.
#
# What this file deliberately does NOT test is whether the module EMITS anything -- a module
# that produced no unit, no config file and no package at all would pass every check below.
# That question is `checks/emission.nix`'s, and it is asked with the real adapters; the
# fixtures here use a stub adapter on purpose, so that a failure in this file always means an
# assertion or a derivation changed, never that an adapter did.
#
# Real NixOS eval-config.nix, not a hand-rolled `lib.evalModules` stub, and not a real
# system-manager or nix-darwin evaluator either -- neither of those is a flake input here (see
# flake.nix's own header on why), and this repo's whole point is that its option surface
# neither needs nor reads anything backend-specific. Evaluating `nixdeploy.backend =
# "nix-darwin"` (or "system-manager") through a plain Linux NixOS eval-config is therefore not
# a shortcut: it is itself part of the proof that the module never reaches for a primitive
# only one of the four targets actually has. If it did, THIS evaluator -- which has none of
# the other three -- would be exactly the one to catch it.
{ pkgs, lib, nixpkgs, system, nixdeployModule }:

let
  bareStubs = {
    boot.loader.grub.enable = false;
    fileSystems."/" = { device = "none"; fsType = "tmpfs"; };
    system.stateVersion = "25.05";
  };

  evalNixdeploy = extraConfig:
    (import (nixpkgs + "/nixos/lib/eval-config.nix") {
      inherit system;
      modules = [ nixdeployModule extraConfig bareStubs ];
    }).config;

  buildFails = extraConfig:
    !(builtins.tryEval (builtins.seq (evalNixdeploy extraConfig).system.build.toplevel true)).success;

  # Reads one derived option back out of an evaluation, without forcing the config block at
  # all -- every fixture using this leaves the receiver disabled, so `buildLocality` is being
  # asked about as a pure derivation from `class`, `localBuildClasses` and `backend`.
  localityOf = extraConfig: (evalNixdeploy extraConfig).nixdeploy.receiver.buildLocality;

  check = name: ok: detail: { inherit name ok detail; };

  # A complete, valid backend adapter -- all five verbs `activationAdapter` declares, because
  # `modules/default.nix`'s config block forces every one of them the moment a receiver is
  # enabled, and a fixture missing one would fail here for a reason that has nothing to do
  # with the assertion under test.
  #
  # The two configuration verbs are deliberately STUBS returning empty fragments. That is not
  # laziness: it keeps these fixtures from dragging in a real adapter's scripts, its package
  # and its rendered config file, none of which any assertion below reads -- and it means a
  # failure here is unambiguously about the option surface. `checks/emission.nix` composes the
  # real adapters and holds them to what they actually produce.
  validActivation = {
    activate = "true";
    currentPath = "echo /nix/store/00000000000000000000000000000000-example";
    schedule = _: { };
    nixSettings = _: { };
  };

  # A complete, valid manifest pointer. `example.org` and a syntactically plausible but
  # entirely fake age public key -- never a value that could resolve to anything real.
  validManifest = {
    url = "https://cache.example.org/manifest.json";
    publicKey = "age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
  };

  # The smallest receiver that should evaluate cleanly: remote build (what a machine in no
  # class gets), a manifest to fetch it from, an adapter, and no ceiling -- so nothing in
  # `config.assertions` has grounds to object. Every "and one thing about it is wrong" fixture
  # below starts from this and changes exactly one field, the same shape nixwatch's own
  # checks/assertions.nix uses for the same reason: it isolates which single change is
  # responsible for a failure.
  validReceiver = {
    nixdeploy.backend = "nixos";
    nixdeploy.receiver.enable = true;
    nixdeploy.receiver.buildLocality = "remote";
    nixdeploy.receiver.manifest = validManifest;
    nixdeploy.receiver.activation = validActivation;
  };

  validHomeReceiver = lib.recursiveUpdate validReceiver {
    nixdeploy.backend = "home-manager";
    nixdeploy.receiver.plane.identity = "alice";
  };

  validPublisher = {
    nixdeploy.backend = "nixos";
    nixdeploy.publisher = {
      enable = true;
      targetsFile = "/nix/store/00000000000000000000000000000000-targets.json";
      revision = "0123456789abcdef";
      signingKeyFile = "/run/secrets/nixdeploy/signing-key";
    };
  };
in
[
  # --- the base fixture itself, listed first: every negative check below is "validReceiver
  #     with exactly one field changed for the worse", so if this one does not build clean,
  #     every "fails" check downstream is proving nothing. ------------------------------------
  (check "assertions/minimal-valid-receiver-evaluates-cleanly"
    (! buildFails validReceiver)
    "expected the minimal valid receiver fixture to build cleanly, but it did not")

  (check "assertions/home-manager-requires-an-explicit-plane-identity"
    (buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.backend = "home-manager";
    }))
    "expected an enabled Home Manager receiver with no identity to fail before it can select a signed target")

  (check "assertions/home-manager-with-an-identity-evaluates-cleanly"
    (! buildFails validHomeReceiver)
    "expected an enabled Home Manager receiver with an explicit identity to satisfy the backend-neutral plane assertions")

  (check "assertions/system-planes-forbid-a-user-identity"
    (buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.receiver.plane.identity = "alice";
    }))
    "expected a system receiver carrying a user identity to fail instead of silently selecting a different signed plane shape")

  # --- publisher: a full replacement is the bootstrap and requires no existing manifest. ---
  (check "assertions/minimal-valid-publisher-evaluates-cleanly"
    (! buildFails validPublisher)
    "expected a full-manifest publisher with absolute inputs and service-owned output to build cleanly")

  (check "assertions/publisher-refuses-an-empty-revision"
    (buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher.revision = "";
    }))
    "expected an empty publisher revision to fail before a timer could repeatedly publish untraceable output")

  # A partial update without a base silently deletes every target that was not selected; a
  # base on a full replacement is equally suspicious because no selection says what to merge.
  (check "assertions/partial-publisher-requires-a-base-manifest"
    (buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher.select.hosts = [ "host-a" ];
    }))
    "expected host or plane selection without baseManifest to fail rather than dropping unselected targets")

  (check "assertions/full-replacement-forbids-a-base-manifest"
    (buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher.baseManifest = "/var/lib/nixdeploy-publisher/manifest.json";
    }))
    "expected baseManifest with no selectors to fail rather than imply a merge whose selected set is undefined")

  (check "assertions/partial-publisher-with-a-base-evaluates-cleanly"
    (! buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher = {
        baseManifest = "/var/lib/nixdeploy-publisher/manifest.json";
        select.hosts = [ "host-a" "host-b" ];
        select.planes = [ "nixos" "home-manager" ];
      };
    }))
    "expected independent host and plane selectors plus a complete base manifest to build cleanly")

  (check "assertions/publisher-output-cannot-escape-its-state-directory"
    ((buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher.manifestOutput = "/srv/http/manifest.json";
    }))
    && (buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher = {
        baseManifest = "/srv/http/manifest.json";
        select.hosts = [ "host-a" ];
      };
    })))
    "expected the publisher to refuse reading or writing mutable publication state outside its own service directory")

  (check "assertions/publisher-input-is-absolute-and-secret-is-runtime-only"
    ((buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher.targetsFile = "targets.json";
    }))
    && (buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher.signingKeyFile = "/var/lib/nixdeploy/key";
    })))
    "expected a relative targetsFile, or a signing key outside runtime secret storage, to fail before scheduling")

  (check "assertions/publisher-plane-selectors-are-names-not-host-plane-pairs"
    (buildFails (lib.recursiveUpdate validPublisher {
      nixdeploy.publisher = {
        baseManifest = "/var/lib/nixdeploy-publisher/manifest.json";
        select.planes = [ "host-a/system" ];
      };
    }))
    "expected HOST/PLANE syntax to be refused: host and plane are independent selector axes")

  # --- backend "nix-darwin" + buildLocality "remote": a darwin closure cannot be produced on
  #     a Linux builder, so a Mac cannot receive one -- it can only ever build its own. The
  #     fixture states "remote" EXPLICITLY, because the default already derives "local" for
  #     this backend (see the buildLocality derivation checks further down); what is under
  #     test here is that overriding that default is still refused, not merely defaulted away.
  (check "assertions/darwin-backend-with-remote-locality-fails-the-build"
    (buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.backend = "nix-darwin";
    }))
    "expected backend = \"nix-darwin\" with buildLocality explicitly set to \"remote\" to fail the build, but it succeeded")

  (check "assertions/darwin-backend-with-local-locality-builds-fine"
    (
      ! buildFails {
        nixdeploy.backend = "nix-darwin";
        nixdeploy.receiver.enable = true;
        nixdeploy.receiver.buildLocality = "local";
        nixdeploy.receiver.activation = validActivation;
        # Deliberately no manifest here: buildLocality = "local" is exactly the case where the
        # "must be told where its manifest is" assertion's own short-circuit
        # (`buildLocality == "local" || manifest.url != ""`) never forces `manifest.url` at all.
      }
    )
    "a nix-darwin receiver with buildLocality already forced to \"local\" should never fail the build on its own")

  # --- a ceiling creates a possible reimage obligation. Require the provider recovery
  #     domain to be declared even though the current off-target registry has no caller and
  #     the live on-target request is wired separately through receiver.reimage. -------------
  (check "assertions/ceiling-set-without-provider-fails-the-build"
    (buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.receiver.maxInplaceDeltaBytes = 500 * 1024 * 1024;
    }))
    "expected a ceiling with no declared provider to fail the build, but it succeeded")

  (check "assertions/ceiling-set-with-provider-builds-fine"
    (
      ! buildFails (lib.recursiveUpdate validReceiver {
        nixdeploy.provider = "example-provider";
        nixdeploy.receiver.maxInplaceDeltaBytes = 500 * 1024 * 1024;
      })
    )
    "a ceiling paired with a declared provider should never fail the build on its own")

  # --- a receiver that does not build locally and was not told where its manifest is: nothing
  #     could ever tell it what it is supposed to become. --------------------------------------
  (check "assertions/remote-locality-without-manifest-url-fails-the-build"
    (buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.receiver.manifest.url = "";
    }))
    "expected buildLocality = \"remote\" with an empty manifest.url to fail the build, but it succeeded")

  # --- the module must load under all four backend values on its own, with the receiver and
  #     publisher both off -- merely stating a backend must never touch a primitive only one
  #     of the four targets actually have. Each of these composes ONLY nixdeployModule
  #     plus the bare NixOS baseline (see `bareStubs` above): if the option surface secretly
  #     reached for something backend-specific, it would surface here as an eval failure
  #     regardless of which real platform this ran on. ------------------------------------------
  (check "assertions/loads-under-nixos-backend"
    (! buildFails { nixdeploy.backend = "nixos"; })
    "expected backend = \"nixos\" alone (receiver and publisher both off) to build cleanly, but it did not")

  (check "assertions/loads-under-system-manager-backend"
    (! buildFails { nixdeploy.backend = "system-manager"; })
    "expected backend = \"system-manager\" alone (receiver and publisher both off) to build cleanly, but it did not")

  (check "assertions/loads-under-home-manager-backend"
    (! buildFails { nixdeploy.backend = "home-manager"; })
    "expected backend = \"home-manager\" alone (receiver and publisher both off) to build cleanly, but it did not")

  (check "assertions/loads-under-nix-darwin-backend"
    (! buildFails { nixdeploy.backend = "nix-darwin"; })
    "expected backend = \"nix-darwin\" alone (receiver and publisher both off) to build cleanly, but it did not")

  # --- `class` -> `buildLocality`. This is the one thing nixdeploy derives from a machine's
  #     capability tier, and `localBuildClasses` is the whole translation: this repo has no
  #     vocabulary of its own for tiers, so the operator names which of theirs mean "capable"
  #     and nothing else about a tier is ever interpreted here. ---------------------------------
  (check "assertions/class-listed-in-localBuildClasses-derives-a-local-build"
    (localityOf
      {
        nixdeploy.backend = "nixos";
        nixdeploy.class = "workstation";
        nixdeploy.localBuildClasses = [ "workstation" "builder" ];
      } == "local")
    "expected a machine whose class appears in localBuildClasses to default to buildLocality = \"local\"")

  (check "assertions/class-outside-localBuildClasses-stays-remote"
    (localityOf
      {
        nixdeploy.backend = "nixos";
        nixdeploy.class = "appliance";
        nixdeploy.localBuildClasses = [ "workstation" "builder" ];
      } == "remote")
    "expected a machine whose class is NOT in localBuildClasses to default to buildLocality = \"remote\"")

  # The empty list is the shipped default, and it must mean "nobody builds locally" rather
  # than "the list is unset, so guess" -- guessing high here means telling a small machine to
  # do the expensive thing this whole repo exists to keep off it.
  (check "assertions/no-class-and-no-class-list-stays-remote"
    (localityOf { nixdeploy.backend = "nixos"; } == "remote"
      && localityOf { nixdeploy.backend = "nixos"; nixdeploy.class = "workstation"; } == "remote")
    "expected an undeclared class, and a declared class with an empty localBuildClasses, to both default to \"remote\"")

  # The derivation is a DEFAULT, not a rule: a capable machine that must nonetheless receive
  # (a builder being rebuilt, a machine temporarily unable to evaluate) states it directly.
  (check "assertions/an-explicit-buildLocality-overrides-the-class-derivation"
    (localityOf
      {
        nixdeploy.backend = "nixos";
        nixdeploy.class = "workstation";
        nixdeploy.localBuildClasses = [ "workstation" ];
        nixdeploy.receiver.buildLocality = "remote";
      } == "remote")
    "expected an explicitly stated buildLocality to win over whatever the class list would have derived")

  # Darwin is decided by the backend, not by the class list, because it is not a policy
  # question at all: a darwin closure cannot be produced on a Linux builder, so a Mac always
  # builds its own -- including on an estate that never populates localBuildClasses.
  (check "assertions/darwin-derives-local-regardless-of-the-class-list"
    (localityOf { nixdeploy.backend = "nix-darwin"; } == "local"
      && localityOf
      {
        nixdeploy.backend = "nix-darwin";
        nixdeploy.class = "appliance";
        nixdeploy.localBuildClasses = [ "workstation" ];
      } == "local")
    "expected backend = \"nix-darwin\" to derive buildLocality = \"local\" on its own, with or without a class list")
]
