# Contributing

This repository is one of three that change together; the process lives in NovaPlayer's
[`CONTRIBUTING.md`](https://github.com/wuuuming/NovaPlayer/blob/main/CONTRIBUTING.md) and its rules in
[`docs/principles.md`](https://github.com/wuuuming/NovaPlayer/blob/main/docs/principles.md). In short:

- Build, `cargo clippy --release --workspace --all-targets` and `cargo test --release` (the unit
  tests) before a commit; `cargo fmt` keeps the style. Integration tests and their fixtures
  are maintained separately.
- A bug fix brings one regression test that fails before the fix and passes after, at the
  lowest level that can see it. Refactors add no tests.
- A change here is tested, committed and pushed first; NovaPlayer then pins the new commit
  together with the change that needs it.
- Every `unsafe` block states its contract in a `// SAFETY:` comment.

Contributions are licensed as the repository is: MIT OR Apache-2.0, without additional terms.
