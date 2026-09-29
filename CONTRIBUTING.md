# Contributing

`davidrs` is one crate with empty default features. Every rule below keeps it small, readable and verifiable, and `scripts/check.sh` enforces them: run it before you push; CI runs the same steps.

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md). Report security problems privately, as [SECURITY.md](SECURITY.md) explains, never in an issue.

## Design rules

- **Native first.** Use the official Lambda runtime, the AWS SDK and the standard HTTP types. Add an abstraction only for behaviour that repeats, or for an application policy that must vary independently (the extension points: `Policy`, `Admission`, `ErrorRenderer`, finalizers, `prepare`).
- **No web framework.** One Lambda serves one operation: no router, no middleware stack, no ORM, no service container.
- **Additive features.** A feature enables only what it names. Update `docs/features.md` when a feature's links change.
- **Keep the invariants:** one absolute deadline per invocation, bounded work, 5xx messages that cannot leak, honest partial outcomes, state and telemetry lifetimes owned by the caller.
- **Nothing application-specific.** No company, product, customer or business domain appears in code, docs, tests or fixtures. Examples use neutral nouns (`orders`, `items`, `users`) and `example.com`.
- **Rust conventions.** Names follow the [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/); `rustfmt` formats the code and `clippy` lints it, both with the settings committed here.

## Comments are documentation

All comments are rustdoc: `//!` for a module, `///` for an item. There are no plain `//` comments in `src/`, `tests/` or `examples/`. When a line of code needs an explanation, the explanation belongs in the doc comment of the item that contains it.

A doc comment is short and plain:

1. The first sentence says what the item is or does.
2. Then, only when it is not obvious, why: the mistake it prevents or the rule it follows.
3. `# Errors` for every fallible public function, `# Panics` if it can panic, `# Examples` for the items a user starts from.

Write for a developer reading the item for the first time. No history ("moved from", "used to", "legacy", "workaround"), no internal jargon, no first person.

## The guide

Chapters live in `docs/` and are included from `src/guide.rs`. Rustdoc on the item is the reference: it says what the item does. A guide chapter exists only when a reader still needs to know **when** to use it, **what to configure around it**, **what a caller or Lambda should observe**, and **what to change when that is not what they observe**.

A chapter that only restates the rustdoc should not be added. Update the reading map at the top of `src/guide.rs` when you add one, so the index stays a list of jobs rather than a list of files.

Write the chapter so that someone who has not seen the code can do all four of these without guessing:

1. Tell, from the first paragraph, whether this is the page for the job they have, which Cargo features to enable, and which other chapter to open instead.
2. Follow one working path. Name the response, the status or the Lambda behaviour they should see when it works.
3. Recognise the failure they are likely to hit — a platform setting left off, a feature unified away by the workspace, a `200` that did not do the whole batch — and know what to change.
4. Find the boundary: what this module will not do, and which other tool does that job.

Keep examples compilable. Rustdoc compiles Rust snippets as doctests; `no_run` snippets compile without executing network-dependent code. Prefer a runnable example to a paragraph that describes one. Neutral vocabulary, as in the design rules above. When a limit comes from AWS, cite the AWS page, as the security chapter does.

## Tests

- Tests live in `tests/`, one file per area, and exercise the **public API** only. `src/` contains no `#[cfg(test)]` code, so every source file reads as documentation plus implementation.
- A file that needs a feature opens with its `//!` summary, then `#![cfg(feature = "…")]` (in that order: a false `cfg` placed first would also remove the summary). It must pass with only that feature enabled as well as with `--all-features`.
- One behaviour per test, named as a sentence: `a_5xx_never_renders_its_message`. A test that needs a reason gets a `///` line, not a comment inside the body.
- Cover every public item and every error branch, once. Two tests that fail for the same reason are one test too many.
- No network, no AWS account: remote ends are the local server in `tests/support/server.rs` or the SDK's in-process test client. Timing assertions use generous margins.
- Rust examples in doc comments and the guide (`docs/*.md`) are doctests: ordinary blocks run, `no_run` blocks only compile. Prefer runnable, account-free examples.

