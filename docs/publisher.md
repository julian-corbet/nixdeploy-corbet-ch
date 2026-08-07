# Scheduled publication

`nixdeploy.publisher.enable` turns the already-working `nixdeploy publish` command into a
real service and timer on the NixOS and system-manager backends. It does one job: validate,
merge, sign and atomically write the target manifest a receiver reads.

It still does not build, evaluate or upload anything. A build system produces the target
closures, makes them available through a binary cache and writes the candidate target JSON.
The publisher is the narrow commit point after those steps: once it replaces the signed
manifest, enabled receivers can converge to the new targets.

## Complete configuration

```nix
{
  nixdeploy.backend = "nixos";

  nixdeploy.publisher = {
    enable = true;
    targetsFile = builtins.toString (pkgs.writeText "nixdeploy-targets.json" ''
      {
        "host-a": {
          "planes": {
            "nixos": {
              "backend": "nixos",
              "target": "/nix/store/00000000000000000000000000000000-system"
            }
          }
        }
      }
    '');
    revision = "0123456789abcdef";

    # A runtime secret path supplied by sops-nix, agenix or an equivalent mechanism.
    # Write this as a string, not a Nix path literal: a path literal copies its input to
    # the world-readable Nix store.
    signingKeyFile = "/run/secrets/nixdeploy/signing-key";

    interval = 3600;
  };
}
```

The default output is `/var/lib/nixdeploy-publisher/manifest.json`; its detached signature
is `/var/lib/nixdeploy-publisher/manifest.json.sig`. The first successful publication is a
full replacement and therefore has no selectors and no base manifest.

`targetsFile` has the same target tree as manifest schema v2, without top-level document
metadata:

```text
host -> planes -> plane name -> { backend, identity?, target, image? }
```

`identity` is required only for a `home-manager` plane and forbidden on every other backend.
`image` is allowed only for a `nixos` plane. The publisher validates those rules before it
changes either output file.

## Partial publication

After the complete manifest exists, a build can update a subset without deleting every
unselected target:

```nix
nixdeploy.publisher = {
  baseManifest = "/var/lib/nixdeploy-publisher/manifest.json";
  select.hosts = [ "host-a" "host-b" ];
  select.planes = [ "nixos" ];
};
```

Host and plane selectors are independent axes. Multiple values on one axis are alternatives;
when both axes are present they intersect. The example updates the `nixos` plane on `host-a`
and `host-b`, while every other host/plane leaf is preserved from `baseManifest`.

The module enforces the safe combinations:

- any host or plane selector requires `baseManifest`;
- a full replacement forbids `baseManifest`;
- plane selectors contain plane names, never encoded `HOST/PLANE` pairs.

Pointing `baseManifest` at `manifestOutput` is intentional. The publisher reads and verifies
the complete old document before its atomic writes replace the signature and manifest. A
missing or invalid base fails the run and leaves the existing publication untouched.

## Service contract

On NixOS and system-manager the backend adapter emits
`nixdeploy-publisher.service` and `nixdeploy-publisher.timer`.

The publisher never runs as UID 0. NixOS declares a dedicated `nixdeploy-publisher` system
account so a local static origin can read its stable state directory; system-manager leaves
the foreign distro's account database alone and uses `DynamicUser=yes`. Both own only:

- `/var/lib/nixdeploy-publisher` for persistent output and `HOME`;
- `/var/cache/nixdeploy-publisher` for `XDG_CACHE_HOME`;
- `/run/nixdeploy-publisher` for runtime state.

Systemd reads `signingKeyFile` with `LoadCredential=` and gives the transient service user a
private read-only credential at run time. The secret contents are never put in argv, an
environment variable, a generated Nix file or the service's state directory.

Because this process only reads immutable candidates and writes a static file, it also runs
without a network namespace, devices, capabilities, access to home directories or a writable
system tree. Those restrictions are deliberately different from the receiver: activation
must be UID 0 and must write the system profile and `/etc`; publication needs none of that.

`manifestOutput` is restricted to the service state directory. A web server or object-store
sync should consume that directory read-only. Giving a manifest signer write access to a web
server's state would join two privilege domains that do not need to be joined.

The scheduled publisher is not enabled on nix-darwin yet. launchd has no direct equivalent of
the DynamicUser plus credential contract above; silently falling back to root would give a
static-file writer privileges it does not need, so the adapter fails evaluation instead.

## Operating it

After activating the host configuration:

```console
systemctl status nixdeploy-publisher.timer
systemctl start nixdeploy-publisher.service
journalctl -u nixdeploy-publisher.service
```

A successful run prints one JSON object from `nixdeploy publish`, including the exact
host/plane targets it updated and the total number of targets in the complete output. Check
both the manifest and signature are visible at the static origin before treating publication
as delivery. Receivers remain the final authority: only their later `Converged` or
`AlreadyCurrent` outcome proves a target is live.

The publisher timer is intentionally not a cache uploader and not a provisioner. Cache upload
must finish before publication; provider replacement remains downstream of a receiver's typed
over-ceiling refusal and the separately configured provisioning adapter.
