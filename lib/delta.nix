# lib/delta.nix -- the PURE half of the delta arithmetic, exposed standalone.
#
# "How big is the change a receiver would have to fetch" has an impure half and a pure half.
# The impure half -- fetching `.narinfo` metadata for the paths a receiver is missing over
# HTTP, and asking the local store which paths it already has -- can only happen on the
# machine deciding, at decision time, and lives in the Rust receiver for exactly that reason.
# Everything else -- parsing a `.narinfo` body once it has been fetched, summing sizes, and
# deciding what a total means against a ceiling -- has no reason to touch the network or the
# store, so it lives here instead, where it can be evaluated and tested (see `checks/`)
# without either. This file depends on nothing but nixpkgs `lib`: no `pkgs`, no IFD, no
# impurity, so `nix eval` against it needs no store access and no network of its own.
#
# THE RULE THIS WHOLE FILE IS BUILT AROUND: a size that fails to parse must become a THROWN
# ERROR, never a silent 0. `nixdeploy.receiver.maxInplaceDeltaBytes` (modules/default.nix)
# exists to catch a change too large for a small machine to survive activating; `0` is the
# one value that reads as "nothing to fetch" and sails straight through that gate no matter
# how low the ceiling is set. A `.narinfo` body that is truncated, corrupted in transit, or
# answered by the wrong thing entirely (an error page, a redirect target) does not mean the
# path behind it is free to fetch -- it means the size is UNKNOWN, and unknown must never be
# indistinguishable from zero. Every parsing function below throws rather than defaulting,
# on exactly this reasoning.
{ lib }:
let
  # parseNarSize :: str -> int
  #
  # A `.narinfo` `NarSize:` value is, by construction, always a non-negative decimal integer
  # -- Nix itself never emits anything else. `builtins.match` is anchored to the WHOLE
  # string (not merely a prefix), so "123", "-123", "12.3", "123abc", "" and " 123" (stray
  # whitespace) all take the throwing branch alongside genuine garbage; only a plain string
  # of digits parses. This is deliberately stricter than `lib.toInt`, which accepts a leading
  # "-" -- a negative NAR size is not a smaller-but-valid number, it is nonsensical, and
  # treating it as parseable would let a corrupt or adversarial `.narinfo` produce a negative
  # summand that silently cancels out real bytes elsewhere in the same sum.
  parseNarSize = s:
    if builtins.match "[0-9]+" s == null then
      throw "nixdeploy: NarSize '${s}' does not parse as a non-negative integer -- refusing to treat an unreadable size as zero"
    else
      lib.toInt s;

  # splitNarinfoLine :: str -> { key :: str, value :: str } | null
  #
  # `.narinfo` is a flat "Key: value" list, one per line -- no nesting, no quoting. `null`
  # for a line that does not match (blank line, stray whitespace) rather than a throw: an
  # incidental blank line is not the same failure as a field whose VALUE is unreadable, and
  # only the latter is dangerous enough to abort on (see `parseNarSize` above). The optional
  # space after the colon accepts both "References: " (a present-but-empty list) and, in
  # principle, "References:" with nothing following.
  splitNarinfoLine = line:
    let
      m = builtins.match "([A-Za-z]+): ?(.*)" line;
    in
    if m == null then null else { key = builtins.elemAt m 0; value = builtins.elemAt m 1; };
