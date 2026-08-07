# lib/manifest.nix -- pure schema for the signed publisher/receiver seam.
#
# A host owns named planes. Each plane selects one activation backend and one exact,
# immutable Nix store target. Version 2 has exactly four canonical names, each equal to its
# backend: nixos, system-manager, home-manager, and nix-darwin.
# `builtins.toJSON (render ...)` is exactly the body the publisher signs.
{ lib }:
let
  currentVersion = 2;
  backends = [ "nixos" "system-manager" "home-manager" "nix-darwin" ];

  storePathRe = "/nix/store/[0-9a-df-np-sv-z]{32}-[A-Za-z0-9+_.?=-]+";
  looksLikeStorePath = value:
    builtins.isString value && builtins.match storePathRe value != null;

  timestampRe = "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z";
  looksLikeTimestamp = value:
    builtins.isString value && builtins.match timestampRe value != null;

  checkPlane = hostName: planeName: plane:
    if !(builtins.isAttrs plane) then
      [ "host '${hostName}' plane '${planeName}': target must be an attrset" ]
    else
    let
      prefix = message: "host '${hostName}' plane '${planeName}': ${message}";
      hasBackend = plane ? backend;
      hasTarget = plane ? target;
      identity = plane.identity or null;
      image = plane.image or null;
      backend = if hasBackend then plane.backend else null;
    in
    lib.optional (!(builtins.elem planeName backends))
      (prefix "name is not one of: ${lib.concatStringsSep ", " backends}")
    ++ lib.optional (!hasBackend) (prefix "missing backend")
    ++ lib.optional (hasBackend && !(builtins.isString backend))
      (prefix "backend must be a string")
    ++ lib.optional (hasBackend && builtins.isString backend && !(builtins.elem backend backends))
      (prefix "backend '${backend}' is not one of: ${lib.concatStringsSep ", " backends}")
    ++ lib.optional (hasBackend && builtins.isString backend && planeName != backend)
      (prefix "name must equal backend '${backend}'")
    ++ lib.optional (!hasTarget) (prefix "missing target")
    ++ lib.optional (hasTarget && !(builtins.isString plane.target))
      (prefix "target must be a string")
    ++ lib.optional (hasTarget && builtins.isString plane.target && !(looksLikeStorePath plane.target))
      (prefix "target '${plane.target}' does not look like a Nix store path")
    ++ lib.optional (backend == "home-manager" && (!(builtins.isString identity) || identity == ""))
      (prefix "identity is required and must be non-empty for home-manager")
    ++ lib.optional (backend != "home-manager" && identity != null)
      (prefix "identity is only meaningful for the home-manager backend")
    ++ lib.optional (image != null && !(builtins.isString image))
      (prefix "image must be a string when present")
    ++ lib.optional (builtins.isString image && image == "")
      (prefix "image must not be empty")
    ++ lib.optional (image != null && backend != "nixos")
      (prefix "image is only meaningful for the nixos backend");

  checkHost = hostName: host:
    if !(builtins.isAttrs host) then
      [ "host '${hostName}': entry must be an attrset" ]
    else
    let
      hasPlanes = host ? planes;
    in
    lib.optional (!hasPlanes) "host '${hostName}': missing planes"
    ++ lib.optional (hasPlanes && !(builtins.isAttrs host.planes))
      "host '${hostName}': planes must be an attrset of plane name to target"
    ++ lib.optional (hasPlanes && builtins.isAttrs host.planes && host.planes == { })
      "host '${hostName}': planes must not be empty"
    ++ lib.optionals (hasPlanes && builtins.isAttrs host.planes)
      (lib.flatten (lib.mapAttrsToList (checkPlane hostName) host.planes));

  check = manifest:
    let
      versionProblems =
        if !(manifest ? version) then [ "missing version" ]
        else if !(builtins.isInt manifest.version) then [ "version must be an integer" ]
        else if manifest.version != currentVersion then
          [ "version ${toString manifest.version} is not ${toString currentVersion}" ]
        else [ ];
      revisionProblems =
        if !(manifest ? revision) then [ "missing revision" ]
        else if !(builtins.isString manifest.revision) || manifest.revision == "" then
          [ "revision must be a non-empty string" ]
        else [ ];
      builtAtProblems =
        if !(manifest ? builtAt) then [ "missing builtAt" ]
        else if !(looksLikeTimestamp manifest.builtAt) then
          [ "builtAt must be an ISO-8601 UTC timestamp, e.g. 2026-08-03T12:00:00Z" ]
        else [ ];
      hostsProblems =
        if !(manifest ? hosts) then [ "missing hosts" ]
        else if !(builtins.isAttrs manifest.hosts) then
          [ "hosts must be an attrset of host name to host entry" ]
        else if manifest.hosts == { } then [ "hosts must contain at least one host" ]
        else lib.flatten (lib.mapAttrsToList checkHost manifest.hosts);
    in
    versionProblems ++ revisionProblems ++ builtAtProblems ++ hostsProblems;

  renderPlane = plane:
    {
      inherit (plane) backend target;
    }
    // lib.optionalAttrs (plane ? identity && plane.identity != null) {
      inherit (plane) identity;
    }
    // lib.optionalAttrs (plane ? image && plane.image != null) {
      inherit (plane) image;
    };
in
{
  inherit currentVersion backends check;

  render = { revision, builtAt, hosts }:
    let
      manifest = {
        version = currentVersion;
        inherit revision builtAt;
        hosts = lib.mapAttrs (_: host: {
          planes = lib.mapAttrs (_: renderPlane) host.planes;
        }) hosts;
      };
      problems = check manifest;
    in
    if problems == [ ] then manifest
    else throw ''
      nixdeploy: refusing to render an invalid manifest:
      ${lib.concatMapStringsSep "\n" (problem: "  - ${problem}") problems}'';
}
