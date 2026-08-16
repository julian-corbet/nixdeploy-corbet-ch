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

## Role in the nix* team

**nixdeploy is the delivery and deployment specialist.** It is the sole owner
of the path from a successfully built artifact to an observed deployment
outcome across NixOS, system-manager, and Home Manager where that plane can
participate. That includes publication, transport, receiver convergence,
raw-device or provider materialisation, slot rotation and selection,
activation, rollback, health acceptance, image upload/registration and
reimage. It may trigger a build on Crow, but Crow performs the build and
nixdeploy does not become a build engine.

Device class and boot role are independent inputs to that delivery contract:

- `nixarch`, `nixnas`, and `nixvps` describe class-specific runtime,
  hardware, storage, or provider facts;
- `primary` and `nixrescue` identify the purpose of a boot artifact;
- nixboot produces and verifies the boot artifact, while nixrescue produces
  recovery content and runtime; and
- nixdeploy delivers either role to any applicable class and records what
  actually happened.

Concrete hostnames, addresses, disks, cloud projects and image identities,
accounts, UID/GID assignments, endpoints, credentials, keys, resource limits,
cadences and production policy values belong in private Infra and secrets.
Provider-specific OpenTofu and command adapters are private inputs that
nixdeploy orchestrates; they are not public defaults in this repository.

Today the repository has NixOS, system-manager, Home Manager and nix-darwin
activation backends. The release path uses content-addressed deployment sets:
each artifact carries its source revision, lock digest, builder/store versions,
root NAR hash, complete closure digest and receiver compatibility requirements.
The signed payload and signature travel in one atomically replaced file. See
[`docs/release-system.md`](docs/release-system.md) for the release transaction
and the one-way schema-v3 migration. Schema version 3 remains read-only during
that receiver-first cutover.

The current receiver can request
provider materialisation of `primary` after an over-ceiling refusal;
`nixrescue` is represented and verified but returns a typed refusal until its
actuator exists. Off-target recovery, image upload/registration and several
post-boot observations remain incomplete. Those gaps are migration work in
nixdeploy, not reasons to add another delivery mechanism elsewhere.

A delivery outcome must distinguish at least: closure installed, userspace
activated, boot artifact installed, reboot required, boot verified, and health
accepted or rolled back. A successful userspace switch must not be reported as
a verified boot. Reboot authority is explicit: nixdeploy may report and stage
`reboot required`, but must not turn that into an unattended reboot unless the
private deployment policy explicitly grants it.

## The shape

```
   builder ──► signed cache ──► deployment set ──► receiver ──► adapter ──► running
                                      │                         │
                 "host H / plane P is exact artifact X"   "how P becomes X"
```

- **The release service** names what every host plane should be running in a
  signed, content-addressed **deployment set**. `nixdeploy promote` performs a
  compare-and-swap promotion, writes the immutable release and signed journal,
  then atomically moves the stable channel. It builds nothing and uploads nothing —
  producing the closures and getting them into a binary cache the receivers
  trust is the caller's job, done by whatever already does it.
- **The receiver** runs on each managed machine. It reads the manifest, decides
  whether its configured plane can safely become that closure, and if so activates it. It is the
  only component that decides anything about a machine, because it is the only
  component that can observe that machine.
- **Adapters** are small, per-platform implementations of the handful of verbs the
  engine cannot know generically — how a machine becomes a closure, how it keeps
  checking, and how it materialises a signed boot role.

## Two adapter registries

The engine is platform-agnostic. Two questions are not, and each is answered by a
registry keyed off a fact the operator already declares about the machine:

| Question | Keyed by | Adapter provides |
|---|---|---|
| "How do I become this closure, and how do I keep checking?" | backend (`nixos`, `system-manager`, `home-manager`, `nix-darwin`) | `activate`, `currentPath`, `rollback`, `schedule`, `nixSettings` |
| "How do I materialise this signed boot role?" | provider (cloud, hypervisor, bare metal, …) | `reimage`, `imageRef` |

Adding a platform is contributing an adapter, not editing the engine.

## Usage

### Manifest and granular publication

The v4 release service is the production interface. See
[`docs/release-system.md`](docs/release-system.md) for its candidate shape and
`nixdeploy promote`/`recover` commands. The v3 interface below is retained only
to upgrade receivers before the stable channel changes; new integrations should
not publish it.

Schema version 3 models a host as a set of independently targeted planes. A NixOS
plane keeps its exact configuration `target` and also states its boot authority:

```json
{
  "host-a": {
    "planes": {
      "system-manager": {
        "backend": "system-manager",
        "target": "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-system"
      },
      "nixos": {
        "backend": "nixos",
        "target": "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-nixos-system",
        "boot": {
          "mode": "managed",
          "roles": {
            "primary": {
              "artifact": "/nix/store/cccccccccccccccccccccccccccccccc-primary-boot",
              "image": "provider-immutable-image-reference"
            },
            "nixrescue": {
              "artifact": "/nix/store/dddddddddddddddddddddddddddddddd-nixrescue-boot"
            }
          }
        }
      }
    }
  }
}
```

The plane names are the closed set `nixos`, `system-manager`, `home-manager`, and
`nix-darwin`, and each name must equal its leaf's backend. `identity` is required only for
`home-manager`. Every `target` is an immutable `/nix/store/...` path: the publisher never
accepts an installable or evaluates it. A NixOS plane must carry either `{ "mode": "none" }`
for a container or another system with no nixdeploy boot actuator, or `mode: "managed"`
with a required `primary` artifact and an optional `nixrescue` artifact. An image reference,
when available, belongs to its exact role artifact rather than to the plane. Boot roles are
orthogonal to planes: publishing one selected NixOS leaf updates its configuration target
and its complete role set atomically. There is one home-manager identity per host in this
version; supporting several is a schema change, not an ambiguous naming convention.

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

