# Contributing

Cocoa-Way has one maintainer. Small, focused contributions with clear validation
help keep review manageable. Bug reports and documentation fixes are welcome too.

## Scope and responsibility

- Keep each PR focused on one problem; avoid unrelated cleanup or formatting.
- Discuss invasive dependency changes (including vendored Smithay), transport
  changes, or Vulkan work in an issue before starting implementation.
- AI-assisted changes are welcome. As the author, you must review and understand
  the changes, validate them, and take responsibility for their correctness.
  Generated explanations or tests are not a substitute for running checks.

## Reproduction and evidence

For bug reports and fixes, give minimal reproduction steps, expected behavior,
and actual behavior before and after the change. Include macOS version, Mac
model/architecture, Rust version, and relevant runtime/Waypipe/client versions,
application profile, and Desktop or Rootless mode. Attach relevant diagnostics
and launch logs, with secrets and private details removed.

Record the exact commands you actually ran and their results, including failures.
State what was skipped and why; documentation-only changes can omit code checks.

## Validation checklist

Use stable Rust on macOS. The dependencies and checks below match
[CI](.github/workflows/ci.yml); install dependencies if needed:

```sh
brew install libxkbcommon pkg-config
```

- [ ] `cargo fmt --all --check`
- [ ] `python3 scripts/vendor-smithay.py` (Python 3.11+) verifies vendored sources.
- [ ] `cargo test --workspace --locked`
- [ ] `cargo build --release --bins --locked`
- [ ] Add or update regression tests where practical.
- [ ] For clipboard relay changes, run `python3 scripts/test-clipboard-relay.py`
  with coreutils `timeout` available on `PATH`.
- [ ] For input, rendering, or window changes, run the changed build with a real
  Wayland application on the affected Mac/runtime. Exercise the changed behavior
  and nearby regressions: keyboard/pointer/focus, drawing/scaling, or window
  resize/fullscreen/close as relevant. Check Desktop and Rootless when affected.
  Report the application, steps, and observed results; builds and CI alone do not
  validate these paths.

Keep hardware-dependent work **Draft** until validated on the affected hardware
and runtime. Describe missing coverage so someone with the right setup can help;
do not present untested behavior as verified.
