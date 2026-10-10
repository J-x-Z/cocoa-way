## Summary

What problem does this solve, and why this approach? Link a relevant issue or
prior discussion, especially for invasive dependency, transport, or Vulkan work.

## Reproduction

For fixes: minimal steps, expected behavior, and actual behavior before/after.
Include relevant diagnostics or launch logs, with private details removed.

## Environment

macOS version; Mac model/architecture; Rust version; relevant runtime, Waypipe,
and client versions; application/profile and Desktop or Rootless mode.

## Validation

List exact commands actually run and results (including failures). Mark skipped
or inapplicable checks with a reason; see [CONTRIBUTING.md](https://github.com/J-x-Z/cocoa-way/blob/main/CONTRIBUTING.md).

- [ ] `cargo fmt --all --check`
- [ ] `python3 scripts/vendor-smithay.py`
- [ ] `cargo test --workspace --locked`
- [ ] `cargo build --release --bins --locked`
- [ ] Relevant regression tests, including
  `python3 scripts/test-clipboard-relay.py` for clipboard relay changes.

Commands/results:

Manual validation for input/rendering/window changes: real application,
hardware/runtime, presentation mode, steps exercised, and observed results.
CI or a successful build alone is not manual validation.

Missing coverage/limitations (keep unvalidated hardware-dependent work **Draft**):

- [ ] This PR is focused; I understand and take responsibility for all changes,
  including any AI-assisted work, and have accurately reported validation.
