### Summary

<!-- One or two sentences: what does this PR do and why? -->

### What changed

<!-- Bullet list of the concrete changes (files/modules/behavior). -->

-
-

### Testing done

- [ ] `cargo fmt` run
- [ ] `cargo clippy` run
- [ ] `cargo test` passes
- [ ] If a provider integration was touched: ran the relevant live test locally against the real API, e.g.
      `cargo test -- --ignored --nocapture --test-threads=1` (requires a local `my.keys.env` with real keys; never run this in CI)

Describe any manual testing (which provider(s), which settings, what you spoke/typed, what you observed):

### Checklist

- [ ] `cargo fmt` is clean
- [ ] `cargo clippy -- -D warnings` is clean
- [ ] `cargo test` passes
- [ ] No secrets committed (no API keys, no personal `settings.json` — only `settings.example.json` should be tracked)
