# checks/lib.nix
#
# Behaviour tests for the two PURE library files -- `lib/delta.nix` (narinfo parsing, size
# arithmetic, the ceiling decision) and `lib/manifest.nix` (the publisher/receiver schema).
# Both are exported from `flake.nix` as `lib.narinfoDelta` and `lib.manifestSchema`
# specifically so a third party can use them WITHOUT this repo's module system or its Rust
# receiver, which means they are public API and nothing else in this repo was evaluating them
# at all before this file existed.
#
# Neither file depends on anything but nixpkgs `lib` -- no `pkgs`, no IFD, no impurity -- so
# every assertion here is a plain value comparison at eval time. Nothing below builds
# anything.
#
# THE ONE RULE MOST OF THIS FILE IS ABOUT: an unreadable or absent size must become a THROWN
# ERROR, never a silent 0 (`lib/delta.nix`'s own header states why: `0` is the one value that
# reads as "nothing to fetch" and sails through `receiver.maxInplaceDeltaBytes` no matter how
# low that ceiling is set). Proving a throw is not enough on its own -- a function that threw
# unconditionally would pass such a test -- so every "this throws" case below is paired with a
# neighbouring case that must NOT throw and must produce a specific non-zero value.
{ lib, narinfoDelta, manifestSchema }:

let
  check = name: ok: detail: { inherit name ok detail; };

  # `deepSeq` because several values under test are lists and attrsets whose THROWING part is
  # inside an element: `tryEval` alone forces only to weak head normal form, so an attrset
  # with a poisoned attribute would evaluate "successfully" and the test would prove nothing.
  throws = expr: !(builtins.tryEval (builtins.deepSeq expr true)).success;

  # A 32-character string drawn from Nix's own restricted base32 alphabet (digits plus every
  # lowercase letter EXCEPT e, o, t and u). Written out rather than generated so that the
  # "wrong alphabet" fixture below can differ from it by exactly one character.
  validHash = "0123456789abcdfghijklmnpqrsvwxyz";
  validPath = "/nix/store/${validHash}-example-system";

  # A complete `.narinfo` body as Nix itself writes one, including the one field that may
  # legitimately repeat (`Sig`, for a path signed by more than one cache).
  fullNarinfo = ''
    StorePath: ${validPath}
    URL: nar/1bqjr4kndxbn2v2p8ny5b5cxg0mhk0mp2s4c5j3n0hgz1cq2v0jm.nar.xz
    Compression: xz
    FileHash: sha256:1bqjr4kndxbn2v2p8ny5b5cxg0mhk0mp2s4c5j3n0hgz1cq2v0jm
    FileSize: 1234
    NarHash: sha256:0zvwd6zvhqx7hjs1v6cxrx2h55dqdwc0kzp2v9dvq6cgqp5wvsjl
    NarSize: 5678
    References: ${validHash}-example-system 11111111111111111111111111111111-dependency
    Deriver: 22222222222222222222222222222222-example-system.drv
    Sig: cache-a-1:AAAA
    Sig: cache-b-1:BBBB
  '';

  parsedFull = narinfoDelta.parseNarinfo fullNarinfo;

  # The same body with `NarSize:` removed entirely -- a truncated response, a cache that
  # answered with something else, an error page. `lib/delta.nix` must report this as UNKNOWN
  # (null), and summing it must throw rather than fold in a zero.
  narinfoWithoutNarSize = ''
    StorePath: ${validPath}
    URL: nar/1bqjr4kndxbn2v2p8ny5b5cxg0mhk0mp2s4c5j3n0hgz1cq2v0jm.nar.xz
    NarHash: sha256:0zvwd6zvhqx7hjs1v6cxrx2h55dqdwc0kzp2v9dvq6cgqp5wvsjl
    References:
  '';

  parsedWithoutNarSize = narinfoDelta.parseNarinfo narinfoWithoutNarSize;

  # Two perfectly good narinfos, and the same pair with the sizeless one added. The pair's own
  # sum must be a specific non-zero number: without that, "the three-element list throws"
  # would be equally satisfied by a `sumNarSize` that threw on everything.
  goodPair = [
    (narinfoDelta.parseNarinfo "StorePath: ${validPath}\nNarSize: 100\n")
    (narinfoDelta.parseNarinfo "StorePath: ${validPath}\nNarSize: 23\n")
  ];

  # ---- manifest fixtures -------------------------------------------------------------------
  validManifest = {
    version = manifestSchema.currentVersion;
    revision = "0123456789abcdef0123456789abcdef01234567";
    builtAt = "2026-08-03T12:00:00Z";
    hosts = {
      host-a.planes.nixos = { backend = "nixos"; target = validPath; image = "example-image-1"; };
      host-b.planes = {
        system-manager = { backend = "system-manager"; target = validPath; };
        home-manager = { backend = "home-manager"; identity = "alice"; target = validPath; };
      };
    };
  };

  # Single-host and single-plane fixtures keep each negative case isolated.
  withHost = host: validManifest // { hosts = { host-a = host; }; };
  withNamedPlane = name: plane: withHost { planes.${name} = plane; };
  withPlane = withNamedPlane "nixos";
  validPlane = { backend = "nixos"; target = validPath; };
  validPlaneFor = backend:
    validPlane // { inherit backend; }
    // lib.optionalAttrs (backend == "home-manager") { identity = "alice"; };
  validHost = { planes.nixos = validPlane; };

  rendered = manifestSchema.render {
    inherit (validManifest) revision builtAt;
    hosts.host-a.planes.nixos = { backend = "nixos"; target = validPath; };
  };