in
{
  inherit parseNarSize;

  # parseNarinfo :: str -> attrs
  #
  # Parses a whole `.narinfo` body into an attrset. Of the fields Nix actually writes
  # (StorePath, URL, Compression, FileHash, FileSize, NarHash, NarSize, References, Deriver,
  # Sig, CA, ...), only four are given special handling here, because only four are used by
  # anything downstream of this file:
  #
  #   StorePath   left as the raw string.
  #   NarSize     parsed to an int via `parseNarSize` (throws rather than defaulting -- see
  #               the file header).
  #   References  split into a list of strings on whitespace ("" -> [ ], never [ "" ]). A
  #               `.narinfo` writes these as bare store-path basenames, not full
  #               `/nix/store/...` paths; this function does not re-qualify them, because
  #               doing so needs the store directory, which is a store fact, not a parsing
  #               fact.
  #   Sig         accumulated into a LIST, because it is the one field `.narinfo` may repeat
  #               -- a path signed by multiple caches has one `Sig:` line per signer, and
  #               folding them the way every other field folds (last value wins) would
  #               silently drop every signature but the last.
  #
  # Every other field is kept as the raw string from its line, last-value-wins, because
  # nothing here needs it typed -- a future caller that does can parse it from the raw string
  # without this file having guessed wrong about the type it wanted.
  parseNarinfo = text:
    let
      lines = lib.filter (l: l != "") (lib.splitString "\n" (lib.replaceStrings [ "\r" ] [ "" ] text));
      kvs = lib.filter (x: x != null) (map splitNarinfoLine lines);

      addField = acc: kv:
        if kv.key == "Sig" then
          acc // { Sig = (acc.Sig or [ ]) ++ [ kv.value ]; }
        else
          acc // { ${kv.key} = kv.value; };

      raw = lib.foldl' addField { Sig = [ ]; } kvs;
    in
    raw // {
      NarSize = if raw ? NarSize then parseNarSize raw.NarSize else null;
      References =
        if raw ? References
        then lib.filter (s: s != "") (lib.splitString " " raw.References)
        else [ ];
    };

  # sumNarSize :: [attrs] -> int
  #
  # Sums `NarSize` over a list of already-parsed narinfos (the shape `parseNarinfo` returns).
  # A narinfo with no `NarSize` field at all (`null` after parsing) throws here rather than
  # contributing 0 to the sum, for the identical reason `parseNarSize` throws on unreadable
  # input: a missing size is an unknown size, and an unknown size must never fold into a sum
  # as if it were known to be zero.
  sumNarSize = narinfos:
    lib.foldl'
      (acc: n:
        if n.NarSize == null then
          throw "nixdeploy: narinfo for '${n.StorePath or "<unknown store path>"}' has no NarSize -- refusing to treat an unknown size as zero"
        else
          acc + n.NarSize
      )
      0
      narinfos;

  # decide :: { bytesToFetch :: int, ceiling :: int | null } -> "in-place" | "reimage"
  #
  # The entire ceiling decision, and nothing else -- it does not know about narinfos, health
  # gates, or the typed `Converged`/`Refused`/`Reimaged` outcomes the receiver reports (see
  # the README's "Outcomes are typed"); those are the receiver's business once it already has
  # this function's answer. Boundary conditions, exactly as they must be:
  #
  #   ceiling == null      no ceiling was configured -- always "in-place". This is
  #                         `nixdeploy.receiver.maxInplaceDeltaBytes = null`'s meaning
  #                         (modules/default.nix): a deliberate "large enough that activation
  #                         size is not a survival question", not an unset placeholder.
  #   bytesToFetch <= ceiling
  #                         "in-place", INCLUDING equality -- a change of exactly the
  #                         configured ceiling is the ceiling doing its job, not exceeding it.
  #   bytesToFetch > ceiling
  #                         "reimage".
  #   bytesToFetch < 0      a hard error. A negative delta cannot occur from a correct sum of
  #                         non-negative NAR sizes; if one reaches here, something upstream
  #                         already broke its own contract, and this function must not paper
  #                         over that by rounding it up to a harmless-looking small number
  #                         (or, worse, treating it as falling safely under any ceiling).
  #   non-integer input     also a hard error, for the same reason -- a caller passing
  #                         anything other than the plain sum this function expects is a
  #                         caller this function cannot trust to have measured correctly.
  decide = { bytesToFetch, ceiling }:
    if !(builtins.isInt bytesToFetch) then
      throw "nixdeploy: bytesToFetch must be an integer, got ${builtins.typeOf bytesToFetch}"
    else if bytesToFetch < 0 then
      throw "nixdeploy: bytesToFetch is negative (${toString bytesToFetch}) -- refusing to treat a negative delta as zero or as safely under any ceiling"
    else if ceiling == null then
      "in-place"
    else if !(builtins.isInt ceiling) then
      throw "nixdeploy: ceiling must be an integer or null, got ${builtins.typeOf ceiling}"
    else if ceiling < 0 then
      throw "nixdeploy: ceiling is negative (${toString ceiling}) -- not a valid ceiling"
    else if bytesToFetch <= ceiling then
      "in-place"
    else
      "reimage";
}
