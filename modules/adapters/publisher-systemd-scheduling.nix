# `nixdeploy.publisher.schedule` for the two systemd backends.
#
# Publishing does not activate a system, write the Nix store or control systemd. It therefore
# does not need UID 0. The NixOS adapter supplies a dedicated system account so another local
# service can read the world-readable manifest from this stable state directory; the
# system-manager adapter, which cannot own the foreign distro's account database, asks
# systemd for a DynamicUser instead. In both cases the three directory directives give the
# process the only writable locations it owns. Serving the result remains outside nixdeploy.
#
# The signing key is a systemd credential rather than a path the transient user must be able
# to read directly. The service manager opens the configured source and exposes a private,
# read-only copy at `%d/signing-key` for the duration of this unit. The key's contents never
# enter argv, the Nix store or the service-owned state directory.
{ lib }:

{
  # mkSchedule :: {
  #   name, description, package, targetsFile, revision, signingKeyFile,
  #   manifestOutput, baseManifest, selectHosts, selectPlanes, intervalSeconds,
  #   dynamicUser?
  # } -> attrs
  mkSchedule =
    { name
    , description
    , package
    , targetsFile
    , revision
    , signingKeyFile
    , manifestOutput
    , baseManifest
    , selectHosts
    , selectPlanes
    , intervalSeconds
    , dynamicUser ? true
    }:
    let
      argv = [
        "${package}/bin/nixdeploy"
        "publish"
        "--targets"
        targetsFile
        "--revision"
        revision
        "--signing-key-file"
        "%d/signing-key"
        "--out"
        manifestOutput
      ]
      ++ lib.optionals (baseManifest != null) [ "--base-manifest" baseManifest ]
      ++ lib.concatMap (host: [ "--host" host ]) selectHosts
      ++ lib.concatMap (plane: [ "--plane" plane ]) selectPlanes;
    in
    {
      systemd.services.${name} = {
        inherit description;

        after = [ "local-fs.target" ];

        serviceConfig = {
          Type = "oneshot";

          StateDirectory = "nixdeploy-publisher";
          StateDirectoryMode = "0755";
          CacheDirectory = "nixdeploy-publisher";
          RuntimeDirectory = "nixdeploy-publisher";
          WorkingDirectory = "/var/lib/nixdeploy-publisher";
          Environment = [
            "HOME=/var/lib/nixdeploy-publisher"
            "XDG_CACHE_HOME=/var/cache/nixdeploy-publisher"
          ];

          LoadCredential = [ "signing-key:${signingKeyFile}" ];
          ExecStart = lib.escapeShellArgs argv;

          # The publisher only reads immutable input and writes its own state directory. It
          # neither uses the network nor performs activation, so the hardening that would
          # break a receiver is both safe and useful here.
          NoNewPrivileges = true;
          PrivateDevices = true;
          PrivateNetwork = true;
          PrivateTmp = true;
          ProtectHome = true;
          ProtectKernelModules = true;
          ProtectKernelTunables = true;
          ProtectSystem = "strict";
          RestrictSUIDSGID = true;
          CapabilityBoundingSet = "";

          Restart = "no";
          TimeoutStartSec = "5min";
          UMask = "0022";
          SyslogIdentifier = name;
        }
        // lib.optionalAttrs dynamicUser {
          DynamicUser = true;
        }
        // lib.optionalAttrs (!dynamicUser) {
          User = "nixdeploy-publisher";
          Group = "nixdeploy-publisher";
        };
      };

      systemd.timers.${name} = {
        inherit description;
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "2min";
          OnUnitActiveSec = "${toString intervalSeconds}s";
        };
      };
    };
}
