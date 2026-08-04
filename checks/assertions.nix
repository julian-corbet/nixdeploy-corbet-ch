# checks/assertions.nix
#
# Eval-time checks for modules/default.nix: each evaluates a real configuration through
# NixOS's own eval-config.nix and asks whether forcing `system.build.toplevel` fails. Nothing
# here activates, boots, or runs anything -- these are entirely about `config.assertions`,
# which is only enforced when something forces it (NixOS's own top-level derivation does,
# unconditionally, as part of building `system.build.toplevel`; a bare read of
# `config.assertions` does not).
#
# Real NixOS eval-config.nix, not a hand-rolled `lib.evalModules` stub, and not a real
# system-manager or nix-darwin evaluator either -- neither of those is a flake input here (see
# flake.nix's own header on why), and this repo's whole point is that its option surface
# neither needs nor reads anything backend-specific. Evaluating `nixdeploy.backend =
# "nix-darwin"` (or "system-manager") through a plain Linux NixOS eval-config is therefore not
# a shortcut: it is itself part of the proof that the module never reaches for a primitive
# only one of the three targets actually has. If it did, THIS evaluator -- which has none of
# the other two -- would be exactly the one to catch it.
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

  check = name: ok: detail: { inherit name ok detail; };

  # A complete, valid activation adapter. Every fixture below that enables the receiver
  # includes this rather than leaning on the module's own laziness (nothing in
  # modules/default.nix's assertions actually reads `activation`, so omitting it would still
  # "pass") -- these checks are meant to exercise a config that looks like a real one, not one
  # that only happens to build because nothing forces the untested field.
  validActivation = {
    activate = "true";
    currentPath = "echo /nix/store/00000000000000000000000000000000-example";
  };

  # A complete, valid manifest pointer. `example.org` and a syntactically plausible but
  # entirely fake age public key -- never a value that could resolve to anything real.
  validManifest = {
    url = "https://cache.example.org/manifest.json";
    publicKey = "age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq";
  };

  # The smallest receiver that should evaluate cleanly: remote build (the documented
  # default), a manifest to fetch it from, an activation adapter, and no ceiling -- so nothing
  # in `config.assertions` has grounds to object. Every "and one thing about it is wrong"
  # fixture below starts from this and changes exactly one field, the same shape nixwatch's
  # own checks/assertions.nix uses for the same reason: it isolates which single change is
  # responsible for a failure.
  validReceiver = {
    nixdeploy.backend = "nixos";
    nixdeploy.receiver.enable = true;
    nixdeploy.receiver.buildLocality = "remote";
    nixdeploy.receiver.manifest = validManifest;
    nixdeploy.receiver.activation = validActivation;
  };
in
[
  # --- the base fixture itself, listed first: every negative check below is "validReceiver
  #     with exactly one field changed for the worse", so if this one does not build clean,
  #     every "fails" check downstream is proving nothing. ------------------------------------
  (check "assertions/minimal-valid-receiver-evaluates-cleanly"
    (! buildFails validReceiver)
    "expected the minimal valid receiver fixture to build cleanly, but it did not")

  # --- backend "nix-darwin" + buildLocality "remote": a darwin closure cannot be produced on
  #     a Linux builder, so a Mac cannot receive one -- it can only ever build its own. -------
  (check "assertions/darwin-backend-with-remote-locality-fails-the-build"
    (buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.backend = "nix-darwin";
    }))
    "expected backend = \"nix-darwin\" with the default buildLocality (\"remote\") to fail the build, but it succeeded")

  (check "assertions/darwin-backend-with-local-locality-builds-fine"
    (! buildFails {
      nixdeploy.backend = "nix-darwin";
      nixdeploy.receiver.enable = true;
      nixdeploy.receiver.buildLocality = "local";
      # Deliberately no manifest and no activation here: buildLocality = "local" is exactly
      # the case where the "must be told where its manifest is" assertion's own short-circuit
      # (`buildLocality == "local" || manifest.url != ""`) never forces `manifest.url` at all
      # -- a machine that only ever builds its own closure genuinely needs neither.
    })
    "a nix-darwin receiver with buildLocality already forced to \"local\" should never fail the build on its own")

  # The restriction is darwin-SPECIFIC, not a blanket ban on remote builds -- the exact same
  # buildLocality = "remote" that fails under nix-darwin above must build fine under a backend
  # that can actually be cross-built for.
  (check "assertions/non-darwin-backend-with-remote-locality-builds-fine"
    (! buildFails validReceiver)
    "expected backend = \"nixos\" with buildLocality = \"remote\" to build cleanly, but it did not")

  # --- a ceiling with nowhere to route the refusal it exists to produce: maxInplaceDeltaBytes
  #     says "reimage me instead if this is too big", but with no provider there is no
  #     provisioning adapter to reimage through -- the refusal could not be routed anywhere. --
  (check "assertions/ceiling-set-without-provider-fails-the-build"
    (buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.receiver.maxInplaceDeltaBytes = 500 * 1024 * 1024;
    }))
    "expected a ceiling with no declared provider to fail the build, but it succeeded")

  (check "assertions/ceiling-set-with-provider-builds-fine"
    (! buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.provider = "example-provider";
      nixdeploy.receiver.maxInplaceDeltaBytes = 500 * 1024 * 1024;
    }))
    "a ceiling paired with a declared provider should never fail the build on its own")

  (check "assertions/no-ceiling-without-provider-builds-fine"
    (! buildFails validReceiver)
    "leaving maxInplaceDeltaBytes unset (null, its default) should never require a provider -- null means no ceiling, not \"not yet tuned\"")

  # --- a receiver that does not build locally and was not told where its manifest is: nothing
  #     could ever tell it what it is supposed to become. --------------------------------------
  (check "assertions/remote-locality-without-manifest-url-fails-the-build"
    (buildFails (lib.recursiveUpdate validReceiver {
      nixdeploy.receiver.manifest.url = "";
    }))
    "expected buildLocality = \"remote\" with an empty manifest.url to fail the build, but it succeeded")

  (check "assertions/remote-locality-with-manifest-url-builds-fine"
    (! buildFails validReceiver)
    "buildLocality = \"remote\" with a real manifest.url should never fail the build on its own")

  # --- the module must load under all three backend values on its own, with the receiver and
  #     publisher both off -- merely stating a backend must never touch a primitive only one
  #     or two of the three targets actually have. Each of these composes ONLY nixdeployModule
  #     plus the bare NixOS baseline (see `bareStubs` above): if the option surface secretly
  #     reached for something backend-specific, it would surface here as an eval failure
  #     regardless of which real platform this ran on. ------------------------------------------
  (check "assertions/loads-under-nixos-backend"
    (! buildFails { nixdeploy.backend = "nixos"; })
    "expected backend = \"nixos\" alone (receiver and publisher both off) to build cleanly, but it did not")

  (check "assertions/loads-under-system-manager-backend"
    (! buildFails { nixdeploy.backend = "system-manager"; })
    "expected backend = \"system-manager\" alone (receiver and publisher both off) to build cleanly, but it did not")

  (check "assertions/loads-under-nix-darwin-backend"
    (! buildFails { nixdeploy.backend = "nix-darwin"; })
    "expected backend = \"nix-darwin\" alone (receiver and publisher both off) to build cleanly, but it did not")
]
