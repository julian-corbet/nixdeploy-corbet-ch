# checks/default.nix
#
# One kind of test lives here so far: EVAL-TIME assertion tests (checks/assertions.nix). Each
# evaluates a real configuration through NixOS's own eval-config.nix and asks whether forcing
# `system.build.toplevel` fails. Nothing here runs a generated script or boots anything --
# there is no runtime component to prove yet (the receiver/publisher binaries and the adapter
# registries are still being written elsewhere in this repo), so the module's option surface
# and its `assertions` are the whole surface these checks can hold accountable today.
{ pkgs, lib, nixpkgs, system, nixdeployModule }:

let
  results = import ./assertions.nix {
    inherit pkgs lib nixpkgs system nixdeployModule;
  };

  failed = builtins.filter (r: !r.ok) results;
  report = lib.concatMapStringsSep "\n" (r: "  - ${r.name}: ${r.detail}") failed;
in
{
  eval-tests =
    if failed != [ ]
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
