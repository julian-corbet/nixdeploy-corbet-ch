# lib/manifest.nix -- the manifest schema, and the ONLY thing a third party needs in order
# to produce a manifest without adopting this repo's publisher at all.
#
# A manifest is the single artifact the publisher and the receiver agree on: "host H should
# be running store path P (built from backend B, and if it must be reimaged rather than
# switched, from image I)". Everything upstream of the manifest (how it was built, which CI
# ran, which git forge hosts the source) is the publisher's business and none of the
# receiver's; everything downstream (how P gets fetched, sized against the receiver's own
# store, and activated) is the receiver's business and none of the publisher's. This file is
# the seam between the two, so it is written to depend on NOTHING but nixpkgs `lib` -- not
# `pkgs`, no IFD, no impurity -- so that a manifest producer who is not this repo (a
# hand-rolled CI script, a different language entirely) can still import exactly this file to
# know precisely what a valid manifest looks like, without pulling in a builder.
#
# WHY THE MANIFEST IS SIGNED (see `nixdeploy.publisher.cache.signingKeyFile` and
# `nixdeploy.receiver.manifest.publicKey` in modules/default.nix): the manifest names store
# paths that a receiver will FETCH and RUN. An unverified manifest is arbitrary code
# execution with extra steps -- anyone who can put bytes in front of a receiver's manifest
# URL (a compromised mirror, a MITM on an unauthenticated HTTP fetch, a typo'd redirect) can
# otherwise hand every managed machine an arbitrary closure to switch to. Signing moves the
# trust boundary from "whoever can answer this URL" to "whoever holds the signing key", which
# is the same trust boundary the binary cache itself already depends on. Signing is enforced
# by the receiver at fetch time, not by anything in this file -- this file only defines the
# shape the signed bytes must have.
#
# WHY THE MANIFEST IS VERSION-GATED: `version` exists so a receiver that does not understand
# a manifest can refuse it instead of guessing. A manifest is not a preference the receiver
# can partially honor -- it names store paths that will be RUN -- so a receiver built against
# an older (or newer) schema than the one it is handed has exactly two safe responses: refuse,
# or refuse. There is no such thing as "mostly understanding" a set of instructions that end
# in code execution. The same reasoning applies one level up, at eval time: THIS file only
# knows how to validate and render the ONE schema version it implements (`currentVersion`
# below). A manifest attrset claiming a different version is, from this file's point of view,
# equally unverifiable -- `check` refuses it for the identical reason a receiver would refuse
# it at runtime, just caught earlier, at build time, where it is cheaper to fix.
{ lib }:
let
  # `./schema.json` is the ONE place `currentVersion` and `backends` are written down --
  # read here with nothing but `builtins.readFile` and `builtins.fromJSON`, so this file
  # still depends on nothing but `lib` (reading a sibling file that ships in the same
  # directory is not a dependency in the sense this file's header cares about: no `pkgs`, no
  # IFD, no derivation has to build before this can evaluate -- a third party who vendors
  # this file need only vendor its one sibling alongside it).
  #
  # `src/manifest.rs` reads the SAME file, via `include_str!` at compile time -- see that
  # file's own doc. That makes `./schema.json` the one place either language's copy of
  # `currentVersion` or `backends` is written, rather than a value each side re-states and
  # trusts to stay equal. `modules/default.nix` imports `backends` from HERE, through this
  # function, rather than carrying its own copy of the list -- so this file is the one
  # Nix-side source for it, not merely one of several hand-kept copies that happen to match
  # today.
  schema = builtins.fromJSON (builtins.readFile ./schema.json);

  # The schema version implemented by THIS copy of the file. Bump it, and the shape below,
  # together, whenever the manifest's meaning changes in a way an old receiver must not
  # silently accept (a field's meaning changing under the same name is the dangerous case;
  # a purely additive field is not, but this repo has no way to know which additions a given
  # receiver already tolerates, so any shape change is treated as a version bump). Bumping it
  # means editing `schema.json`, which is what keeps `src/manifest.rs`'s
  # `supported_schema_version()` in step automatically -- there is no second place left to
  # forget.
  currentVersion = schema.currentVersion;

  # The three backends `nixdeploy.backend` accepts (modules/default.nix). Read from
  # `schema.json` rather than written here, for the same reason `currentVersion` is: this was
  # the third of three hand-kept copies (the other two were `modules/default.nix`'s `backend`
  # enum and `src/publish.rs`'s `BACKENDS`), and a list edited in three places by hand is a
  # list that drifts the moment one edit is forgotten.
  backends = schema.backends;

  # A Nix store path, loosely: `/nix/store/<32-char base32 hash>-<name>`. The 32-character
  # alphabet is Nix's own restricted base32 (digits plus lowercase letters, EXCLUDING
  # "e", "o", "t", "u" -- chosen upstream to avoid accidentally spelling words). This is a
  # "looks like a store path" check, not a full re-implementation of Nix's own store-path
  # validity rules (case sensitivity of the name part, reserved characters, etc.) -- good
  # enough to catch the actual failure this guards against, which is a manifest naming
  # something that is obviously NOT a store path (a URL, a bare package name, an empty
  # string) rather than a subtly malformed one.
  storePathRe = "/nix/store/[0-9a-df-np-sv-z]{32}-[A-Za-z0-9+_.?=-]+";
  looksLikeStorePath = s: builtins.isString s && builtins.match storePathRe s != null;

  # A conservative ISO-8601 UTC timestamp, second precision (e.g. "2026-08-03T12:00:00Z").
  # The contract only requires "a build timestamp"; UTC-with-Z is chosen here so the value is
  # unambiguous without a timezone table and sorts correctly as a plain string -- both
  # properties a receiver's refusal/log output benefits from without having to parse it at
  # all.
  isoTimestampRe = "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z";
  looksLikeTimestamp = s: builtins.isString s && builtins.match isoTimestampRe s != null;

  # checkHost :: str -> attrs -> [string]
  #
  # Validates one `hosts.<name>` entry. `image` is OPTIONAL -- `null` for a host that is
  # switched in place, a non-empty string for a host that can only be reimaged (see the
  # README's "How do I become this image?" adapter registry) -- so its absence is not itself
  # a problem; only a present-but-malformed value is.
  checkHost = name: h:
    let
      p = msg: "host '${name}': ${msg}";
      hasBackend = h ? backend;
      hasPath = h ? path;
      image = h.image or null;
    in
    lib.optional (!hasBackend) (p "missing backend")
    ++ lib.optional (hasBackend && !(builtins.elem h.backend backends))
      (p "backend '${toString h.backend}' is not one of: ${lib.concatStringsSep ", " backends}")
    ++ lib.optional (!hasPath) (p "missing path")
    ++ lib.optional (hasPath && !(builtins.isString h.path)) (p "path must be a string")
    ++ lib.optional (hasPath && builtins.isString h.path && !(looksLikeStorePath h.path))
      (p "path '${h.path}' does not look like a Nix store path")
    ++ lib.optional (image != null && !(builtins.isString image))
      (p "image must be a string or null, got ${builtins.typeOf image}")
    ++ lib.optional (image != null && builtins.isString image && image == "")
      (p "image must not be an empty string -- omit it (null) for a host that is never reimaged");

  # check :: attrs -> [string]
  #
  # Returns a list of human-readable problems; empty means valid. Deliberately never throws
  # -- a caller building a manifest (or a third party validating a hand-built one before
  # publishing it) wants every problem in one pass, not a build aborted on the first one.
  # `render` below is the one place a problem list turns into a hard failure.
  #
  # Bound here in the `let`, not as a sibling attribute of the returned set below, because
  # `render` needs to call it too -- an un-`rec` attrset cannot see its own other attributes,
  # and making the whole returned set `rec` for one internal call would let every attribute
  # see every other, which is a larger and easier-to-misuse scope than this actually needs.
  check = manifest:
    let
      hasVersion = manifest ? version;
      versionProblems =
        if !hasVersion then [ "missing version" ]
        else if !(builtins.isInt manifest.version) then
          [ "version must be an integer, got ${builtins.typeOf manifest.version}" ]
        else if manifest.version != currentVersion then
          [
            "version ${toString manifest.version} is not ${toString currentVersion} -- this copy of lib/manifest.nix only validates the schema version it itself implements; a receiver refusing an unfamiliar version is correct, and so is this check refusing to bless one"
          ]
        else [ ];

      hasRevision = manifest ? revision;
      revisionProblems =
        if !hasRevision then [ "missing revision" ]
        else if !(builtins.isString manifest.revision) || manifest.revision == "" then
          [ "revision must be a non-empty string" ]
        else [ ];

      hasBuiltAt = manifest ? builtAt;
      builtAtProblems =
        if !hasBuiltAt then [ "missing builtAt" ]
        else if !(builtins.isString manifest.builtAt) || manifest.builtAt == "" then
          [ "builtAt must be a non-empty string" ]
        else if !(looksLikeTimestamp manifest.builtAt) then
          [ "builtAt '${manifest.builtAt}' is not an ISO-8601 UTC timestamp, e.g. 2026-08-03T12:00:00Z" ]
        else [ ];

      hasHosts = manifest ? hosts;
      hostsProblems =
        if !hasHosts then [ "missing hosts" ]
        else if !(builtins.isAttrs manifest.hosts) then
          [ "hosts must be an attrset of host name -> host entry" ]
        else lib.flatten (lib.mapAttrsToList checkHost manifest.hosts);
    in
    versionProblems ++ revisionProblems ++ builtAtProblems ++ hostsProblems;
