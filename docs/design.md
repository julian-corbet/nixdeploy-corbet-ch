# Design notes

The reasoning behind the shape described in the main [README](../README.md),
stated at the length the README deliberately does not spend. Open questions
that are reasoned rather than measured live in
[`../experiments/README.md`](../experiments/README.md); anything that closed
against a real run lives in [`../studies/`](../studies/README.md).

## Why a host contains named planes

A host is not one activation target. A foreign-distro workstation can have a
system-manager system closure and a home-manager generation, while a NixOS or
nix-darwin host has its own system plane. These targets are built, become stale, activate,
and roll back independently. Collapsing them into one host-level path either leaves user
configuration undeployed or pretends several activation mechanisms are one transaction.

The signed schema therefore maps `host -> planes -> target`. Version 2 has four canonical
plane names, each equal to its explicit backend: `nixos`, `system-manager`, `home-manager`,
and `nix-darwin`. Home-manager alone requires an `identity`; version 2 supports one such
identity per host. The receiver cross-checks the configured name, backend, and identity
against the signed leaf before it invokes any adapter. Images are limited to NixOS
whole-machine planes because an image cannot meaningfully replace a user profile or one
system-manager slice.

Publisher selectors are two independent axes: host names and plane names. Supplying both
selects their intersection. A partial publication is always merged into a complete base
manifest, so precision affects only which immutable targets change; it never makes the
unselected targets disappear.

## Why the receiver decides, not a controller

The obvious way to build this is a controller: one component that watches
every machine's actual state, compares it to what the fleet's manifests say
it should be, and pushes the difference. That shape has an unavoidable
property — the controller must be correct, must be reachable, and must be
holding accurate state about a machine *before* that machine can converge at
all. Every one of those is a way for a fully healthy machine to sit un-converged
for a reason that has nothing to do with that machine.

The receiver is instead the only component asked to decide anything about
its own machine, because it is the only component that *can* observe that
machine — its actual store contents, its actual running closure, its actual
free memory right now. A controller answering the same question is reasoning
from a copy: a record of what a machine last reported, refreshed on
whatever cadence the controller polls at, and wrong for exactly as long as
that cadence allows. Moving the decision onto the machine being decided
about does not remove the possibility of being wrong; it removes the
possibility of being wrong *about someone else's disk*.

## Why there is no controller at all

Deciding on the receiver only answers half the question. The other half is
why nothing centralizes even the parts a controller would still be a
reasonable place to put — dispatch, scheduling, retry — and the answer is
the same property stated more sharply: a component that everything else
depends on for correctness is a component whose own failure mode is
"nothing converges," and that failure mode is worst exactly when it matters
most. A controller that is down, partitioned, or wrong about one machine's
state does not fail quietly for that one machine; it is a single point of
failure sitting in front of every machine's own path back to health,
including the controller's own infrastructure if that infrastructure is
itself managed by this repo.

Removing the controller does not remove scheduling and retry as concerns —
it distributes them. Each receiver schedules its own check (`receiver.interval`,
[modules/default.nix](../modules/default.nix)), retries on its own next tick
with no coordination required. Ordinary transient failures rerun the same convergence path.
There is one deliberate exception: after a candidate actually activates, fails a health gate,
and successfully rolls back, repeating the same activation is no longer a retry — it is a
known poison loop. The receiver persists that immutable store path beneath its declared
`stateDirectory` and returns the typed `RejectedTarget` failure stage on later ticks before
delta sizing or activation. A different published store path proceeds normally and clears the
stale pin once it passes health; retrying the exact same immutable closure before then requires
an operator to remove the plane-scoped pin explicitly.
There is still no separate controller-side reconciliation loop to keep correct alongside the
main one.

## Why pull is the floor and push would only be an accelerator

This repo ships no push mechanism at all: `receiver.interval` and the unit the
backend adapter schedules from it are the whole of delivery. The argument below
is why that is sufficient rather than a missing feature.


A push-only design ties delivery to reachability: the publisher must be able
to reach the machine right now, over whatever network exists right now, for
anything to happen. That is precisely backwards for a delivery system, because
network configuration is itself something this system might need to deliver —
a firewall rule, an overlay client, a routing table. A delivery mechanism
that depends on the network it is sometimes asked to fix cannot fix the
thing that made it unreachable, by construction, no matter how carefully it
is implemented.

Pull inverts the dependency. Every managed machine already knows how to get
from "whatever it is running now" to "whatever the manifest says," on its
own schedule, with no publisher involved in the loop at all. A push is
demoted to what it actually is: a request to check *sooner* than the next
scheduled tick, delivered on a best-effort basis over whatever channel
happens to be reachable at that moment. If the push never arrives, or
arrives at a machine that has lost every route to everywhere, nothing is
lost — the interval was always going to fire, and it still will. The push
exists purely to trade latency for convenience on the common case where
reachability isn't the problem; it is never load-bearing for the case where
it is.

This also means machine count costs the publisher nothing beyond the cache
storage and manifest size — no persistent connection, no fleet of
outstanding pushes to track the delivery state of, no accounting for which
machines have and have not yet received something the publisher is
responsible for confirming.

## Why the ceiling is enforced receiver-side

