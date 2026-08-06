# The adapter registries

Read `modules/default.nix` and `modules/publisher.nix` first -- their `activationAdapter` and
`provisioningAdapter` submodules are the authoritative contracts; this file explains how the two registries built
on top of them relate, and how to add to either one. `provisioning-README.md` in this same
directory goes deep on the provisioning registry specifically; this file's own deep dive is
the activation registry, which is what `nixos.nix`, `system-manager.nix` and `nix-darwin.nix`
here implement.

## Two registries, two different questions, two different keys

| | Question | Keyed by | Adapter provides | Runs on |
|---|---|---|---|---|
| **Activation** | "How do I become this closure, and how do I keep checking?" | `nixdeploy.backend` -- which Nix module system built this machine's config (`nixos`, `system-manager`, `nix-darwin`) | `activate`, `currentPath`, `rollback`, `schedule`, `nixSettings` | the receiver, i.e. the machine being converged |
| **Provisioning** | "How do I become this image?" | `nixdeploy.provider` -- an operator-chosen name for where this machine runs, in the operator's own vocabulary | `reimage`, `imageRef` | nothing yet -- see below |

The provisioning row is an off-target contract with no caller. Nothing reads
`nixdeploy.publisher.provisioning`; the scheduled manifest publisher deliberately has no
provider authority. The receiver-side route (`src/receive.rs`'s `route_over_ceiling`) is
reachable through `nixdeploy.receiver.reimage`, while `imageRef` still has no reader. The
rest of this file is about the ACTIVATION registry, which is fully wired.

The five activation verbs split into two kinds, and the split is not cosmetic. `activate`,
`currentPath` and `rollback` are COMMAND LINES: strings, transcribed by `modules/default.nix`
into the receiver's JSON config and shelled out by the Rust binary at run time. `schedule` and
`nixSettings` are FUNCTIONS returning configuration: they are called at EVAL time, never
serialized, and never seen by the binary at all. `modules/default.nix`'s own comment on the
submodule explains why they nonetheless live in the same registry -- same key, same file, and
a second registry keyed identically would only be a second place to forget.

Both registries exist because `modules/default.nix`'s option surface is deliberately
backend- and provider-agnostic -- it asks each machine to state which Nix module system
built it (`backend`) and where it runs (`provider`), and answers neither "how" question
itself. A registry entry is what turns a fact a machine already states about itself into an
actual command line. Neither registry is consulted by the other: a machine's activation
adapter has no idea what provider it runs on, and a provisioning adapter has no idea (or
need to know) what backend the machine it's replacing used -- reimaging installs a whole new
image, activation was never going to run against it anyway.

The split between the two is also why they end up looking different in this directory.
`backend` is a closed `types.enum` in `modules/default.nix` -- three real, currently-
supported module systems, not a free string -- so the activation registry is three
hand-written files, one per value the enum can take, each gated on matching `cfg.backend`
(see "adding a new backend" below for what that costs). `provider` is `types.nullOr
types.str`, "in the operator's own vocabulary" -- an open set with no enum to extend -- so
the provisioning registry is instead a plain `attrsOf provisioningAdapter`
(`nixdeploy.publisher.provisioning`) that any number of operator-chosen provider names can
populate, and `provisioning-generic.nix`'s `mkAdapter` exists to make populating one entry of
it cheap. Nothing about either shape is required by the other -- provisioning could in
principle have been a closed enum too, and activation an open attrset -- this is just what
`modules/default.nix`'s own option types already commit to.

## The activation registry, in this directory

An activation adapter is a NixOS-style module that does two things, gated so both only fire
under its own backend: it SETS `nixdeploy.receiver.activation` (the five verbs
`activationAdapter` declares in `modules/default.nix`), and it APPLIES the two
configuration-valued ones.

```nix
{ config, lib, pkgs, ... }:
let
  cfg = config.nixdeploy;
  forward = (import ./apply.nix { inherit lib; }).forward {
    adapter = "my-backend";
    trees = [ "systemd" ];  # every option tree this backend may be written into
  };
in
{
  config = lib.mkIf (cfg.backend == "my-backend") (lib.mkMerge [
    {
      nixdeploy.receiver.activation = {
        activate = "..."; # a command; receives the target store path as its one argument
        currentPath = "..."; # a command; prints the running store path on stdout, nothing else
        rollback = null; # or a command; null is a legitimate, documented answer -- see below
        schedule = { name, description, argv, intervalSeconds }: { /* a config fragment */ };
        nixSettings = { httpConnections, downloadBufferSize }: { /* a config fragment */ };
      };
    }

    (lib.mkIf cfg.receiver.enable (lib.mkMerge [
      (forward "schedule" (cfg.receiver.activation.schedule cfg.receiver.job))
      (forward "nixSettings" (cfg.receiver.activation.nixSettings {
        inherit (cfg.receiver) httpConnections downloadBufferSize;
      }))
    ]))
  ]);
}
```

The second half is boilerplate, but it is not boilerplate that could be hoisted into
`modules/default.nix`. **Read `apply.nix` before writing it**: the module system collects
which option NAMES each module defines before `config` exists, so a module whose config is a
fragment read out of `config` deadlocks -- and the fix, naming the option trees statically,
can only be done by a file that knows which backend it is. That is the same fact the registry
itself exists for, arriving from a different direction.

Note that the application reads `cfg.receiver.activation.schedule`, not the local function the
same file just assigned to it. That is deliberate: what the adapter sets is a DEFAULT, and an
operator who overrides `nixdeploy.receiver.activation.schedule` (to spread a fleet's ticks, to
schedule through something else entirely) must get their version applied rather than this
file's.

The `cfg.backend ==` guard is not decoration: a plain attrset assignment (not `mkDefault`)
means two adapter files imported into the same evaluation by mistake -- the wrong one for
this machine's actual backend, or genuinely two at once -- fail loudly with Nix's own
"conflicting definitions" error, rather than one silently winning. Every file in this
directory follows that same shape; `nixos.nix` is the most heavily commented of the three
and the one to read first, because the exit-code ambiguity it works around
(`activationAdapter.activate`'s own "if and only if" requirement, and why a switch command's
own exit code cannot be trusted to mean that) is the reasoning the other two reuse rather
than re-derive.

**`rollback` is allowed to be `null`, but only honestly.** `modules/default.nix` documents
this as "the receiver then reports a failed activation it could not undo, rather than
pretending it did" -- a real, supported outcome, not a stub. What is not acceptable is
inventing a rollback command for a backend where no real mechanism was actually verified to
exist: a `rollback` that silently no-ops, or one that runs something plausible-looking
without confirming it undoes anything, is worse than `null`, because `null` at least reports
honestly. Every `rollback` in this directory's three files is a real, cited mechanism (an
ordinary `nix-env --rollback` against the same profile path the backend's own official
rebuild tool uses) -- read straight from that backend's own upstream source, not assumed.

