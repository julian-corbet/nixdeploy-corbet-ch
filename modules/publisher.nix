# The publisher option surface and the structured job consumed by a backend adapter.
#
# This module never names systemd or launchd. As with the receiver, scheduling belongs to the
# backend adapter: the option surface assembles one complete job, and the adapter translates
# it into its platform's unit vocabulary. The two systemd backends currently implement that
# verb; nix-darwin refuses publisher.enable until it has an equally unprivileged scheduler.
{ config, lib, pkgs, ... }:

let
  inherit (lib) mkEnableOption mkIf mkOption literalExpression types;
  cfg = config.nixdeploy.publisher;
  manifestSchema = import ../lib/manifest.nix { inherit lib; };

  publisherName = "nixdeploy-publisher";
  hasSelection = cfg.select.hosts != [ ] || cfg.select.planes != [ ];

  provisioningAdapter = types.submodule {
    options = {
      reimage = mkOption {
        type = types.str;
        description = ''
          Command that replaces a machine wholesale with one signed boot-role artifact.
          Receives three arguments: role, exact nixboot artifact store path, and provider
          image reference.

          The scheduled publisher does not call this command: publishing is a deterministic
          manifest update, while reimaging is a provider mutation driven by a receiver's
          refusal. Keeping the registry here records the provider boundary without granting
          a timer that writes static files authority to replace machines.
        '';
      };

      imageRef = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = ''
          Command printing the image reference a provider currently reports. Reserved for
          the provisioning path; it is not an input to manifest publication.
        '';
      };
    };
  };
