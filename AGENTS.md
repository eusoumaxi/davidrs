# Working in davidrs

One Rust crate: a small framework for AWS Lambda functions. Read README.md,
CONTRIBUTING.md and the guide in `docs/` first.

- Follow CONTRIBUTING.md: rustdoc-only comments, tests in `tests/` through
  the public API, neutral vocabulary, empty default features.
- Application policy goes through the extension points (`Policy`,
  `Admission`, `ErrorRenderer`, finalizers, `prepare`), never into the crate.
- Use the native AWS, Lambda and HTTP libraries. One handler needs no router.
- Tests and examples never need real credentials or an AWS account.
- Run `scripts/check.sh` before reporting a change as done.