in
{
  inherit currentVersion backends check;

  # render :: { revision :: str, builtAt :: str, hosts :: attrs } -> attrs
  #
  # Stamps `currentVersion` on and produces the JSON-safe attrset (`builtins.toJSON` on the
  # result is exactly the manifest bytes the publisher signs and the receiver verifies) --
  # every value in it is a string, int, bool, null, list or attrset of those, never a
  # derivation, path or function, so it round-trips through JSON with nothing lost.
  #
  # Runs `check` on its own output and throws if that list is non-empty: it must not be
  # possible to render an invalid manifest through this function. A caller assembling a
  # manifest by hand (not through `render`) can still call `check` directly to validate
  # before doing its own serialization -- that is the whole reason `check` is exposed
  # separately rather than folded silently into `render`.
  render = { revision, builtAt, hosts }:
    let
      manifest = {
        version = currentVersion;
        inherit revision builtAt;
        hosts = lib.mapAttrs
          (_: h: {
            backend = h.backend;
            path = h.path;
            image = h.image or null;
          })
          hosts;
      };
      problems = check manifest;
    in
    if problems == [ ] then manifest
    else
      throw ''
        nixdeploy: refusing to render an invalid manifest:
        ${lib.concatMapStringsSep "\n" (p: "  - ${p}") problems}'';
}
