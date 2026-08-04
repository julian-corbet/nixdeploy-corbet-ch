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
                        "host H should be X"        "how H becomes X"
```

- **The publisher** builds closures for the machines that cannot build their own,
  signs them into a binary cache, and publishes a **manifest**: for each machine,
  the store path it should be running (and, where applicable, the image it should
  be running *from*).
- **The receiver** runs on each managed machine. It reads the manifest, decides
  whether it can safely become that closure, and if so activates it. It is the
  only component that decides anything about a machine, because it is the only
  component that can observe that machine.
- **Adapters** are small, per-platform implementations of the two verbs the
  engine cannot know generically.

## Two adapter registries

The engine is platform-agnostic. Two questions are not, and each is answered by a
registry keyed off a fact the operator already declares about the machine:

| Question | Keyed by | Adapter provides |
|---|---|---|
| "How do I become this closure?" | backend (`nixos`, `system-manager`, `nix-darwin`, …) | `activate`, `currentPath`, `rollback` |
| "How do I become this image?" | provider (cloud, hypervisor, bare metal, …) | `reimage`, `imageRef` |

Adding a platform is contributing an adapter, not editing the engine.

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
numbers**, and the machine is reimaged instead. Refusing is a first-class outcome,
not an error.

## Pull is the floor; push is an accelerator

Every managed machine converges on its own from the manifest. A push is only a
request to *check now*: it makes convergence prompt, and if it fails nothing is
lost, because the machine was going to converge anyway.

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