## Checks

```bash
scripts/check.sh                  # everything CI runs
scripts/check.sh lint             # one step: rules, lint, test, features, docs, package, spelling, workflows, deny
scripts/check.sh coverage         # line coverage (needs cargo-llvm-cov)
git config core.hooksPath .githooks   # once per clone: check commit messages locally
```

Default features are empty, so an editor that analyses the default build greys out every feature-gated module and test. `.vscode/settings.json` makes rust-analyzer analyse all features and run clippy in VS Code and Cursor; in another editor, set rust-analyzer's `cargo.features` to `"all"`.

Develop with Rust 1.98.1 (pinned in `rust-toolchain.toml`) and the committed `Cargo.lock`. The crate supports Rust 1.94.1 and later (`rust-version`, the floor the AWS SDK sets): `scripts/check.sh msrv` compiles everything with it, so do not use a newer standard-library API without raising `rust-version` in the same change. The `spelling`, `workflows` and `deny` steps need [`typos`](https://github.com/crate-ci/typos), [`actionlint`](https://github.com/rhysd/actionlint) with [`shellcheck`](https://www.shellcheck.net), and [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny); `scripts/check.sh` skips them when they are not installed, CI never does.

## Branches

Work on a branch named `<type>/<description>`, never on `main`:

- `<type>` is one of the commit types below: `feat/sqs-visibility`, `fix/42-jwks-refresh`, `docs/getting-started`.
- `<description>` is lowercase words and digits joined by `-`, with an issue number first when there is one. A dot is allowed for a version: `chore/release-0.2.0`.

Reviewers check the name; no script does. Rename a branch with `git branch -m <type>/<description>`.

## Commit messages

Commits follow [Conventional Commits 1.0](https://www.conventionalcommits.org/en/v1.0.0/):

```text
<type>(<scope>): <description>

<body>

<footers>
```

- **type**: `feat` (a new capability), `fix` (a bug fix), `docs`, `test`, `refactor` (no change in behaviour), `perf`, `style` (formatting only), `build` (Cargo.toml, dependencies), `ci`, `chore` (anything else, such as a release), `revert`.
- **scope**: optional; the module or area in lowercase: `http`, `queue`, `mcp`, `dynamo`, `auth`, `deps`.
- **description**: imperative mood, lowercase unless it starts with a name, no trailing period: `fix(queue): report unattempted records as failures`. The whole subject is at most 72 characters.
- **body**: optional, after a blank line. Say what changes and why, wrapped at 72 characters.
- **footers**: `BREAKING CHANGE: <what breaks and how to migrate>` for a breaking change (or `!` after the type or scope: `feat(http)!: …`), and `Refs: #123`.

Git's own `Merge`, `Revert`, `fixup!` and `squash!` subjects are accepted as they are; squash fixups before review. `scripts/check.sh commit-msg` checks a message, the hook runs it on every commit, and CI checks every commit and the title of every pull request.

## Pull requests

1. Open an issue first for anything larger than a fix, so the design is agreed before the code.
2. Branch from `main`, keep one concern per pull request, and run `scripts/check.sh`.
3. Add an entry under `## [Unreleased]` in `CHANGELOG.md` for every change a user can notice.
4. Fill in the template, including the AI disclosure.
5. Pull requests are squash-merged: the title becomes the commit on `main`, so it follows the commit convention too.

A pull request needs the maintainer's review (`.github/CODEOWNERS`) and green checks.

## AI-assisted contributions

AI coding assistants are welcome. Their output meets the same bar as anyone's, and a person answers for every line.

- **A person owns every pull request.** The author has read and understood every line, can explain it in review, and has run `scripts/check.sh`. Pull requests opened by an agent with no human author are closed.
- **Disclose it.** Tick the AI box in the pull request template and name the assistant there. Commits carry only the name of the person who authors them: no assistant author, co-author or trailer.
- **Same rules.** Branch names, commit messages, rustdoc-only comments, neutral vocabulary and tests through the public API apply unchanged. Agents read them from [`AGENTS.md`](AGENTS.md).
- **Small and deliberate.** One concern per pull request. No generated churn: mass reformatting, speculative abstractions, reworded documentation that says nothing new, or tests that assert what the code happens to do rather than what it should do.
- **No invented facts.** Every API, flag, limit and link a change mentions must exist. Limits of AWS services cite the AWS documentation, as the guide does.
- **Evidence for reports.** Include reproduction steps when available. For a suspected security issue, identify the affected code and explain the possible impact even if you cannot safely reproduce it; disclose uncertainty instead of claiming a confirmed vulnerability.
- **Your right to submit it.** You confirm that you may contribute the change under the MIT licence, and that it does not reproduce code whose licence forbids that.

## Releases

For the maintainer. Versions follow [Semantic Versioning](https://semver.org): before 1.0, a minor version (`0.x`) may break the public API, and the changelog says how; a patch version never does.

Every push to `main` runs the full CI through `release.yml`. After CI succeeds,
changes to `src/`, `Cargo.toml`, `Cargo.lock`, `build.rs` or
`rust-toolchain.toml` since the latest release trigger an automatic release.
Documentation, tests, skills and workflow changes alone do not publish a crate.
[Release-plz](https://release-plz.dev) compares the package with crates.io,
updates the version, and checks API compatibility with `cargo-semver-checks`.
It writes no changelog entries: it adds the heading `## [X.Y.Z] - YYYY-MM-DD`
below `## [Unreleased]`, so the entries written there become the new
version's section and its release notes (`release-plz.toml`). Compatible
changes normally increment the patch version; breaking changes increment the
minor version before 1.0. Mark behavioural breaking changes with `!` or a
`BREAKING CHANGE` footer too, because API checks cannot detect every
compatibility change.

The workflow commits release metadata on a `chore/release-<version>` branch,
advances `main` without rewriting history, publishes with Trusted Publishing,
and creates the tag, GitHub release and documentation from the same commit.
A newer `main` commit supersedes a queued preparation. GitHub's workflow token
does not trigger another push workflow when it writes release metadata.
The `release` environment must allow `main` and tags `v*`; `github-pages`
must allow both too. Never add the local `.env` token as a workflow secret.

For an explicit version or a retry, the manual tag workflow remains available:

1. On a branch such as `chore/release-0.2.0`, set `version` in `Cargo.toml`, rename `## [Unreleased]` in `CHANGELOG.md` to `## [0.2.0] - YYYY-MM-DD`, add a new empty `## [Unreleased]` above it, and update the links at the end of the file. Merge it as `chore(release): 0.2.0`.
2. Tag the merged commit on `main` and push the tag:

   ```bash
   git switch main && git pull
   git tag -a v0.2.0 -m "davidrs 0.2.0"
   git push origin v0.2.0
   ```

3. The release workflow runs CI, checks that the tag, `Cargo.toml` and `CHANGELOG.md` agree and that the commit is on `main`, publishes the crate to crates.io, creates the GitHub release from the changelog section and rebuilds the documentation site. docs.rs builds the API reference by itself.

Once per repository:

- **First publish.** crates.io only accepts trusted publishing for a crate that already exists, so publish the first version from your machine with `cargo publish`. Then, on crates.io, add a trusted publisher to the crate: repository `eusoumaxi/davidrs`, workflow `release.yml`, environment `release`. Push the tag afterwards: the workflow sees the version is already published and only creates the release and the site.
- **Environments.** In the repository settings, protect the `release` environment (for example, with a required reviewer), and allow `main` and tags `v*` to deploy to `github-pages`.

## Licence

`davidrs` is released under the [MIT licence](LICENSE). Dependencies retain their own licences; preserve the required notices when distributing them. `cargo deny` checks dependency licences against `deny.toml`. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in the crate is licensed under the same terms, without any additional terms or conditions.
