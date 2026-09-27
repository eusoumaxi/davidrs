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

## Branches, commits and pull requests

- Never commit to `main`. Work on a branch named `<type>/<description>`,
  such as `fix/42-jwks-refresh`; `scripts/check.sh branch` checks it.
- Commit only when asked. Messages follow Conventional Commits
  (`fix(queue): report unattempted records as failures`, subject at most 72
  characters, no trailing period) and end with a trailer naming the
  assistant, such as `Co-authored-by: <assistant> <address>`;
  `scripts/check.sh commit-msg <file>` checks a message.
- A pull request you open says that an assistant wrote it, in the template's
  AI section, and stays a draft until a person has reviewed it.
- Do not change the version in `Cargo.toml`, the dated sections of
  `CHANGELOG.md`, tags or `.github/workflows/release.yml` unless asked.
  Record user-visible changes under `## [Unreleased]`.