in
[
  # =========================================================================================
  # lib/delta.nix -- parseNarSize
  # =========================================================================================
  (check "lib/delta/parseNarSize-accepts-plain-decimal-integers"
    (narinfoDelta.parseNarSize "5678" == 5678 && narinfoDelta.parseNarSize "0" == 0)
    "expected \"5678\" and \"0\" to parse to 5678 and 0")

  # The whole point of the file: every one of these is a size that could not be READ, and none
  # of them may come back as a number. `-1` is called out separately in `lib/delta.nix`'s own
  # comment because `lib.toInt` would happily accept it, and a negative summand silently
  # cancels real bytes elsewhere in the same total.
  (check "lib/delta/parseNarSize-throws-on-everything-that-is-not-a-non-negative-integer"
    (lib.all (s: throws (narinfoDelta.parseNarSize s))
      [ "-1" "-5678" "1.5" "" " 12" "12 " "0x10" "abc" "12abc" "1e3" "+7" ])
    "expected every unreadable NarSize to throw; one of them parsed to a number instead")

  # =========================================================================================
  # lib/delta.nix -- parseNarinfo
  # =========================================================================================
  (check "lib/delta/parseNarinfo-types-NarSize-as-an-int-not-a-string"
    (builtins.isInt parsedFull.NarSize && parsedFull.NarSize == 5678)
    "expected NarSize to come back as the integer 5678")

  (check "lib/delta/parseNarinfo-splits-References-into-bare-basenames"
    (parsedFull.References == [
      "${validHash}-example-system"
      "11111111111111111111111111111111-dependency"
    ])
    "expected References to split on whitespace into two bare store-path basenames, unqualified")

  # The one field `.narinfo` may legitimately repeat. Folding it the way every other field
  # folds (last value wins) would silently drop every signature but the last, on exactly the
  # paths that are signed by more than one cache.
  (check "lib/delta/parseNarinfo-accumulates-repeated-Sig-lines-instead-of-overwriting"
    (parsedFull.Sig == [ "cache-a-1:AAAA" "cache-b-1:BBBB" ])
    "expected both Sig lines to be kept, in order")

  (check "lib/delta/parseNarinfo-keeps-unmodelled-fields-as-raw-strings"
    (parsedFull.StorePath == validPath
      && parsedFull.Compression == "xz"
      && parsedFull.FileSize == "1234")
    "expected StorePath/Compression/FileSize to survive as the raw strings from their lines (FileSize deliberately NOT typed as an int -- nothing downstream reads it)")

  (check "lib/delta/parseNarinfo-reads-an-empty-References-line-as-an-empty-list"
    (parsedWithoutNarSize.References == [ ])
    "expected \"References:\" with nothing after it to produce [ ], never [ \"\" ] -- a phantom empty reference would be walked as a store path")

  # A missing size is UNKNOWN, and unknown is not zero. `parseNarinfo` reports it as null (it
  # is not the function that decides what to do about it); `sumNarSize` below is where that
  # becomes an error.
  (check "lib/delta/parseNarinfo-reports-a-missing-NarSize-as-null-not-zero"
    (parsedWithoutNarSize.NarSize == null)
    "expected a narinfo with no NarSize line to come back with NarSize = null")

  # =========================================================================================
  # lib/delta.nix -- sumNarSize
  # =========================================================================================
  (check "lib/delta/sumNarSize-adds-known-sizes"
    (narinfoDelta.sumNarSize goodPair == 123 && narinfoDelta.sumNarSize [ ] == 0)
    "expected 100 + 23 = 123, and an empty list to sum to 0 (nothing missing is genuinely nothing to fetch)")

  # THE ERROR-NOT-ZERO RULE, stated as a test. The list below is the passing pair plus one
  # narinfo whose size could not be read; the pair's own sum is asserted separately above as
  # 123, so this cannot be satisfied by a function that simply throws on everything.
  (check "lib/delta/sumNarSize-throws-rather-than-folding-an-unknown-size-in-as-zero"
    (throws (narinfoDelta.sumNarSize (goodPair ++ [ parsedWithoutNarSize ])))
    "expected a narinfo with no NarSize to abort the sum; it contributed 0 and the total came back as if nothing were missing")

  # =========================================================================================
  # lib/delta.nix -- decide
  # =========================================================================================
  (check "lib/delta/decide-treats-a-null-ceiling-as-no-ceiling"
    (narinfoDelta.decide { bytesToFetch = 0; ceiling = null; } == "in-place"
      && narinfoDelta.decide { bytesToFetch = 999999999999; ceiling = null; } == "in-place")
    "expected ceiling = null to mean no ceiling at all, for any number of bytes")

  # Equality is the boundary that gets written wrong: a change of exactly the configured
  # ceiling is the ceiling doing its job, not exceeding it.
  (check "lib/delta/decide-is-inclusive-at-the-ceiling-and-exclusive-one-byte-above"
    (narinfoDelta.decide { bytesToFetch = 500; ceiling = 500; } == "in-place"
      && narinfoDelta.decide { bytesToFetch = 501; ceiling = 500; } == "reimage"
      && narinfoDelta.decide { bytesToFetch = 499; ceiling = 500; } == "in-place")
    "expected <= ceiling to be in-place and > ceiling to be reimage")

  (check "lib/delta/decide-handles-a-zero-ceiling-without-special-casing-it"
    (narinfoDelta.decide { bytesToFetch = 0; ceiling = 0; } == "in-place"
      && narinfoDelta.decide { bytesToFetch = 1; ceiling = 0; } == "reimage")
    "expected ceiling = 0 to allow only a genuinely empty change")

  (check "lib/delta/decide-throws-on-a-negative-delta-instead-of-calling-it-safely-small"
    (throws (narinfoDelta.decide { bytesToFetch = -1; ceiling = 500; }))
    "expected a negative bytesToFetch to be a hard error -- it cannot come from a correct sum of non-negative sizes, and it would otherwise pass every ceiling there is")

  (check "lib/delta/decide-throws-on-non-integer-input"
    (throws (narinfoDelta.decide { bytesToFetch = "500"; ceiling = 500; })
      && throws (narinfoDelta.decide { bytesToFetch = 500; ceiling = "500"; })
      && throws (narinfoDelta.decide { bytesToFetch = 500; ceiling = -1; }))
    "expected a string bytesToFetch, a string ceiling and a negative ceiling to each be a hard error")

  # =========================================================================================
  # lib/manifest.nix -- check
  # =========================================================================================
  (check "lib/manifest/check-passes-a-complete-manifest"
    (manifestSchema.check validManifest == [ ])
    "expected the valid manifest fixture to produce no problems; every negative case below is this fixture with one field changed, so they prove nothing if this one fails")

  (check "lib/manifest/check-refuses-a-schema-version-it-does-not-implement"
    (manifestSchema.check (validManifest // { version = manifestSchema.currentVersion + 1; }) != [ ]
      && manifestSchema.check (validManifest // { version = "1"; }) != [ ]
      && manifestSchema.check (builtins.removeAttrs validManifest [ "version" ]) != [ ])
    "expected a newer version, a stringly-typed version and a missing version to each be refused -- a manifest ends in code execution, so 'mostly understanding' it is not an option")

  (check "lib/manifest/check-requires-a-revision-and-an-ISO-8601-UTC-builtAt"
    (manifestSchema.check (validManifest // { revision = ""; }) != [ ]
      && manifestSchema.check (builtins.removeAttrs validManifest [ "revision" ]) != [ ]
      && manifestSchema.check (validManifest // { builtAt = "2026-08-03"; }) != [ ]
      && manifestSchema.check (validManifest // { builtAt = "2026-08-03T12:00:00+02:00"; }) != [ ]
      && manifestSchema.check (validManifest // { builtAt = "2026-08-03T12:00:00Z"; }) == [ ])
    "expected an empty/absent revision and a non-UTC or date-only builtAt to be refused, and a real UTC timestamp to pass")

  (check "lib/manifest/check-requires-hosts-to-be-an-attrset-of-entries"
    (manifestSchema.check (validManifest // { hosts = [ ]; }) != [ ]
      && manifestSchema.check (builtins.removeAttrs validManifest [ "hosts" ]) != [ ])
    "expected a list-valued or missing hosts to be refused")

  (check "lib/manifest/check-requires-each-plane-to-name-a-supported-backend"
    (manifestSchema.check (withPlane (builtins.removeAttrs validPlane [ "backend" ])) != [ ]
      && manifestSchema.check (withPlane (validPlane // { backend = "freebsd"; })) != [ ]
      && lib.all (backend: manifestSchema.check (withNamedPlane backend (validPlaneFor backend)) == [ ])
      manifestSchema.backends)
    "expected a missing backend and an unsupported one to be refused, and all four plane backends to pass")

  # The store-path shape check exists to catch a manifest naming something that is obviously
  # not a store path. `e` is one of the four letters Nix's restricted base32 excludes, so a
  # hash containing it is exactly the "subtly wrong" case worth proving is caught.
  (check "lib/manifest/check-rejects-a-target-that-is-not-a-store-path"
    (manifestSchema.check (withPlane (builtins.removeAttrs validPlane [ "target" ])) != [ ]
      && manifestSchema.check (withPlane (validPlane // { target = "https://example.org/system"; })) != [ ]
      && manifestSchema.check (withPlane (validPlane // { target = "example-system"; })) != [ ]
      && manifestSchema.check (withPlane (validPlane // { target = ""; })) != [ ]
      && manifestSchema.check (withPlane (validPlane // { target = 42; })) != [ ]
      && manifestSchema.check
      (withPlane (validPlane // {
        target = "/nix/store/e123456789abcdfghijklmnpqrsvwxyz-example";
      })) != [ ])
    "expected malformed or non-store targets to be refused")

  (check "lib/manifest/check-enforces-identity-and-image-by-plane-kind"
    (manifestSchema.check (withNamedPlane "home-manager" (validPlaneFor "home-manager")) == [ ]
      && manifestSchema.check (withPlane (validPlane // { backend = "home-manager"; })) != [ ]
      && manifestSchema.check (withPlane (validPlane // { identity = "alice"; })) != [ ]
      && manifestSchema.check (withPlane (validPlane // { image = "example-image-1"; })) == [ ]
      && manifestSchema.check (withPlane (validPlane // { backend = "system-manager"; image = "example-image-1"; })) != [ ]
      && manifestSchema.check (withPlane (validPlane // { image = ""; })) != [ ])
    "expected identity only on home-manager and image only on nixos")

  (check "lib/manifest/check-requires-canonical-nonempty-planes"
    (manifestSchema.check (withHost { planes = { }; }) != [ ]
      && manifestSchema.check (withHost { planes.custom = validPlane; }) != [ ]
      && manifestSchema.check (withHost { planes.nixos = validPlaneFor "system-manager"; }) != [ ]
      && manifestSchema.check (withHost { planes.nixos = validPlane; }) == [ ])
    "expected every host to carry at least one plane whose canonical name equals its backend")

  # `check` is documented as never throwing, so that a third party validating a hand-built
  # manifest gets EVERY problem in one pass rather than a build aborted on the first.
  (check "lib/manifest/check-reports-every-problem-at-once-rather-than-aborting-on-the-first"
    (builtins.length
      (manifestSchema.check {
        version = 99;
        hosts.host-a.planes.nixos = { backend = "freebsd"; target = "not-a-store-path"; };
      }) >= 5)
    "expected a manifest broken in five ways to report every problem in one list")

  # =========================================================================================
  # lib/manifest.nix -- render
  # =========================================================================================
  (check "lib/manifest/render-stamps-the-current-version-and-keeps-optionals-absent"
    (rendered.version == manifestSchema.currentVersion
      && !(rendered.hosts.host-a.planes.nixos ? image)
      && !(rendered.hosts.host-a.planes.nixos ? identity)
      && rendered.hosts.host-a.planes.nixos.target == validPath
      && manifestSchema.check rendered == [ ])
    "expected render to stamp currentVersion, omit absent optionals, and produce a valid manifest")

  (check "lib/manifest/render-refuses-to-produce-an-invalid-manifest"
    (throws
      (manifestSchema.render {
        revision = "abc";
        builtAt = "2026-08-03T12:00:00Z";
        hosts.host-a.planes.nixos = { backend = "nixos"; target = "not-a-store-path"; };
      })
    && throws (manifestSchema.render {
      revision = "";
      builtAt = "2026-08-03T12:00:00Z";
      hosts = { };
    }))
    "expected render to throw on a plane whose target is not a store path, and on an empty revision")

  # `render`'s own doc claims its output is the exact bytes the publisher signs and the
  # receiver verifies: strings, ints, bools, nulls, lists and attrsets, never a derivation,
  # path or function. A round-trip through JSON is the cheapest way to hold that claim
  # accountable -- anything unserialisable throws, and anything that serialises to a different
  # value comes back unequal.
  (check "lib/manifest/render-output-round-trips-through-JSON-unchanged"
    (builtins.fromJSON (builtins.toJSON rendered) == rendered)
    "expected the rendered manifest to survive toJSON/fromJSON identically -- it IS the signed bytes, so anything that does not survive that trip is not a manifest")
]
