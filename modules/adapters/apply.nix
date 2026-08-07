# modules/adapters/apply.nix
#
# WHY THE ADAPTERS APPLY THE TWO CONFIGURATION VERBS, AND `modules/default.nix` DOES NOT
#
# `modules/default.nix` declares five verbs on `nixdeploy.receiver.activation`. Three of them
# (`activate`, `currentPath`, `rollback`) are command lines it transcribes into a JSON file;
# the other two (`schedule`, `nixSettings`) are functions returning CONFIGURATION, and it is
# the adapter files -- not `modules/default.nix` -- that call them and splice the result in.
# That is forced by the module system, and the failure is worth stating exactly, because the
# obvious arrangement looks like it should work and does not:
#
#   A module's `config` is merged by first collecting, for every module, which OPTION NAMES it
#   defines. That collection happens BEFORE `config` itself exists. So a module whose config
#   is `mkMerge [ (config.some.option arg) ]` deadlocks: discovering which names that fragment
#   defines requires forcing it, forcing it requires reading `config.some.option`, and reading
#   any option requires the very name collection that is still in progress. Nix reports it as
#   "infinite recursion encountered ... while evaluating the module argument `config'".
#
# The cycle breaks the moment the top-level names are STATIC and only the values read
# `config`. `forward` below is that: `lib.genAttrs` over a literal list of option trees, whose
# names are known without evaluating anything, wrapping values that are lazy. And only a
# BACKEND ADAPTER can supply that literal list, because the list is exactly "which option
# trees does this backend's module system have" -- `systemd` on NixOS and system-manager,
# `launchd` on nix-darwin. Naming either in `modules/default.nix` is precisely the
# backend-specific primitive that file must never contain, which is the same reason the verbs
# are adapter-shaped in the first place.
#
# So the split is not a workaround for the module system, it is the module system agreeing
# with the design: the file that must stay loadable under four backends cannot name any one
# backend's option tree, and the file that already knows which backend it is can.
#
# WHY THIS CHECKS FOR TREES IT WAS NOT ASKED TO FORWARD
#
# `nixdeploy.receiver.activation.schedule` is an ordinary option, so an operator can replace
# it -- to add a `RandomizedDelaySec` across a fleet, to schedule through something other than
# a timer. A replacement that returns a fragment naming a tree this adapter does not forward
# would otherwise be dropped in silence, and a silently-dropped half of a scheduling
# definition is the worst kind of wrong: the unit exists, so nothing looks broken, and the
# part that was dropped is discovered from the machine's behaviour rather than from a build.
{ lib }:

{
  # forward :: { adapter :: str, trees :: [str] } -> str -> attrs -> attrs
  #
  # Curried on purpose: an adapter binds the first two once, at the top of its own file, and
  # then forwards each verb's fragment through the result. `trees` is that adapter's complete,
  # literal answer to "which option trees may this backend be written into", so every name in
  # it exists on that backend by construction.
  #
  # Every tree is emitted on EVERY call, as `{ }` where the fragment does not mention it. An
  # empty definition of an option that exists is a no-op, and emitting it unconditionally buys
  # two things: the returned attrset's shape stops depending on what the fragment happens to
  # contain (which is what keeps its names static, above), and every verb's fragment gets
  # FORCED by something -- including a verb whose entire job is to refuse, whose throw would
  # otherwise be a refusal nobody ever received. `system-manager.nix`'s `nixSettings` is
  # exactly that verb.
  forward = { adapter, trees }: verb: fragment:
    let
      unknown = lib.subtractLists trees (builtins.attrNames fragment);

      # A partially-applied `throwIf`, wrapped around each VALUE rather than around the
      # `genAttrs` itself. Both bindings here are lazy, which is the whole point: deciding
      # whether to throw requires `builtins.attrNames fragment`, and forcing the fragment is
      # exactly what must not happen while the module system is still collecting names. Inside
      # a value it is safe -- by the time anything reads `config.systemd`, `config` exists.
      refuseUnknownTrees = lib.throwIf (unknown != [ ]) ''
        nixdeploy: the "${verb}" verb configured for backend adapter "${adapter}" returned a
        configuration fragment naming ${lib.concatStringsSep ", " unknown}, but this adapter
        forwards only ${lib.concatStringsSep ", " trees}.

        Anything outside that list would be dropped without a word, so it is refused here
        instead. Either emit into ${lib.concatStringsSep ", " trees} only, or -- if this
        backend genuinely needs another option tree -- widen the `trees` list in
        modules/adapters/${adapter}.nix, which is the one place that knows what this backend
        has.
      '';
    in
    lib.genAttrs trees (tree: refuseUnknownTrees (fragment.${tree} or { }));
}
