# experiments

Throwaway trials: spikes, one-off scripts, things tried and abandoned or not yet worth
writing up. Nothing here is guaranteed to work, be maintained, or survive the next cleanup
pass.

If something in here turns out to matter, distill the actual finding into
[`../studies/`](../studies/README.md) and let the experiment stay disposable (or delete it).

See the main [README](../README.md) for the project itself.

## Open questions worth an experiment, not yet run

The three BACKEND adapters (nixos, system-manager, nix-darwin) are implemented and covered
by [`../checks/`](../checks); the PROVISIONING registry is declared and has no caller. Every
claim below is reasoned, not measured.

- **The reimage path, exercised for real.**
  [`docs/design.md`](../docs/design.md#the-ratchet-hazard) argues a provisioning adapter has
  to be exercised regularly, not just registered, or a drifted machine has nowhere left to
  go. Nothing reads `nixdeploy.publisher.provisioning` yet, and the module renders no
  reimage command into the receiver's config, so there is no adapter — and no wired route —
  to test that argument against. See [`../docs/reimage.md`](../docs/reimage.md)'s "What is
  implemented".
- **The receiver-side reimage route, on a machine that is actually replaced.**
  `src/receive.rs`'s `route_over_ceiling` is covered end to end by `tests/pipeline_test.rs`
  against a scripted command. It has never run against a provider that actually destroyed
  the machine underneath it, which is the one condition its "the process may not survive
  this call" ordering exists for.
- **A `nix-darwin` receiver on a real Mac.** `checks/emission.nix` proves the launchd
  fragment's shape against a stub, because nix-darwin is deliberately not a flake input. No
  Mac has loaded the generated plist.
- **narinfo-sum cost on a genuinely small machine.** The receiver's sizing check (`.narinfo`
  metadata per missing path, summed) is cheap relative to a download, but unmeasured against
  an actual low-memory target with a manifest naming many new paths at once.
