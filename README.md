# nixdeploy

The mechanism for getting a **prebuilt** Nix closure onto a machine that did not
build it, and knowing afterwards whether it actually got there.

One problem, stated precisely:

> A capable builder has produced a system closure for some machine. That machine
> must end up running it. The machine may be too small to evaluate or build
> anything itself, may be on a different platform than the builder, may be
> unreachable from the builder, and may be too small to survive activating a
> large change in place.

Everything in this repo follows from that sentence.

## The shape

```
   builder ──► signed cache ──► manifest ──► receiver ──► adapter ──► running
                                    │                        │
                    "host H / plane P is X"     "how P becomes X"
```

- **The publisher** names what every host plane should be running, in a signed
  **manifest**: for each named plane, the exact store path (and, only for a NixOS
  whole-machine plane, the image it should run *from*). `nixdeploy publish` renders that manifest, signs
  it, and writes it next to its detached signature. It builds nothing and uploads
  nothing — producing the closures and getting them into a binary cache the
  receivers trust is the caller's job, done by whatever already does it.
- **The receiver** runs on each managed machine. It reads the manifest, decides
  whether its configured plane can safely become that closure, and if so activates it. It is the
  only component that decides anything about a machine, because it is the only
  component that can observe that machine.
- **Adapters** are small, per-platform implementations of the handful of verbs the
  engine cannot know generically — how a machine becomes a closure, how it keeps
  checking, and how it becomes an image.

## Two adapter registries

The engine is platform-agnostic. Two questions are not, and each is answered by a
registry keyed off a fact the operator already declares about the machine:

| Question | Keyed by | Adapter provides |
|---|---|---|
| "How do I become this closure, and how do I keep checking?" | backend (`nixos`, `system-manager`, `nix-darwin`, …) | `activate`, `currentPath`, `rollback`, `schedule`, `nixSettings` |
| "How do I become this image?" | provider (cloud, hypervisor, bare metal, …) | `reimage`, `imageRef` |

Adding a platform is contributing an adapter, not editing the engine.

## Usage

### Manifest and granular publication

Schema version 2 models a host as a set of independently targeted planes:

```json
{
  "host-a": {
    "planes": {
      "system-manager": {
        "backend": "system-manager",
        "target": "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system"
      },
      "home-manager": {
        "backend": "home-manager",
        "identity": "alice",
        "target": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-home-manager-generation"
      }
    }
  }
}
```

Version 2's plane names are the closed set `nixos`, `system-manager`, `home-manager`, and
`nix-darwin`, and each name must equal its leaf's backend. `identity` is required only for
`home-manager`. `image` is optional and valid only on a `nixos` plane. Every `target` is an
immutable `/nix/store/...` path: the publisher never accepts an installable or evaluates it.
There is one home-manager identity per host in this version; supporting several is a schema
change, not an ambiguous naming convention.

A complete publication replaces the complete manifest:

```console
nixdeploy publish --targets targets.json --revision REV \
  --signing-key-file manifest.key --out manifest.json
```