in
{
  options.nixdeploy.publisher = {
    enable = mkEnableOption "the scheduled nixdeploy manifest publisher on this machine";

    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ../package.nix { };
      defaultText = literalExpression "pkgs.callPackage ./package.nix { }";
      description = ''
        Package providing the `nixdeploy publish` command. It is referenced by absolute store
        path from the scheduled job and is not added to a global package set.
      '';
    };

    targetsFile = mkOption {
      type = types.str;
      example = literalExpression ''"''${pkgs.writeText "nixdeploy-targets.json" (builtins.toJSON targets)}"'';
      description = ''
        JSON candidate target file passed to `nixdeploy publish --targets`. Its shape is the
        current manifest's `hosts.<host>.planes.<plane>` tree without the document metadata;
        the Rust publisher validates it before changing the live manifest.

        This is an already-built input. The publisher never evaluates or builds a host
        configuration and never uploads a closure to a cache.
      '';
    };

    revision = mkOption {
      type = types.str;
      example = "0123456789abcdef";
      description = ''
        Non-empty source/build revision recorded in the manifest. This is data from the
        build that produced `targetsFile`, not something the publisher infers from a checkout.
      '';
    };

    signingKeyFile = mkOption {
      type = types.str;
      example = "/run/secrets/nixdeploy/signing-key";
      description = ''
        Runtime path below `/run` to the ed25519 signing key. On systemd backends the service
        manager reads this source as a credential and exposes a private copy only to the
        unprivileged publisher process; the source path, not the key contents, enters the
        generated unit. Do not use a Nix path literal, which would copy the secret into the
        store.
      '';
    };

    manifestOutput = mkOption {
      type = types.str;
      default = "/var/lib/nixdeploy-publisher/manifest.json";
      description = ''
        Final manifest path. The detached signature is written beside it as `<path>.sig`.
        Scheduled publishers write only below `/var/lib/nixdeploy-publisher`, the state
        directory owned by the service. A static origin should expose that directory
        read-only rather than making the publisher write into a web server's state.
      '';
    };

    baseManifest = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "/var/lib/nixdeploy-publisher/manifest.json";
      description = ''
        Complete existing manifest to merge a partial publication into. Required whenever
        either selector below is non-empty, and forbidden for a full replacement. Pointing
        this at `manifestOutput` makes each atomic publication preserve every unselected
        host/plane leaf from the previously signed document.

        The first publication must be a full replacement: a partial update cannot safely
        invent the targets it was asked to preserve.
      '';
    };

    select = {
      hosts = mkOption {
        type = types.listOf types.str;
        default = [ ];
        description = ''
          Host-name filter, passed as one repeatable `--host` argument per entry. With no
          plane filter, every candidate plane for these hosts is updated.
        '';
      };

      planes = mkOption {
        type = types.listOf (types.enum manifestSchema.backends);
        default = [ ];
        description = ''
          Plane-name filter, passed as one repeatable `--plane` argument per entry. With a
          host filter too, the two independent axes intersect: only matching planes on
          matching hosts are updated. Entries are plane names, never `HOST/PLANE` pairs.
        '';
      };
    };

    interval = mkOption {
      type = types.ints.positive;
      default = 3600;
      description = ''
        Publication cadence in seconds. A backend starts the job shortly after boot and at
        least this often afterwards. Re-publishing unchanged candidates is safe and atomic.
      '';
    };

    # The option-surface-to-adapter seam. This stays structured until the adapter turns it
    # into a platform unit; in particular the signing key source stays a source path here and
    # becomes a systemd credential only on a backend that has that primitive.
    job = mkOption {
      type = types.attrs;
      readOnly = true;
      default = {
        name = publisherName;
        description = "nixdeploy publisher: atomically update the signed target manifest";
        inherit (cfg)
          package
          targetsFile
          revision
          signingKeyFile
          manifestOutput
          baseManifest;
        selectHosts = cfg.select.hosts;
        selectPlanes = cfg.select.planes;
        intervalSeconds = cfg.interval;
      };
      description = ''
        Structured publisher job passed to this backend's `publisher.schedule` verb. It is
        read-only so the command cannot drift from the options that document its inputs.
      '';
    };

    schedule = mkOption {
      type = types.functionTo types.attrs;
      internal = true;
      description = ''
        Backend adapter verb which turns `publisher.job` into a scheduled platform unit.
        Operators compose this repo's backend adapter rather than setting it by hand.
      '';
    };

    provisioning = mkOption {
      type = types.attrsOf provisioningAdapter;
      default = { };
      description = ''
        Provider reimage adapters. This registry is deliberately not called by the manifest
        publisher: publication has no authority to mutate infrastructure. It records the
        off-target provider contract, but no controller consumes it yet. The current
        on-target path is configured separately through `receiver.reimage`.
      '';
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.revision != "";
        message = "nixdeploy: publisher.revision must be non-empty.";
      }
      {
        assertion = lib.hasPrefix "/nix/store/" cfg.targetsFile
          && !(lib.hasInfix "/../" cfg.targetsFile)
          && !(lib.hasSuffix "/.." cfg.targetsFile);
        message = ''
          nixdeploy: publisher.targetsFile must be an immutable, already-built file in the
          Nix store; a mutable checkout or temporary workspace is not a publication input.
        '';
      }
      {
        assertion = lib.hasPrefix "/run/" cfg.signingKeyFile
          && !(lib.hasInfix "/../" cfg.signingKeyFile)
          && !(lib.hasSuffix "/.." cfg.signingKeyFile);
        message = ''
          nixdeploy: publisher.signingKeyFile must be runtime secret material below /run,
          never a login home or the Nix store.
        '';
      }
      {
        assertion = lib.hasPrefix "/var/lib/nixdeploy-publisher/" cfg.manifestOutput
          && !(lib.hasInfix "/../" cfg.manifestOutput)
          && !(lib.hasSuffix "/.." cfg.manifestOutput);
        message = ''
          nixdeploy: publisher.manifestOutput must stay below the publisher's service-owned
          state directory, /var/lib/nixdeploy-publisher.
        '';
      }
      {
        assertion = hasSelection == (cfg.baseManifest != null);
        message = ''
          nixdeploy: a partial publication (select.hosts or select.planes) requires
          publisher.baseManifest so unselected targets are preserved; a full replacement
          must leave baseManifest null.
        '';
      }
      {
        assertion = cfg.baseManifest == null
          || (lib.hasPrefix "/var/lib/nixdeploy-publisher/" cfg.baseManifest
            && !(lib.hasInfix "/../" cfg.baseManifest)
            && !(lib.hasSuffix "/.." cfg.baseManifest));
        message = ''
          nixdeploy: publisher.baseManifest must stay below the publisher's service-owned
          state directory when set.
        '';
      }
      {
        assertion = builtins.all (value: value != "") (cfg.select.hosts ++ cfg.select.planes);
        message = "nixdeploy: publisher selectors must not contain empty names.";
      }
    ];
  };
}
