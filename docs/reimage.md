# Reimage delivery

Reimage is the guarded alternative to in-place activation when a NixOS receiver measures a
delta above `receiver.maxInplaceDeltaBytes`. This document separates what schema version 3
can represent, what the receiver can do today, and what remains unimplemented.

## Signed boot contract

Boot authority is orthogonal to the NixOS configuration target. A NixOS manifest leaf keeps
its exact `target` and must carry one of these objects:

```json
{ "mode": "none" }
```

or:

```json
{
  "mode": "managed",
  "roles": {
    "primary": {
      "artifact": "/nix/store/00000000000000000000000000000000-primary-boot",
      "image": "provider-immutable-primary-image"
    },
    "nixrescue": {
      "artifact": "/nix/store/11111111111111111111111111111111-nixrescue-boot",
      "image": "provider-immutable-rescue-image"
    }
  }
}
```

`none` is a boot authority mode, not a role. `managed` requires `primary`; `nixrescue` is
optional. Each role owns its exact nixboot-produced artifact and optional provider-native
image reference. A role is not a manifest plane, and a host may carry both roles at once.

The configuration target, role, artifact and image are covered by the manifest signature.
The receiver never accepts an image reference supplied only by mutable local configuration.

## Current receiver path

`nixdeploy.receiver.reimage` is a guarded request:

```nix
nixdeploy.receiver.reimage = {
  command = "${pkgs.exampleProvisioner}/bin/reimage";
  role = "primary";
};
```

It is valid only for a NixOS receiver. On every over-ceiling decision, the receiver first
persists `reimage-owed-<plane>.json` in `stateDirectory` with mode `0600`; without an
explicit request it records the `primary` role as the required recovery route and returns a
typed refusal. With a request configured, it then:

1. resolves the configured role from the verified manifest;
2. requires both its exact artifact store path and provider image reference;
3. invokes the provider command with three distinct arguments; and
4. returns `Reimaged { role, artifact, image }` only if the command exits successfully.

The command contract is:

```text
argv[1] = role
argv[2] = exact signed boot artifact store path
argv[3] = exact signed provider image reference
```

The generic provisioning wrapper also exports these as `BOOT_ROLE`, `BOOT_ARTIFACT` and
`IMAGE_REF`, and rejects any invocation that does not have exactly three arguments.

Persisting the owed record is part of the safety boundary. If the record cannot be written,
the receiver returns `Failed { stage: state, ... }` and does not call the provider. Once
recorded, the debt remains visible through `nixdeploy_reimage_owed` across unrelated failed
timer runs. It is cleared only after a later run observes `Converged` or `AlreadyCurrent`.

`Reimaged` therefore means only that the provider accepted this exact request. The process
making the request runs on the machine being replaced and may disappear as the provider
acts; it cannot honestly claim that the replacement booted or passed health checks. The
later convergence outcome supplies that evidence.

## Role support today

The schema deliberately represents both required delivery roles, but the current on-target
provider actuator implements only `primary`.

- `primary`: the exact signed artifact and image may be passed to the provider command.
- `nixrescue`: the receiver returns `Failed { stage: reimage, ... }` explaining that this
  actuator is not implemented, and it does not invoke the provider command.
- `mode: none`: no role can be materialised; a configured reimage request fails explicitly.

This is a typed limitation, not a reason to collapse roles into one field or silently use the
primary image for recovery.

With no `receiver.reimage` request configured, an over-ceiling run remains
`Refused { reason, bytes, ceiling }`. That is a complete policy answer for a host that must
never replace itself automatically.

## Off-target recovery is still absent

`nixdeploy.publisher.provisioning` records a provider-adapter contract, but the scheduled
publisher does not read or invoke it. Publishing a signed static file does not grant
authority to mutate a provider. `imageRef` is likewise reserved and currently has no caller.

No controller in this repository can yet replace a host that is too broken or unreachable
to run its receiver. The implemented path is on-target and therefore still requires a
working receiver, access to the signed manifest and cache metadata, and a provider command
that can be invoked from that host. Off-target reconciliation is the missing piece required
for provider control to become a true recovery floor.

## Why this path exists

Fetching, decompressing, registering and activating many new store paths can overlap in
memory. On a small host, that peak can be unsafe even when every individual operation is
ordinary. The receiver therefore sizes missing paths from cache metadata before downloading
them. Reimage lets a provider materialise a prebuilt boot image instead of asking the target
to survive a delta it has already refused.

A RAM-resident installer is not equivalent to provider materialisation: it still consumes
the target's memory and may fail for the same reason the in-place path was refused. The
intended provider contract replaces an instance or boot volume from an already-produced
image. nixdeploy chooses and records the signed artifact; private Infra supplies provider
identity, commands, credentials and policy.

Because crossing the ceiling removes the in-place route, provider reimage is a ratchet rather
than a casual fallback. Any configured adapter should be exercised regularly before a real
host depends on it. A missing or failing adapter must remain loud; it must never degrade to a
successful no-op.
