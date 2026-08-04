# modules/adapters/nix-conf.nix
#
# The `nixSettings` verb (`modules/default.nix`'s `activationAdapter`) for the two backends
# whose machine OWNS its own `nix.conf` as part of the closure this module replaces:
# `nixos.nix` and `nix-darwin.nix`. `system-manager.nix` deliberately does not import this --
# see its own `nixSettings`, which throws, and why.
#
# Factored out for one reason: the two nix.conf key names below. `nix.settings` is freeform
# on both backends, so a misspelt key does not fail a build -- it produces a setting Nix
# silently ignores, which is the exact failure `receiver.httpConnections` and
# `receiver.downloadBufferSize` exist to prevent (a memory ceiling that reads as protection
# and is not one). One spelling, in one place, is worth a file.
{ lib }:

{
  # mkNixSettings :: { httpConnections, downloadBufferSize } -> attrs
  #
  # Both values are read by `nix-daemon` while it substitutes, not by whoever asked it to,
  # which is why they land on the MACHINE rather than being handed to the receiver: the fetch
  # they bound is the one the adapter's own `activate` triggers (`nix-env --set` on a path
  # that is not present yet), and that fetch happens in the daemon's address space. On both
  # of these backends a switch restarts `nix-daemon` when `nix.conf` changes, so a changed
  # value takes effect without anyone being asked to do anything.
  #
  # `optionalAttrs` rather than emitting `null`: a null in `nix.settings` renders into
  # nix.conf as an empty value, which is not the same as leaving the system default alone --
  # and "leaves the system default alone" is exactly what both options document `null` to
  # mean.
  mkNixSettings = { httpConnections, downloadBufferSize }: {
    nix.settings =
      lib.optionalAttrs (httpConnections != null) { http-connections = httpConnections; }
      // lib.optionalAttrs (downloadBufferSize != null) { download-buffer-size = downloadBufferSize; };
  };
}
