# nixdeploy -- the option surface.
#
# Loadable under THREE backends (NixOS, system-manager, nix-darwin) from one file. It
# therefore touches no backend-specific primitive in its own option surface; everything a
# platform needs to do differently is reached through an ADAPTER (see `activation` and
# `provisioning` below), never through a conditional in this file.
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
  inherit (lib) mkOption mkEnableOption types mkIf literalExpression;
  cfg = config.nixdeploy;

  # Host FACTS are read defensively BY NAME from whatever namespace the operator uses to
  # declare them, never taken as a flake input (see flake.nix). `or null` throughout: this
  # module must stay loadable on a host that declares no facts at all, in which case the
  # operator states the two values it actually needs directly on nixdeploy.
  factClass = config.nixhost.stance.class or null;
  factProvider = config.nixhost.stance.provider or null;

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
    };
  };

  provisioningAdapter = types.submodule {
    options = {
      reimage = mkOption {
        type = types.str;
        description = ''
          Command that replaces this machine wholesale with a named image, used when a
          change is too large to apply in place. Receives the image reference as its
          single argument.

          This runs on the PUBLISHER side, not on the machine being replaced -- a machine
          cannot reliably participate in its own replacement, and requiring it to be
          reachable would reintroduce the dependency this design removes. It is therefore
          also the only recovery path that works when the machine is wedged.
        '';
      };

      imageRef = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          Command printing the image reference this machine is currently running from, if
          the provider can report that. `null` where it cannot; convergence is then judged
          from `currentPath` alone.
        '';
      };
    };
  };
in
{
  options.nixdeploy = {
    backend = mkOption {
      type = types.enum [ "nixos" "system-manager" "nix-darwin" ];
      example = "nixos";
      description = ''
        Which flake output composed this module. Required, with no default, and stated by
        the caller rather than detected: this module cannot ask which backend loaded it
        without reading a backend-specific primitive, which is precisely what would make
        it fail to load under the other two.
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
        closure or receives one (see `buildLocality`) -- and nothing else. The tier itself
        is a fact belonging to whoever declares it; deriving further policy from it here
        would make every consumer inherit this repo's opinion of what a tier means.
      '';
    };

    provider = mkOption {
      type = types.nullOr types.str;
      default = factProvider;
      defaultText = literalExpression "config.nixhost.stance.provider or null";
      description = ''
        Where this machine runs, in the operator's own vocabulary. Selects the provisioning
        adapter, and therefore determines whether this machine can be reimaged at all.
      '';
    };

    receiver = {
      enable = mkEnableOption "the nixdeploy receiver on this machine";

      buildLocality = mkOption {
        type = types.enum [ "remote" "local" ];
        default = "remote";
        description = ''
          Whether this machine's closure is built elsewhere and fetched (`remote`) or built
          here (`local`).

          `remote` is the default on purpose: a builder with a warm store and a populated
          cache exists precisely so that machines do not each repeat that work, and the
          machines least able to build are the ones most likely to be forgotten when
          choosing. `local` is correct for genuinely capable machines, and unavoidable
          where a closure cannot be cross-built for this platform at all -- a nix-darwin
          system cannot be produced on Linux, so a Mac always builds its own.
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
        '';
      };

      downloadBufferSize = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        example = 64 * 1024 * 1024;
        description = ''
          Substituter download buffer. Same reasoning as `httpConnections`: a throughput
          setting that is really a memory ceiling on machines that have none to spare.
        '';
      };

      manifest = {
        url = mkOption {
          type = types.str;
          description = ''
            Where to fetch the manifest naming this machine's target closure.

            This should be reachable WITHOUT any overlay, VPN or mesh this estate also
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
        type = types.str;
        default = "1h";
        description = ''
          How often this machine checks the manifest on its own. This is the floor, not the
          expected latency: a publisher may additionally poke a reachable machine to check
          immediately. A poke that fails costs nothing, because this interval still runs.
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
          How this machine becomes a closure. Contributed by whichever module implements
          this machine's backend, not hand-written per host: the point of an adapter
          registry is that the same three verbs are implemented once per platform rather
          than once per machine.
        '';
      };
    };

    publisher = {
      enable = mkEnableOption "the nixdeploy publisher on this machine";

      targets = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = ''
          Names of the machines this publisher builds and publishes for. Machines that
          build locally are excluded -- there is nothing to publish for a machine that
          produces its own closure.
        '';
      };

      cache = {
        writeUrl = mkOption {
          type = types.str;
          description = "Store URL the publisher signs and writes finished closures to.";
        };
        signingKeyFile = mkOption {
          type = types.path;
          description = ''
            Private key used to sign closures. Receivers verify against its public half
            before running anything the manifest names.
          '';
        };
      };

      manifestOutput = mkOption {
        type = types.str;
        description = ''
          Where the publisher writes the signed manifest. Serving it is deliberately not
          this module's job -- any static HTTP origin will do, and coupling delivery to a
          particular server is how a delivery system acquires a single point of failure it
          did not need.
        '';
      };

      provisioning = mkOption {
        type = types.attrsOf provisioningAdapter;
        default = { };
        description = ''
          Reimage adapters, keyed by provider. A machine whose provider has no entry here
          cannot be reimaged, and the receiver's refusal is then terminal rather than a
          routing decision -- which is a real state worth reporting, not one to paper over
          by silently doing nothing.
        '';
      };
    };
  };

  config = mkIf (cfg.receiver.enable || cfg.publisher.enable) {
    assertions = [
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
          could not be routed to any reimage adapter. Either declare the provider or drop
          the ceiling deliberately.
        '';
      }
    ];
  };
}
