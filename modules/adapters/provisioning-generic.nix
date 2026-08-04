# modules/adapters/provisioning-generic.nix
#
# WHY this file exists, and why it does not name a single cloud, hypervisor or IaC tool:
#
# `modules/default.nix`'s `provisioningAdapter` submodule asks for exactly two things --
# `reimage` (a command line, receiving the image reference as its one argument) and
# `imageRef` (a command line printing the image this machine currently runs from, or `null`
# where the provider cannot report that). Both are already just `types.str` / `types.nullOr
# types.str`: nothing in the option surface stops an operator from writing the command line
# by hand, once, per provider. What a hand-written command line gets wrong the first few
# times is everything AROUND the command itself: forgetting the "exactly one argument" half
# of the contract (a `reimage` command that silently reads an unset positional as an empty
# string reimages a machine against an empty image reference -- not a build error, a 3am
# incident), reaching for tools on whatever PATH happens to be ambient on the publisher host
# instead of a self-contained one, and interpolating operator-supplied values into a shell
# string without escaping them.
#
# `mkAdapter` below is that hand-written command line, done once, correctly, so an operator
# writing their tenth provider adapter pays the same near-zero cost as their first. It is the
# MECHANISM: a function from "a shell command that already does the right thing against your
# infrastructure" to a value shaped exactly like `provisioningAdapter`. Which CLI, which
# cloud, which account -- all of that is configuration an operator supplies at the call site,
# in their own, private configuration. This repo ships no defaults that would encode a
# choice among them, and this file itself never calls `mkAdapter` -- see
# `provisioning-README.md` for a worked example against a placeholder CLI, and for the
# alternative (writing a bespoke adapter file instead of using this factory) when a single
# shell command cannot express what a provider needs.
{ lib, pkgs }:

let
  # A `pkgs.writeShellApplication` `name` becomes a Nix store path component. Nix store paths
  # accept [A-Za-z0-9+._?=-], with no leading dot or dash -- reject anything that would not
  # survive that here, with a message that names the actual problem, rather than let a bad
  # `name` surface later as an opaque failure from deep inside nixpkgs' own derivation
  # machinery.
  validNamePattern = "[A-Za-z0-9][A-Za-z0-9._-]*";

  # Shell-identifier check for `environment` keys -- see `mkExports`'s own comment for why an
  # invalid key here is worth catching before any script is even built, not after.
  validEnvKeyPattern = "[A-Za-z_][A-Za-z0-9_]*";

  # Renders `environment` (plain, NON-SECRET configuration values -- see `mkAdapter`'s own
  # description for why a credential does not belong here) as `export KEY=value` lines, each
  # value individually shell-escaped so an operator-supplied value containing spaces, quotes
  # or shell metacharacters can never be read as anything other than one literal string.
  # Every key is checked against `validEnvKeyPattern` first: an invalid key would otherwise
  # land as literal, unescaped text in the generated script -- `export 3x=...` is not an
  # assignment, it is a syntax error the shell only reports at RUN time, on the publisher, in
  # the middle of a reimage. Catching it at Nix eval time instead is strictly better than
  # catching it there.
  mkExports = adapterName: environment:
    lib.concatStrings (lib.mapAttrsToList
      (k: v:
        if builtins.match validEnvKeyPattern k == null
        then
          throw ''
            nixdeploy provisioning-generic.mkAdapter (name = "${adapterName}"): environment key
            "${k}" is not a valid shell identifier (expected to match ${validEnvKeyPattern}).
          ''
        else "export ${k}=${lib.escapeShellArg v}\n")
      environment);
