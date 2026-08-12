# lib/manifest.nix -- pure schema for the signed publisher/receiver seam.
#
# A host owns named planes. Each plane selects one activation backend and one exact,
# immutable Nix store target. A system plane separately describes whether nixdeploy owns no
# boot actuator (`mode = "none"`) or a managed set of boot-role artifacts. Keeping the boot
# object below the configuration plane preserves the two independent axes: one system target
# can carry both primary and nixrescue artifacts without pretending either role is a plane.
# `builtins.toJSON (render ...)` is exactly the body the publisher signs.
{ lib }:
let
  schema = builtins.fromJSON (builtins.readFile ./schema.json);
  inherit (schema) currentVersion backends bootModes bootRoles;

  storePathRe = "/nix/store/[0-9a-df-np-sv-z]{32}-[A-Za-z0-9+_.?=-]+";
  looksLikeStorePath = value:
    builtins.isString value && builtins.match storePathRe value != null;

  timestampRe = "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z";
  looksLikeTimestamp = value:
    builtins.isString value && builtins.match timestampRe value != null;

  unknownFields = allowed: value:
    builtins.filter (name: !(builtins.elem name allowed)) (builtins.attrNames value);

  checkBootArtifact = hostName: planeName: roleName: artifact:
    let
      prefix = message: "host '${hostName}' plane '${planeName}' boot role '${roleName}': ${message}";
      image = if builtins.isAttrs artifact then artifact.image or null else null;
      unknown = if builtins.isAttrs artifact
        then unknownFields [ "artifact" "image" ] artifact
        else [ ];
    in
    if !(builtins.isAttrs artifact) then
      [ (prefix "artifact must be an attrset") ]
    else
      lib.optional (unknown != [ ])
        (prefix "unknown fields: ${lib.concatStringsSep ", " unknown}")
      ++ lib.optional (!(artifact ? artifact)) (prefix "missing artifact")
      ++ lib.optional (artifact ? artifact && !(builtins.isString artifact.artifact))
        (prefix "artifact must be a string")
      ++ lib.optional (artifact ? artifact && builtins.isString artifact.artifact
        && !(looksLikeStorePath artifact.artifact))
        (prefix "artifact does not look like a Nix store path")
      ++ lib.optional (image != null && !(builtins.isString image))
        (prefix "image must be a string when present")
      ++ lib.optional (builtins.isString image && image == "")
        (prefix "image must not be empty");

  checkBoot = hostName: planeName: boot:
    let
      prefix = message: "host '${hostName}' plane '${planeName}' boot: ${message}";
      mode = if builtins.isAttrs boot then boot.mode or null else null;
      roles = if builtins.isAttrs boot then boot.roles or null else null;
      unknown = if builtins.isAttrs boot
        then unknownFields [ "mode" "roles" ] boot
        else [ ];
      unknownRoles = if builtins.isAttrs roles
        then builtins.filter (name: !(builtins.elem name bootRoles)) (builtins.attrNames roles)
        else [ ];
    in
    if !(builtins.isAttrs boot) then
      [ (prefix "must be an attrset") ]
    else
      lib.optional (unknown != [ ])
        (prefix "unknown fields: ${lib.concatStringsSep ", " unknown}")
      ++ lib.optional (!(boot ? mode)) (prefix "missing mode")
      ++ lib.optional (boot ? mode && !(builtins.isString mode))
        (prefix "mode must be a string")
      ++ lib.optional (builtins.isString mode && !(builtins.elem mode bootModes))
        (prefix "mode '${mode}' is not one of: ${lib.concatStringsSep ", " bootModes}")
      ++ lib.optional (mode == "none" && (boot ? roles))
        (prefix "mode 'none' must not carry roles")
      ++ lib.optional (mode == "managed" && !(builtins.isAttrs roles))
        (prefix "mode 'managed' requires a roles attrset")
      ++ lib.optional (mode == "managed" && builtins.isAttrs roles && !(roles ? primary))
        (prefix "mode 'managed' requires the primary role")
      ++ lib.optional (unknownRoles != [ ])
        (prefix "unknown roles: ${lib.concatStringsSep ", " unknownRoles}")
      ++ lib.optionals (mode == "managed" && builtins.isAttrs roles)
        (lib.flatten (lib.mapAttrsToList (checkBootArtifact hostName planeName) roles));

  checkPlane = hostName: planeName: plane:
    if !(builtins.isAttrs plane) then
      [ "host '${hostName}' plane '${planeName}': target must be an attrset" ]
    else
    let
      prefix = message: "host '${hostName}' plane '${planeName}': ${message}";
      hasBackend = plane ? backend;
      hasTarget = plane ? target;
      identity = plane.identity or null;
      boot = plane.boot or null;
      backend = if hasBackend then plane.backend else null;
      isSystemPlane = builtins.elem backend [ "nixos" "system-manager" ];
      unknown = unknownFields [ "backend" "identity" "target" "boot" "image" ] plane;
    in
    lib.optional (unknown != [ ])
      (prefix "unknown fields: ${lib.concatStringsSep ", " unknown}")
    ++ lib.optional (!(builtins.elem planeName backends))
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
    ++ lib.optional (plane ? image)
      (prefix "image moved to boot.roles.<role>.image in schema version ${toString currentVersion}")
    ++ lib.optional (backend == "nixos" && boot == null)
      (prefix "boot is required for nixos; use mode 'none' when no boot actuator exists")
    ++ lib.optional (!isSystemPlane && boot != null)
      (prefix "boot is valid only on nixos and system-manager system planes")
    ++ lib.optionals (isSystemPlane && boot != null) (checkBoot hostName planeName boot);

  checkHost = hostName: host:
    if !(builtins.isAttrs host) then
      [ "host '${hostName}': entry must be an attrset" ]
    else
    let
      hasPlanes = host ? planes;
      unknown = unknownFields [ "planes" ] host;
    in
    lib.optional (unknown != [ ])
      "host '${hostName}': unknown fields: ${lib.concatStringsSep ", " unknown}"
    ++ lib.optional (!hasPlanes) "host '${hostName}': missing planes"
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
      unknown = unknownFields [ "version" "revision" "builtAt" "hosts" ] manifest;
    in
    lib.optional (unknown != [ ])
      "unknown document fields: ${lib.concatStringsSep ", " unknown}"
    ++ versionProblems ++ revisionProblems ++ builtAtProblems ++ hostsProblems;

  renderBootArtifact = artifact:
    { inherit (artifact) artifact; }
    // lib.optionalAttrs (artifact ? image && artifact.image != null) {
      inherit (artifact) image;
    };

  renderBoot = boot:
    { inherit (boot) mode; }
    // lib.optionalAttrs (boot.mode == "managed") {
      roles = lib.mapAttrs (_: renderBootArtifact) boot.roles;
    };

  renderPlane = plane:
    {
      inherit (plane) backend target;
    }
    // lib.optionalAttrs (plane ? identity && plane.identity != null) {
      inherit (plane) identity;
    }
    // lib.optionalAttrs (plane ? boot && plane.boot != null) {
      boot = renderBoot plane.boot;
    };
in
{
  inherit currentVersion backends bootModes bootRoles check;

  render = { revision, builtAt, hosts }:
    let
      candidate = {
        version = currentVersion;
        inherit revision builtAt;
        inherit hosts;
      };
      problems = check candidate;
      manifest = {
        inherit (candidate) version revision builtAt;
        hosts = lib.mapAttrs (_: host: {
          planes = lib.mapAttrs (_: renderPlane) host.planes;
        }) candidate.hosts;
      };
    in
    if problems == [ ] then manifest
    else throw ''
      nixdeploy: refusing to render an invalid manifest:
      ${lib.concatMapStringsSep "\n" (problem: "  - ${problem}") problems}'';
}
