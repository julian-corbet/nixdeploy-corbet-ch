# The provisioning-adapter contract

A **provisioning adapter** answers one question: *how does this host materialise one exact
signed boot role*, when the change is too large to apply in place and the host is being
replaced wholesale instead of switched (see `docs/reimage.md` for why that path exists).

**Nothing in this repo calls a publisher-side provisioning adapter.**
`nixdeploy.publisher.provisioning` is read by no module and no code path; the scheduled
publisher only commits a signed static manifest. The distinct on-target route is real:
`nixdeploy.receiver.reimage` reaches the receiver config and `src/receive.rs` calls it after
an over-ceiling refusal. What follows is the off-target contract, not a description of a
controller currently running -- see `docs/reimage.md` for the exact split.

Read `modules/publisher.nix`'s own `provisioningAdapter` submodule first -- this file
restates its contract in prose and shows how to satisfy it, but the submodule's option
descriptions are the authoritative source. In short:

- **`reimage`** -- a command line. Receives exactly three arguments: the role, its signed
  nixboot artifact store path, and its signed provider image reference.
  Specified to run off the machine being replaced, because a machine cannot reliably
  participate in its own replacement and requiring it to be reachable would reintroduce
  the dependency reimaging exists to remove (see `docs/reimage.md`, "Off-target recovery is
  still absent"). The only reimage code that exists today runs the opposite way -- on the
  target, from `src/receive.rs` -- and it is configured through `nixdeploy.receiver.reimage`.
- **`imageRef`** -- a command line printing the image reference this machine currently
  runs from, or `null` if the provider cannot report that. Read by nothing; convergence is
  judged from `currentPath` alone in every case.
- Adapters are registered in `nixdeploy.publisher.provisioning`, an `attrsOf
  provisioningAdapter` **keyed by provider**, and each host states which provider it uses
  via its own `nixdeploy.provider`. The current publisher-side registry has no caller, so
  neither a present nor an absent entry causes a provider mutation today. The implemented
  on-target path is configured separately through `nixdeploy.receiver.reimage` and returns
  a typed refusal or failure instead of silently doing nothing.

## Granularity: what "keyed by provider" actually lets you do

`provider` is `types.nullOr types.str`, "in the operator's own vocabulary" -- not an enum
of known clouds. Nothing about the option surface requires one entry per cloud account, or
even per cloud. Register `nixdeploy.publisher.provisioning` at whatever grain makes a
single `reimage` command line unambiguous about which machine it targets:

- **Coarse** -- one entry for an entire account/project, if your `reimage` command line
  can determine which specific machine to act on from context it already has (an
  inventory lookup keyed by hostname, a tag already applied to the instance, whatever your
  own tooling already tracks).
- **Fine** -- one entry per machine, if it can't. There is nothing wrong with
  `nixdeploy.provider = "host-a"` on one machine and `nixdeploy.provider = "host-b"` on
  another, each pointing at its own `nixdeploy.publisher.provisioning` entry whose
  `reimage` command is already specific to that one machine. This is often the simplest
  correct answer, and it costs nothing extra: the attrset is unbounded and machine count
  was never meant to be a scaling concern here (see the top-level README's "pull is the
  floor" section for why machine count costs the publisher nothing generally).

This factory has no opinion on which you pick. Pick whichever makes the underlying CLI or
IaC invocation actually correct, and let that decide the name.

## Two ways to satisfy the contract

### 1. `provisioning-generic.nix` -- wrap an existing CLI or IaC invocation

Most providers already have a command that does the right thing: a cloud CLI subcommand
that swaps a boot volume or replaces an instance from an image, an `OpenTofu apply` against
a machine resource pinned to a new image variable, a local script that already talks to
your hypervisor. `provisioning-generic.nix`'s `mkAdapter` turns "a shell command that
already does the right thing" into the exact shape `provisioningAdapter` needs --
argument-count enforcement, a self-contained `PATH` via `runtimeInputs`, and safe
interpolation of any non-secret configuration values the command needs. Read that file's
own header comment for the full contract of `mkAdapter`'s arguments; this is the shape of
using it, against a placeholder CLI (`cloudctl`, an invented name -- not a real product) and
a placeholder host, so nothing below names anything about any real operator's
infrastructure:

```nix
{ lib, pkgs, ... }:

let
  provisioningGeneric = import <nixdeploy>/modules/adapters/provisioning-generic.nix {
    inherit lib pkgs;
  };
in
{
  nixdeploy.publisher.provisioning."host-a" = provisioningGeneric.mkAdapter {
    name = "host-a";
    runtimeInputs = [ pkgs.cloudctl ]; # a placeholder package; use your provider's real CLI

    # $1/$BOOT_ROLE is the role, $2/$BOOT_ARTIFACT is its exact store artifact, and
    # $3/$IMAGE_REF is its provider reference. cloudctl is invented syntax for illustration.
    reimageCommand = ''
      cloudctl instances replace-boot-image \
        --instance host-a.example.org \
        --image "$IMAGE_REF" \
        --wait
    '';

    # Omit imageRefCommand entirely for a provider that can't report this; convergence is
    # then judged from currentPath alone, which is a legitimate, documented answer (see
    # provisioningAdapter.imageRef's own description in modules/default.nix).
    imageRefCommand = ''
      cloudctl instances describe host-a.example.org --output-field=bootImage
    '';

    # Plain configuration, not a credential -- see mkAdapter's own comment for exactly why a
    # secret must never land here. A credential path is fine (the command reads it itself);
    # the credential's contents are not.
    environment = {
      CLOUDCTL_API_ENDPOINT = "https://api.example.org";
    };
  };
}
```

### 2. A bespoke adapter module -- when one shell command isn't enough

`mkAdapter` fits a provider whose replace-the-machine operation is genuinely one
command (even a long one, with retries and polling baked into that one command's own
script body). It does not fit a provider whose reimaging needs its own state -- rate
limiting across attempts, a multi-step orchestration with its own systemd units, a health
poll loop with hysteresis. For that shape, write a real NixOS module the same way this
family's other adapter-contributing modules do: own your own option namespace, build
whatever systemd units or scripts you need, and set

```nix
nixdeploy.publisher.provisioning.<name> = { reimage = "..."; imageRef = "..."; };
```

from inside it, via ordinary Nix attrset assignment -- exactly the value shape
`provisioningAdapter` expects, built however you need to build it. Nothing about the
`provisioningAdapter` submodule requires the value to come from `mkAdapter`; the factory
is a convenience for the common case, not the only legal way to populate the registry.

## What neither shape does for you

Neither `mkAdapter` nor a bespoke adapter module is asked to decide **when** a machine
should be reimaged, or to report the typed outcome of having done so
(`Reimaged { role, artifact, image }`, per the top-level README's outcome list) -- that
decision and that reporting belong to
whatever invokes `reimage`, using the receiver's own sizing of the change against its own
store (`maxInplaceDeltaBytes`, judged from narinfo metadata -- see `modules/default.nix`).
An adapter's own contract stops at "the provider accepted this exact role/artifact/image
request, or this command exits non-zero" -- keep it that narrow. An adapter that tries to also decide
policy about *when* to run duplicates a decision that belongs to the receiver, which is
the only component that can see whether that decision was actually correct for its own
store (see the top-level README's "Why the receiver decides").
