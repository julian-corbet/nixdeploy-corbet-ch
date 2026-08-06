# Why reimage exists, and its one honest limitation

This is the reasoning document for the `Reimaged { image }` outcome and the
`provisioningAdapter` registry (`modules/publisher.nix`). It does not name any operator's
infrastructure -- every example below is a placeholder (`example.org`, `host-a`) precisely
so this document stays true regardless of which cloud, hypervisor or bare-metal fleet reads
it.

## What is implemented, and what is only specified

Read this first, because the rest of this document is reasoning about a delivery mode that
is only half built.

**Implemented, receiver-side.** `src/receive.rs`'s `route_over_ceiling` runs on the machine
being replaced. When a delta comes back over `maxInplaceDeltaBytes`, it records the refusal
to whatever metrics sinks are configured -- first, because what follows may end the process
-- and then, if its config file names a `reimage` command AND the manifest names an image
for this host, runs that command with the image as its single argument. It returns
`Reimaged { image }`, which claims only that a replacement was requested and the request was
accepted. The image comes only from the selected NixOS plane; system-manager, home-manager,
and nix-darwin planes cannot name one. With no command configured it refuses and stops,
which is a complete and correct answer.

**Specified, with no off-target caller.** `nixdeploy.publisher.provisioning` is an `attrsOf
provisioningAdapter` that the scheduled manifest publisher deliberately does not read: a
static-file writer has no authority to replace machines. A configured
`nixdeploy.receiver.reimage` does reach the receiver's JSON and makes the receiver-side route
above real. What remains absent is the separate controller that could consume the publisher
registry after a refusal when the target is too broken to run its own receiver. `imageRef`
still has no reader anywhere.

So the "needs nothing from the target" property argued for below is still a property of the
DESIGN, not of any code here: the receiver-side route needs the target healthy enough to run
its receiver. A machine too wedged for that is not covered by anything implemented here.

## The memory argument: why reimage exists as a delivery mode at all

Applying a large change in place on a healthy, adequately-sized machine is unremarkable:
fetch the new store paths, register them, restart the units that changed. On a *small*
machine, the same operation is not four sequential steps with modest peaks -- it is four
things that all want memory **at the same moment**. Substituter connections hold
decompression state for as long as they are open, and several run concurrently by
default. Downloaded NARs sit in buffers before they are unpacked. Newly-fetched paths get
registered into the store database. And the moment activation actually switches over, a
mass unit restart can bring up several services' own peak working sets simultaneously,
often overlapping the tail of the fetch/register work that triggered it in the first
place. None of these steps is expensive alone. Their **union**, at their shared peak, is
what a machine with little RAM to spare cannot always survive.