Granular publication uses independent selector axes. Repeated `--host` values restrict
hosts; repeated `--plane` values restrict plane names; when both are present their
intersection is updated. A partial publication must name the current complete manifest as
its base (including the base's sibling `.sig`), and every unselected leaf is preserved from
it. The base signature must verify with the publication key before anything is carried
forward:

```console
nixdeploy publish --targets candidates.json --base-manifest manifest.json \
  --host host-a --plane system-manager --revision REV \
  --signing-key-file manifest.key --out manifest.next.json
```

This is a safe read-modify-write operation, not a smaller replacement manifest: selecting
one plane cannot remove the targets needed by every other receiver.

A machine composes **two** modules: the option surface, and its backend's adapter.
The pair is what `receiver.enable = true` turns into a running receiver — the
option surface deliberately names no backend's option tree, so it cannot render a
unit by itself (see `modules/adapters/apply.nix` for the module-system property
that forces this, and why it is the same property the adapter registry exists for).

### Filesystem and privilege contract

Activation is a privileged operation: replacing a system profile, writing `/etc` and
restarting system units require UID 0. That privilege does **not** make the privileged
account's home a suitable workspace or state directory. On the NixOS and system-manager
backends, the receiver's systemd unit therefore declares:

- `StateDirectory=nixdeploy` with `HOME=/var/lib/nixdeploy`;
- `CacheDirectory=nixdeploy` with `XDG_CACHE_HOME=/var/cache/nixdeploy`;
- `RuntimeDirectory=nixdeploy` for ephemeral runtime material.

The receiver JSON contract names the persistent location as `stateDirectory` (default
`/var/lib/nixdeploy`, matching the systemd declaration). Health-rejected targets are recorded
there as plane-scoped `rejected-target-<plane>.json` files with mode `0600`. A target that
failed a health gate and was successfully rolled back is not activated again on every timer
tick: later runs stop before delta sizing with `Failed { stage: rejectedTarget }`. Because a
Nix store path is immutable, publishing a new store path is the normal recovery; its first
healthy convergence clears the stale pin. Removing the pin earlier is an explicit operator
override to retry the exact same closure.

The Nix store and daemon state remain in their standard `/nix/store` and `/nix/var/nix`
locations, and activation continues to manage `/etc` as the selected backend requires.
Neither the receiver nor the publisher may use an administrator's home for generated state,
cache, credentials or a checkout. The publisher needs no activation privileges: its systemd
unit runs as an unprivileged service identity, receives the signing key through a private systemd
credential, and owns `/var/lib/nixdeploy-publisher`, `/var/cache/nixdeploy-publisher` and
`/run/nixdeploy-publisher`. Backend adapters must provide their platform's
equivalent service-owned locations rather than inheriting a login account's HOME.

```nix
{
  inputs.nixdeploy.url = "github:julian-corbet/nixdeploy-corbet-ch";

  outputs = { nixpkgs, nixdeploy, ... }: {
    nixosConfigurations.host-a = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        nixdeploy.nixosModules.nixdeploy       # the option surface
        nixdeploy.nixosModules.backendAdapter  # the "nixos" entry in the backend registry

        {
          nixdeploy.backend = "nixos";
          nixdeploy.provider = "example-provider";

          nixdeploy.receiver = {
            enable = true;
            manifest.url = "https://cache.example.org/manifest.json";
            manifest.publicKey = "cache.example.org-1:<base64>";
            maxInplaceDeltaBytes = 500 * 1024 * 1024;  # your ceiling; there is no default
            interval = 3600;                           # seconds
            httpConnections = 4;
            downloadBufferSize = 64 * 1024 * 1024;
          };
        }
      ];
    };
  };
}
```

`systemManagerModules.backendAdapter` and `darwinModules.backendAdapter` are the
other two entries, in the namespace each backend's module system reads. Code
building configurations for a mixed fleet can index all three by the same string
it sets `nixdeploy.backend` to:

```nix
modules = [ nixdeploy.nixosModules.nixdeploy nixdeploy.backendAdapters.${host.backend} ];
```

The same pair makes `nixdeploy.publisher.enable = true` a real service and timer on the
NixOS and system-manager backends. It validates and atomically publishes an already-built v2
target tree, and supports safe host/plane selection by merging into a complete base manifest.
It does not evaluate, build, upload or serve anything. See
[`docs/publisher.md`](docs/publisher.md) for the complete option set, bootstrap/partial-update
rules and operating checks.

The second registry is populated by the operator, not by this repo, so it ships as
a factory rather than a set of adapters, built against whichever `pkgs` will run
the command:

```nix
nixdeploy.publisher.provisioning.example-provider =
  (nixdeploy.lib.provisioning pkgs).mkAdapter {
    name = "example-provider";
    runtimeInputs = [ pkgs.example-cli ];
    reimageCommand = ''example-cli instance replace --image "$IMAGE_REF"'';
  };
```

**The provisioning registry remains separate.** The scheduled publisher never reads
`nixdeploy.publisher.provisioning`: signing a static target document does not grant authority
to replace machines. The receiver-side route is live through `nixdeploy.receiver.reimage` and
`src/receive.rs`'s `route_over_ceiling`; an off-target recovery controller for a machine that
cannot run its receiver does not exist yet. `imageRef` still has no caller. See
[`docs/reimage.md`](docs/reimage.md) for that exact boundary.

## Why the receiver decides

A small machine can be destroyed by activating a large change in place — parallel
NAR decompression, download buffers, store registration and a mass unit restart
all peak at once. So a size ceiling is necessary.

**That ceiling is a fact about the receiver**, and only the receiver knows its own
store, so only the receiver can size the change: it fetches `.narinfo` metadata for
the paths it is missing and sums them, without downloading anything. A sender
cannot do this correctly — it would have to model each machine's store from a
record kept somewhere else, and that record is wrong precisely when it matters
(after an unclean run, a garbage collection, or a restore).

When the change is over the ceiling, the receiver **refuses, loudly, with the
numbers**, and stops there. Refusing is a first-class outcome, not an error. If —
and only if — its config names a reimage command and the manifest names an image
for this machine, it records the refusal and then asks for the machine to be
replaced instead (see the provisioning boundary above for the separate off-target case).

## Pull is the floor

Every managed machine converges on its own from the manifest, on its own timer,
with no publisher in the loop. That is the whole delivery guarantee, and it is
the only one this repo implements: there is no push mechanism here. A push, if an
operator builds one, can only ever be a request to *check now* — it makes
convergence prompt, and if it fails nothing is lost, because the machine was
going to converge anyway.

This is deliberate. A delivery system that depends on reaching a machine cannot
deliver the fix for the thing that made the machine unreachable — and network
configuration is itself something you deliver. Pull inverts that dependency. It
also means machine count costs the publisher nothing.

## Outcomes are typed

Every run of the receiver ends in exactly one of:

```
Converged { from, to }     — activated, health-gated, and confirmed
AlreadyCurrent { rev }     — nothing to do
Refused { reason, bytes, ceiling }
                           — safe, deliberate, and NOT a failure
Failed { stage, detail }   — something broke; says which stage
Reimaged { image }         — replaced rather than switched
```

"Did nothing" and "succeeded" are different values. A run that delivers to no one
cannot report success, because there is no outcome that means that.
`Failed { stage: rejectedTarget }` is the persistent, typed answer for a signed immutable
target that a prior run already health-rejected and rolled back; it is loud without repeating
the dangerous activation.

## Non-goals

- **Not a builder.** It never evaluates Nix. Evaluation is the expensive step that
  happens *before* any cache can help, and pushing it onto a small machine is the
  failure this repo exists to prevent.
- **Not a cloud provisioner.** It asks a provider adapter to reimage a machine; it
  does not create, size, bill or destroy infrastructure.
- **Not a CI system.** It has no opinion about what triggers a build.
- **Not a monitoring stack.** It reports its own outcomes and stops there.
- **Not an operator's policy.** Ceilings, cadences, health gates and machine
  classes are inputs. This repo ships no defaults that encode one estate's taste.

## Reading a sibling by name

Where this module needs facts about a machine — its backend, provider and
capability class — it reads them defensively from a sibling namespace by name
rather than taking a flake input, the same convention used elsewhere in this
family. Facts belong to whoever declares them; this repo only consumes them and
derives its own knobs.
