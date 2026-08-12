# Content-addressed release system

The production unit of delivery is a deployment set, not a Git revision and not
a CI run number. One set is the exact desired composition of every selected host
plane. Its ID is `sha256:` over canonical JSON containing every artifact and its
provenance. Timestamps, queue order and workflow identity are deliberately
outside that hash.

## Artifact contract

Every configuration or boot artifact contains:

```json
{
  "target": "/nix/store/...",
  "narHash": "sha256-...",
  "closureDigest": "sha256:...",
  "provenance": {
    "source": {
      "repository": "https://forge.example/owner/repository",
      "revision": "full-git-object-id",
      "lockDigest": "sha256:..."
    },
    "builder": {
      "id": "logical-builder/system",
      "nixVersion": "full client banner",
      "storeVersion": "daemon implementation version"
    }
  },
  "requirements": {
    "system": "x86_64-linux",
    "minimumStoreVersion": "2.35.0"
  }
}
```

`narHash` identifies the root NAR. `closureDigest` identifies the sorted,
transitive cache closure, including references. The full source object ID and
the hash of `flake.lock` say what was evaluated. Builder fields say where and
with which Nix/store implementations it was realised.

Compatibility never compares product branding. A Determinate client and an
upstream client can have different banners while speaking to a compatible
store daemon. Before reading `currentPath` or calculating a delta, a v4 receiver
compares the signed system and minimum daemon version with its own runtime facts
from `nix store info --json`. An incompatible or unidentifiable daemon produces
the typed `compatibility` failure stage and nothing activates.

## One-file trust boundary

The stable channel is one JSON envelope:

```text
envelopeVersion + base64(exact payload bytes) + ed25519 signature
```

The receiver parses only the untrusted envelope, verifies the signature over
the decoded bytes, then parses the trusted payload. Signature and payload move
together in one atomic rename; there is no detached-signature window and no
JSON re-serialization ambiguity.

## Promotion transaction

`nixdeploy promote` is the only signing boundary:

```console
nixdeploy promote \
  --targets candidates.json \
  --origin /srv/nixdeploy \
  --expected-base sha256:... \
  --signing-key-file /run/secrets/release.key \
  --request-id build-object-id \
  --result /srv/nixdeploy-results/build-object-id.json
```

Under one exclusive lock it:

1. verifies and repairs the current channel from the signed journal;
2. validates all candidate artifacts and the selected host/plane intersection;
3. compares the captured base ID with stable;
4. composes a partial update without changing any unselected artifact record;
5. writes `releases/<set-id>.json` once;
6. writes the next signed, contiguous promotion record once; and
7. atomically replaces `channels/stable.json` with those exact release bytes.

Every valid request has a terminal status: `promoted`, `unchanged`,
`superseded`, or `rejected`. A retry whose candidate is already stable is
`unchanged` and does not advance the generation. A build completed against an
older base is `superseded`; it does not poison the queue or overwrite newer
work. Only storage, locking or trust failures are retryable infrastructure
errors and omit the terminal result.

`nixdeploy recover --origin ... --signing-key-file ...` verifies the complete,
contiguous journal and restores stable from the exact immutable release named
by its newest record. Recovery never replays old queue descriptors or invokes a
historical publisher over today's keys and schema.

After a terminal result, the queue owner can remove that request's descriptor
and GC roots. The immutable release and journal are sufficient for recovery.

Cache retention must follow verified releases rather than reevaluating a moving
source branch. `nixdeploy verify-release --release FILE --public-key KEY` checks
the signed envelope and prints a JSON inventory containing every configuration
and managed-boot store root named by that deployment set. It is read-only and
does not receive the signing key.

## Rolling Nix and Determinate

Nix installations are rolling inputs, not permanent pins. Each update is a
candidate with one exact lock and toolchain during its build. The stable channel
remains the last proven deployment set until all required checks, cache closure
verification and promotion succeed. A failed candidate changes neither stable
nor host desired state.

The builder itself is an appliance boundary: client and store daemon versions
must agree, sandbox probes must pass, and only then may it attest artifacts.
Host Determinate revisions can roll independently because weak hosts never
evaluate or build; they substitute a signed closure, enforce the artifact's
compatibility requirements, health-gate activation and retain rollback state.

## Migration from schema v3

The cutover is one way:

1. deploy the dual-read receiver everywhere while stable remains v3;
2. verify those receivers still converge or report already current;
3. produce and verify one complete v4 candidate;
4. promote it with expected base `none` into the new v4 origin;
5. move the public stable URL to `channels/stable.json`; and
6. enable partial v4 promotions only after that complete base exists.

New publication never emits v3. The detached v3 reader stays only for the
receiver-first migration window and can be removed after every managed host has
observed v4 successfully.