## Adding a new backend

Say a fourth Nix module system exists (`my-backend`, standing in for something real) and you
want a machine built by it to receive closures the same way. Three things change, and only
three:

1. **Widen `nixdeploy.backend`'s enum in `modules/default.nix`** to include `"my-backend"`.
   This is the one place adding an activation adapter is NOT free -- `backend` is a closed
   `types.enum`, deliberately (see `modules/default.nix`'s own description: "stated by the
   caller rather than detected," because a module cannot probe for a backend-specific
   primitive without becoming unloadable under the other backends). Extending a closed enum
   by one value is the smallest change that could satisfy that constraint; it is not
   equivalent to teaching the option surface a new primitive.
2. **Write `modules/adapters/my-backend.nix`**, shaped exactly like the skeleton above,
   implementing `activate`/`currentPath`/`rollback` against whatever `my-backend`'s own
   generation and activation mechanism actually is, plus `schedule` and `nixSettings` against
   whatever it uses to run something repeatedly and to configure Nix. If it has systemd, the
   first of those is already written: `systemd-scheduling.nix` here is shared by `nixos.nix`
   and `system-manager.nix`. If its machines own their own `nix.conf`, so is the second:
   `nix-conf.nix`. If a machine on this backend owns neither, say so the way
   `system-manager.nix` does -- by throwing, not by accepting the option and dropping it.
3. **Export it** from `flake.nix`, as `<thatModuleSystem>Modules.backendAdapter` and as a
   `backendAdapters.<name>` entry, so an operator can reach it without an import-by-store-path.

Nothing else needs to change, and specifically **the engine does not**: `src/activate.rs`
(the receiver binary that actually runs these three commands) never names a backend
anywhere in its own source -- it reads `activation.activate`/`.currentPath`/`.rollback` as
plain strings out of its JSON config and shells them out, tokenized, exactly the same way
regardless of which file produced them. `src/manifest.rs`, `src/delta.rs` and `src/outcome.rs`
are equally unaware. A correct new adapter file is indistinguishable, from the engine's
point of view, from one of the three already in this directory -- which is the entire point
of the registry existing as a registry rather than a chain of `if backend == ...` branches
inside the engine itself.

## What a "small" adapter still has to get right

Every adapter in this directory does more than a naive reading of `activationAdapter` might
suggest is necessary, and both extras are load-bearing, not caution for its own sake:

- **`activate`'s exit code is never trusted on its own.** Every `activate`/`rollback` script
  here re-reads `currentPath` after running the underlying tool and decides success or
  failure from THAT, discarding the tool's own exit code as a diagnostic detail only. This
  mirrors what `src/activate.rs` also does at the engine layer (see its own module doc) --
  the two are independent, deliberately: `activationAdapter.activate`'s contract in
  `modules/default.nix` is stated as belonging to the command itself ("must exit non-zero if
  and only if..."), not merely to whatever happens to be the engine's current behaviour, so
  each adapter honours it on its own terms rather than leaning on the caller to paper over a
  command that doesn't.
- **`currentPath` must always succeed with non-empty output, including on a machine that has
  never been activated even once.** `src/activate.rs`'s `run_capturing` -- the only caller of
  this command -- treats a non-zero exit or empty trimmed stdout as a hard error that aborts
  the entire run, and `src/receive.rs` calls it BEFORE deciding whether a machine needs to
  activate at all. A `currentPath` that errors on a fresh machine would make that machine
  permanently unable to ever converge for the first time. Every adapter here prints a fixed
  sentinel (`nixdeploy-uninitialized`) instead of erroring when nothing has been registered
  yet -- a string guaranteed to never equal a real `/nix/store/...` path, so the ordinary
  "does current match target" comparison still does the right thing.