System activation is privileged: replacing a system profile, writing `/etc` and restarting
system units require UID 0. That privilege does **not** make the privileged account's home a
suitable workspace or state directory. On the NixOS and system-manager backends, the
receiver's systemd unit therefore declares:

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

An over-ceiling refusal is recorded first as
`reimage-owed-<plane>.json`, also mode `0600`. That marker survives later failed timer runs
and keeps `nixdeploy_reimage_owed` asserted until a later receiver run actually reports
`Converged` or `AlreadyCurrent`. `Reimaged` means the provider accepted a request; when a
replacement was requested, later observed convergence is what proves it landed.

The Nix store and daemon state remain in their standard `/nix/store` and `/nix/var/nix`
locations, and activation continues to manage `/etc` as the selected backend requires.
Neither the receiver nor the publisher may use an administrator's home for generated state,
cache, credentials or a checkout. The publisher needs no activation privileges: its systemd
unit runs as an unprivileged service identity, receives the signing key through a private systemd
credential, and owns `/var/lib/nixdeploy-publisher`, `/var/cache/nixdeploy-publisher` and
`/run/nixdeploy-publisher`. Backend adapters must provide their platform's
equivalent service-owned locations rather than inheriting a login account's HOME.

A Home Manager plane is intentionally different: its receiver runs in the declared user's
service manager, with `receiver.plane.identity` required to equal `home.username`. Its
receiver state/cache/runtime directories are the `nixdeploy` children of that user's
`XDG_STATE_HOME`, `XDG_CACHE_HOME`, and `XDG_RUNTIME_DIR`; its Home Manager generation and GC
roots remain in Home Manager's own standard per-user locations. It neither runs as UID 0 nor
uses a system account's home. A headless Linux user must have a persistent user manager (for
example, systemd linger) if the timer must run while that user is logged out.

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

`systemManagerModules.backendAdapter`, `homeManagerModules.backendAdapter`, and
`darwinModules.backendAdapter` are the other entries, in the namespace each backend's module
system reads. Code building configurations for mixed hosts can index all four by the same string
it sets `nixdeploy.backend` to:

```nix
modules = [ nixdeploy.nixosModules.nixdeploy nixdeploy.backendAdapters.${host.backend} ];
```

A standalone Home Manager configuration composes the same pair but must bind the plane to
the account it activates:

```nix
home-manager.lib.homeManagerConfiguration {
  inherit pkgs;
  modules = [
    nixdeploy.homeManagerModules.nixdeploy
    nixdeploy.homeManagerModules.backendAdapter
    {
      home.username = "alice";
      home.homeDirectory = "/home/alice";
      home.stateVersion = "25.05"; # keep the account's existing Home Manager state version
      nixdeploy.backend = "home-manager";
      nixdeploy.receiver = {
        enable = true;
        plane.identity = "alice";
        manifest.url = "https://cache.example.org/manifest.json";
        manifest.publicKey = "cache.example.org-1:<base64>";
      };
    }
  ];
}
```

The adapter registers each target in Home Manager's standard `home-manager` profile, runs
the target's `activate --driver-version 1`, and regards
`$XDG_STATE_HOME/home-manager/gcroots/current-home` as current only after activation has
completed. Rollback moves that same standard profile back one generation and activates the
result. `home.activationGenerateGcRoot` must remain enabled so that convergence is observable.
On Linux, the receiver unit uses Home Manager's `X-SwitchMethod=keep-old`: an activation may
install the receiver's replacement unit for the next timer tick, but it must not stop the
in-flight receiver before the final `current-home` proof is written.

The same pair makes `nixdeploy.publisher.enable = true` a real service and timer on the
NixOS and system-manager backends. It validates and atomically publishes an already-built v3
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
    reimageCommand = ''
      example-cli instance replace \
        --role "$BOOT_ROLE" \
        --artifact "$BOOT_ARTIFACT" \
        --image "$IMAGE_REF"
    '';
  };
```

**The provisioning registry remains separate.** The scheduled publisher never reads
`nixdeploy.publisher.provisioning`: signing a static target document does not grant authority
to replace machines. The receiver-side route is live through `nixdeploy.receiver.reimage` and
`src/receive.rs`'s `route_over_ceiling`; an off-target recovery controller for a machine that
cannot run its receiver does not exist yet. The receiver-side command is configured with an
exact role and receives the role, signed boot artifact and signed image reference as three
separate arguments. It currently implements only `primary`; requesting `nixrescue` fails
explicitly without invoking a provider. `imageRef` still has no caller. See
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
and only if — its config names a `primary` reimage command and the signed NixOS boot
role names both an artifact and an image, it durably records the debt before asking the
provider to replace the machine (see the provisioning boundary above for the separate
off-target case).

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
Reimaged { role, artifact, image }
                           — provider accepted an exact replacement request
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
- **Not an operator's policy.** Ceilings, cadences, health gates and host
  classes are inputs. This repo ships no defaults that encode one deployment's taste.

## Reading a sibling by name

Where this module needs facts about a machine — its backend, provider and
capability class — it reads them defensively from a sibling namespace by name
rather than taking a flake input, the same convention used elsewhere in this
family. Facts belong to whoever declares them; this repo only consumes them and
derives its own knobs.