This is exactly what `nixdeploy.receiver.maxInplaceDeltaBytes` exists to bound (see
`modules/default.nix`'s own description: "this bounds concurrent peak memory during
fetch, decompression, store registration and the unit restarts that follow, on a machine
that may have very little"). But a byte ceiling on the delta only prevents the machine
from being asked to survive a change too large for it -- it does not make the change
happen. Something has to happen instead. Reimaging is that something: replace the
machine wholesale, at the provider, from a prebuilt image that already contains the
target closure, rather than asking the machine's own limited memory to survive the
journey from what it is running now to what it should run next.

**When in doubt, replace.** A machine that gets OOM-killed mid-activation is not a
machine that failed safely -- it is a machine that may come back with a half-registered
store, a partially-restarted unit set, or not come back at all without hands-on
intervention. A machine that got reimaged is, by construction, either running the new
image cleanly or still running the old one; there is no state in between that a reimage
can leave behind, because the machine that would have to occupy that state was replaced,
not edited.

## The recovery floor the design aims at -- not yet the one it has

Every way this repo currently gets a machine onto a new closure needs something from the
machine itself: a running receiver process, enough store to hold an overlay of new and
old paths side by side, and a network path back to wherever the manifest and cache live.
The receiver-side reimage route is no exception -- it is a command the receiver runs, so a
machine that cannot run its receiver cannot take it.

The design the `provisioningAdapter` registry is specified for needs **none of that**: a
`reimage` command invoked from somewhere other than the target, whose target is not asked to
do anything at all -- not run a receiver, not accept an SSH connection, not even be
reachable. The provider replaces the machine (a new instance from an image, or a boot volume
swap) the same way it would for a machine that was never running nixdeploy's receiver in the
first place.

That is what would make reimaging a recovery floor: a path that still works when everything
else on the target is broken, including a target that cannot be reached at all, that has
no working init system left to run a receiver under, or whose store is corrupt beyond
anything an in-place operation could repair. If a machine is wedged badly enough that
nothing on it can be trusted to run correctly, asking it to participate in its own repair
is asking the broken thing to fix itself. **No code in this repo does that yet** -- the
registry is declared and has no caller -- so today a wedged machine's recovery floor is a
human with out-of-band access.

## The circular dependency it would break

A delivery mechanism that itself depends on the network reaching its own target cannot be
the mechanism that delivers a fix for that same network -- if the fix is "the network
configuration was wrong," then reaching the machine to deliver the fix requires the
network configuration to already be right, which is the thing that needs fixing. This is
not a hypothetical: network configuration is itself something this kind of system
deploys, the same as any other part of a machine's closure, which means it is entirely
possible to ship a change that breaks the very path a pull-based receiver would use to
notice there is a better closure available, or that a push-based accelerator would use to
reach the machine at all.

A reimage invoked from off the target breaks that circularity by not routing through the
broken thing. It goes through the provider's own control plane instead -- a plane that is,
by construction,
independent of whatever this machine's own network stack currently believes about itself.
A machine that broke its own routing table, its own firewall, or its own overlay client
config is still reachable at the layer *below* all of that: the provider that can replace
the machine wholesale never has to ask the machine's own (possibly broken) network stack
for permission first.

## The honest limitation: an installer that boots into RAM still needs RAM

There is a well-known alternative shape for "reinstall a machine's operating system
without touching it by hand": kexec into a memory-resident installer environment, then
have that installer environment partition, format and populate disk from inside itself,
entirely from RAM, before handing off to the newly-installed system. `nixos-anywhere` is
the best-known implementation of this idea for NixOS, and its own documentation is
explicit about the tradeoff: because the entire installer environment -- root filesystem
included -- lives in RAM for the duration of the install, a machine with very little
memory can run out of it partway through, before the install ever completes.

State this plainly, because it is a real trap and not a corner case: the machines an
operator most wants a floor-level recovery path for are disproportionately the *same*
machines this failure mode targets. A machine small enough that `maxInplaceDeltaBytes`
matters at all -- small enough that an ordinary in-place activation can threaten it -- is
also small enough that a kexec-into-RAM installer can threaten it, for the identical
underlying reason: not enough memory to hold everything the operation needs
simultaneously. Reaching for a RAM-resident reinstall as "the safe fallback for a small
machine" can therefore fail for exactly the same root cause the fallback was chosen to
avoid.

This is why `provisioningAdapter.reimage` is specified the way it is: replacement at the
**provider** level -- a new instance created from a prebuilt image, or an existing
machine's boot volume swapped for one -- rather than an in-place reinstall driven from
inside the target. A provider-level replacement never asks the target's own RAM to hold
an installer, because the target's own RAM is never asked to do anything; the new image
is already complete before the machine that will run it exists (or before the boot volume
that will be attached to it is swapped in). This is not a smaller version of the same
technique with the memory problem tuned away -- it is a different technique that does not
have the memory problem in the first place, which is exactly why it is the one this
repo's contract asks a provisioning adapter to implement.

## A provider with no adapter is a terminal refusal worth reporting

`nixdeploy.publisher.provisioning` is an `attrsOf provisioningAdapter`, and nothing
requires every provider a machine might declare to have an entry in it. When a machine's
own `nixdeploy.provider` names something absent from that attrset, and that machine needs
reimaging -- because its receiver refused an in-place change as over its
`maxInplaceDeltaBytes` ceiling, or because it is wedged badly enough that only the
recovery floor above could reach it at all -- there is no fallback to slide to. The refusal
is **terminal**.

Today it is terminal for every machine, whether or not the provider has an entry, because
nothing reads that attrset (see "What is implemented" at the top). The paragraph below
describes the state an operator has to design around either way.

Treat that as a state worth surfacing, not a state worth working around silently. A
machine that needed replacing and had nowhere to route that request is a machine that
stays on its current (refused, over-ceiling, or wedged) closure indefinitely, with no
further attempt scheduled, because there is no further attempt this repo's contract
defines. That is a legitimate answer for an operator to design around -- add the missing
provisioning adapter, lower the machine's `maxInplaceDeltaBytes` so fewer changes ever
need reimaging in the first place, or accept that this particular machine's recovery
floor is "a human with out-of-band access" -- but it is an answer that has to be *chosen*,
deliberately, rather than arrived at by a refusal nobody noticed. The same principle the
top-level README states for every other outcome this repo produces applies here too:
"did nothing" and "succeeded" are different values, and a refusal with nowhere left to go
is neither -- it is its own outcome, and it should be reported as one.