in
{
  # mkAdapter :: {
  #   name              : str,               # becomes the generated scripts' derivation name
  #   reimageCommand     : str,               # shell text; see below for what it may reference
  #   imageRefCommand    : null | str = null, # shell text; omit when the provider can't report this
  #   runtimeInputs      : [ derivation ] = [ ],
  #   environment        : attrsOf str = { }, # plain config values, NEVER secrets -- see below
  # } -> { reimage : str; imageRef : null | str; }
  #
  # The return value is exactly the shape `modules/default.nix`'s `provisioningAdapter`
  # submodule expects -- assign it straight into
  # `nixdeploy.publisher.provisioning.<providerName>`.
  #
  # `reimageCommand` and `imageRefCommand` are SHELL TEXT, not a path and not a templating
  # micro-language with its own placeholder syntax -- the substitution mechanism is ordinary
  # shell variable expansion. This is deliberate: inventing a second templating syntax on top
  # of shell, for a string that is about to be handed to a shell anyway, adds a layer to
  # learn and a layer that can disagree with the one underneath it, for no expressive power
  # shell does not already have. Inside `reimageCommand`, both of these are set to the SAME
  # value -- which one to use is the operator's call, not this factory's:
  #   - `$1`         -- the image reference, positional, exactly as `provisioningAdapter.
  #                     reimage`'s own contract states it ("receives the image reference as
  #                     its single argument"). A CLI that wants a bare positional (most do)
  #                     needs nothing else.
  #   - `$IMAGE_REF` -- the same value, exported, for a CLI or IaC tool that wants a named
  #                     variable instead (a Terraform `TF_VAR_image`-shaped invocation, for
  #                     instance).
  #
  # `runtimeInputs` is threaded straight into `pkgs.writeShellApplication`: every tool the
  # command text calls resolves to an absolute Nix store path baked into the wrapper's own
  # PATH, never the invoking process's ambient one. This is not a style preference -- a
  # `reimage` command is invoked by whatever process on the publisher ends up running it, at
  # a time this factory does not control, and "works when I tested it interactively" is not
  # the same guarantee as "works from that process's actual environment."
  #
  # `environment` is for values a command needs but that are not secret -- an API endpoint, a
  # project or zone identifier, a region. It is NOT a place for a credential: every value
  # here is baked into a Nix store path, which is world-readable on every machine that has
  # ever evaluated or fetched this configuration. Pass a PATH to a credentials file instead,
  # and have the command text read it at run time (`export
  # SOME_TOKEN="$(cat /run/secrets/some-token)"` inside `reimageCommand` itself, for
  # instance) -- the same reasoning that keeps a private key out of `nix.conf` or a Nix
  # expression anywhere else.
  #
  # Naming: `provisioning` is an `attrsOf provisioningAdapter` keyed by whatever string an
  # operator calls a provider (see `modules/default.nix`'s `provider` option: "in the
  # operator's own vocabulary"). Nothing requires that key to name a cloud or a hypervisor --
  # it can be as coarse as one entry shared by every machine on one cloud account, or as fine
  # as one entry per machine, if that is what makes a single `reimage` command line
  # unambiguous about which machine it targets. This factory has no opinion on that
  # granularity; see `provisioning-README.md` for the tradeoff.
  #
  # Argument-count enforcement: the generated `reimage` script exits non-zero, naming what it
  # actually got, if invoked with anything other than exactly one argument. The direction
  # worth guarding is not a MISSING argument (an unset `$1` already fails loudly under `set
  # -u`) but EXTRA ones -- a naive `"$@"`-forwarding command line would otherwise silently
  # accept them and let the underlying CLI decide which one wins, reimaging against whichever
  # that turns out to be. Checking `$#` explicitly turns that into one loud, named error
  # instead of something discovered from a live provider console after the fact.
  mkAdapter =
    { name
    , reimageCommand
    , imageRefCommand ? null
    , runtimeInputs ? [ ]
    , environment ? { }
    }:
    let
      exports = mkExports name environment;

      reimageScript = pkgs.writeShellApplication {
        name = "nixdeploy-reimage-${name}";
        inherit runtimeInputs;
        text = ''
          set -euo pipefail

          if [ "$#" -ne 1 ]; then
            echo "nixdeploy-reimage-${name}: expected exactly one argument (the image reference), got $#: $*" >&2
            exit 1
          fi
          IMAGE_REF="$1"
          export IMAGE_REF

          ${exports}
          ${reimageCommand}
        '';
      };

      imageRefScript =
        if imageRefCommand == null then null
        else
          pkgs.writeShellApplication {
            name = "nixdeploy-imageref-${name}";
            inherit runtimeInputs;
            text = ''
              set -euo pipefail

              ${exports}
              ${imageRefCommand}
            '';
          };
    in
    assert lib.assertMsg (builtins.match validNamePattern name != null) ''
      nixdeploy provisioning-generic.mkAdapter: name "${name}" would not survive as a Nix
      store path component (expected to match ${validNamePattern}).
    '';
    assert lib.assertMsg (reimageCommand != "") ''
      nixdeploy provisioning-generic.mkAdapter (name = "${name}"): reimageCommand must not be
      empty -- an empty command exits 0 having reimaged nothing, which reports success for a
      no-op. If this provider genuinely cannot be reimaged, omit it from
      nixdeploy.publisher.provisioning entirely; a machine naming an absent provider gets a
      terminal refusal instead of a reimage that silently did nothing (see docs/reimage.md).
    '';
    {
      reimage = "${reimageScript}/bin/nixdeploy-reimage-${name}";
      imageRef =
        if imageRefScript == null then null
        else "${imageRefScript}/bin/nixdeploy-imageref-${name}";
    };
}
