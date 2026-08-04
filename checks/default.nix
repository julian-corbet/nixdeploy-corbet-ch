# checks/default.nix
#
# Three families of eval-time test, all reduced to one list of `{ name, ok, detail }` and one
# derivation, so `nix flake check` either passes or names every failure at once:
#
#   assertions.nix  what the option surface REFUSES, and what it DERIVES -- real NixOS
#                   eval-config, forcing `system.build.toplevel` so `config.assertions` is
#                   actually enforced, plus direct reads of derived options.
#   emission.nix    what the module PRODUCES -- a scheduled unit, a rendered config file and
#                   the Nix settings the memory ceilings land in, under each of the three
#                   backends with their real adapters composed.
#   lib.nix         the two PURE library files (`lib/delta.nix`, `lib/manifest.nix`), which
#                   are public flake outputs (`lib.narinfoDelta`, `lib.manifestSchema`) and
#                   were evaluated by nothing at all before this existed.
#
# Nothing here boots, activates or runs a generated script. The receiver binary's own
# behaviour is tested where it lives -- `cargo test`, run by `package.nix`'s checkPhase -- and
# duplicating that here would only prove Nix can start a process.
{ pkgs, lib, nixpkgs, system, nixdeployModule, backendAdapters }:

let
  results =
    import ./assertions.nix { inherit pkgs lib nixpkgs system nixdeployModule; }
    ++ import ./emission.nix { inherit pkgs lib nixpkgs system nixdeployModule backendAdapters; }
    ++ import ./lib.nix {
      inherit lib;
      narinfoDelta = import ../lib/delta.nix { inherit lib; };
      manifestSchema = import ../lib/manifest.nix { inherit lib; };
    };

  failed = builtins.filter (r: !r.ok) results;
  report = lib.concatMapStringsSep "\n" (r: "  - ${r.name}: ${r.detail}") failed;

  # Every check's `name` must be unique: two checks sharing one is how a rename that was
  # meant to add coverage quietly becomes a duplicate of something already tested, which is
  # exactly what this file's own history contained -- four of twelve checks were the same
  # expression under four names. A count comparison catches it for free.
  names = map (r: r.name) results;
  duplicated = lib.subtractLists (lib.unique names) names;
in
{
  eval-tests =
    if duplicated != [ ]
    then
      throw ''
        nixdeploy eval-tests: duplicate check names, which means two checks are asserting
        under one identity and one of them is invisible in this report:
        ${lib.concatMapStringsSep "\n" (n: "  - ${n}") (lib.unique duplicated)}
      ''
    else if failed != [ ]
    then
      throw ''
        nixdeploy eval-tests FAILED (${toString (builtins.length failed)}/${toString (builtins.length results)}):
        ${report}
      ''
    else
    # Depending on `passedCount` forces `results`, so the checks genuinely run under
    # `nix flake check` rather than merely being defined.
      pkgs.runCommand "nixdeploy-eval-tests"
        { passedCount = toString (builtins.length results); }
        ''
          echo "all $passedCount nixdeploy eval tests passed"
          touch $out
        '';
}