A size ceiling exists because a small machine can be destroyed by activating
a large change in place — parallel NAR decompression, download buffers,
store registration, and the mass unit restart that follows activation all
peak in memory at roughly the same moment, and a machine with little to
spare does not survive that peak just because the change was "supposed" to
be small.

Enforcing that ceiling anywhere other than the receiver means enforcing it
against a *model* of the receiver's store rather than the store itself — a
record kept by the publisher, or a controller, of what this machine last
reported having. That model is wrong in exactly the situations where the
ceiling matters most: after an unclean run left the store in an unknown
state, after a garbage collection reclaimed paths the model still believes
are present, or after a restore from backup put the machine somewhere the
model never saw. A sender using that model to decide "this change is small
enough" is answering a question about a machine that may no longer exist in
the form the model describes.

The receiver instead sizes the change against its own store directly, from
`.narinfo` metadata for the paths it is missing — no download, no
evaluation, just the sizes the cache already publishes for each path. This
is cheap enough to run on every check, and it is the one measurement that
cannot be stale, because it is taken against the store it will actually be
applied to, at the moment it is about to be applied. When the sum is over
`receiver.maxInplaceDeltaBytes`, the receiver refuses — loudly, with the
numbers that produced the refusal — rather than guessing that it will
probably be fine.

## Why outcomes are typed

A run that changes nothing and a run that fails have to be distinguishable
from a run that succeeds, and from each other, in the return value itself —
not recoverable later from a log line, an exit code overloaded to mean two
things, or the absence of an error. `AlreadyCurrent` and `Converged` are
both healthy, wanted outcomes and neither one is "nothing happened, which is
fine" collapsed into the other; a fleet where every machine reports
`AlreadyCurrent` forever, because a manifest URL typo means nothing has ever
actually been fetched, must be visibly different from a fleet that
genuinely has nothing new to converge to. A system that only has "success"
and "not success" cannot report that difference, because there is no value
in its vocabulary that means it.

`Refused` is the sharpest case of this. A refusal is the receiver's size
guard doing exactly its job — the *correct* response to a change that would
not survive activation — and reporting it as a failure would train whoever
reads these outcomes to treat a working safety mechanism as an incident,
which is how that mechanism gets disabled the first time it becomes
inconvenient. `Refused` carries the bytes and the ceiling that produced the
decision, so the report is actionable (raise the ceiling deliberately, or
accept that this machine reimages) rather than merely reassuring.

`Failed` is the one outcome that names *where* things went wrong
(`{ stage, detail }`) rather than just that they did, because the module
surface this repo defines is itself built around a distinction that erases
easily if outcomes don't carry it: a backend whose `activate` command exits
non-zero because some unrelated unit failed, while the configuration it was
given actually applied, must not be indistinguishable from an `activate`
that exited zero having applied nothing at all
(see `receiver.activation.activate`'s own description in
[modules/default.nix](../modules/default.nix) for the full statement of that
contract). A typed `Failed{ stage }` is what lets that disambiguation be
visible in the outcome, instead of being a fact only the adapter author knew
while writing it.

## The ratchet hazard

Every one of the properties above rests on one path staying open: the
in-place activation path is only reachable while a machine's own delta
against the manifest is under its ceiling. A machine that drifts past that
ceiling — an unclean run, a long stretch offline, a ceiling lowered after
the fact — stops being eligible for the path this repo spends most of its
design on, and becomes eligible for exactly one other path: reimage.

That makes the reimage path a ratchet, not a fallback. A machine can cross
from "in-place works" to "in-place refuses" in one direction only, because
crossing back requires either the machine shrinking its own delta on its
own (which a machine too far behind to activate in place generally cannot
do, for the same memory reasons the ceiling exists) or the reimage path
actually working. If that path is broken, untested, or simply
absent — no provisioning adapter registered for this machine's provider,
a `reimage` command that has silently stopped working, a provider that was
declared and never actually wired up — the machine does not get a second
chance at the in-place path. It sits refused, forever, because the *only*
other route was the one nobody exercised.

The on-target reimage path exists in the receiver binary (`src/receive.rs`'s
`route_over_ceiling`) and `nixdeploy.receiver.reimage` reaches its rendered config. The
separate off-target recovery path still has no caller: nothing reads
`nixdeploy.publisher.provisioning`, so a machine too broken to run its receiver has nowhere
automatic to route the refusal. See
[`reimage.md`](reimage.md)'s "What is implemented" for the exact split.

The module surface enforces the narrowest version of this it can check for
free: `receiver.enable && maxInplaceDeltaBytes != null` asserts
`provider != null`, because a ceiling with no declared provider means a
refusal that could not be routed to any reimage adapter at all
(see the assertions in [modules/default.nix](../modules/default.nix), and
the tests that prove both the refusal and its absence in
[`../checks/assertions.nix`](../checks/assertions.nix)). That assertion is
necessary and it is not sufficient. It proves a provider name exists; it
proves nothing about whether `publisher.provisioning.<that provider>.reimage`
is actually implemented, actually reachable, or actually exercised before
the day a real drifted machine needs it. A reimage path first exercised
under pressure, on the one machine that has already proven it cannot take
the easier path, is the worst possible time to discover it does not work.
The conclusion this repo draws from that is not a feature this module
surface can enforce by itself: reimage has to be treated as a first-class,
regularly-exercised operation for any provider it is registered against —
not the thing that only ever runs once, unattended, against whichever
machine happened to drift the furthest.
