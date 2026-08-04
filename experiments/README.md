# experiments

Throwaway trials: spikes, one-off scripts, things tried and abandoned or not yet worth
writing up. Nothing here is guaranteed to work, be maintained, or survive the next cleanup
pass.

If something in here turns out to matter, distill the actual finding into
[`../studies/`](../studies/README.md) and let the experiment stay disposable (or delete it).

See the main [README](../README.md) for the project itself.

## Open questions worth an experiment, not yet run

nixdeploy v1 is a fresh scaffold with no adapter yet implemented for any real backend or
provider; every claim below is reasoned, not tested.

- **The reimage path, exercised for real.**
  [`docs/design.md`](../docs/design.md#the-ratchet-hazard) argues a provisioning adapter has
  to be exercised regularly, not just registered, or a drifted machine has nowhere left to
  go. No adapter exists yet to test that argument against.
- **narinfo-sum cost on a genuinely small machine.** The receiver's sizing check (`.narinfo`
  metadata per missing path, summed) is cheap relative to a download, but unmeasured against
  an actual low-memory target with a manifest naming many new paths at once.
