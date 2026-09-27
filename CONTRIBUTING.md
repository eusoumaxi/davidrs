# Contributing

`davidrs` is one crate with empty default features. Every rule below keeps it small, readable and verifiable. `scripts/check.sh` enforces all of them; run it before pushing, CI runs the same script.

## Design rules

- **Native first.** Use the official Lambda runtime, the AWS SDK and the standard HTTP types. Add an abstraction only for behaviour that repeats, or for an application policy that must vary independently (the extension points: `Policy`, `Admission`, `ErrorRenderer`, finalizers, `prepare`).
- **No web framework.** One Lambda serves one operation: no router, no middleware stack, no ORM, no service container.
- **Additive features.** A feature enables only what it names. Update `docs/features.md` when a feature's links change.
- **Keep the invariants:** one absolute deadline per invocation, bounded work, 5xx messages that cannot leak, honest partial outcomes, state and telemetry lifetimes owned by the caller.
- **Nothing application-specific.** No company, product, customer or business domain appears in code, docs, tests or fixtures. Examples use neutral nouns (`orders`, `items`, `users`) and `example.com`.

## Comments are documentation

All comments are rustdoc: `//!` for a module, `///` for an item. There are no plain `//` comments in `src/`, `tests/` or `examples/`. When a line of code needs an explanation, the explanation belongs in the doc comment of the item that contains it.

A doc comment is short and plain:

1. The first sentence says what the item is or does.
2. Then, only when it is not obvious, why: the mistake it prevents or the rule it follows.
3. `# Errors` for every fallible public function, `# Panics` if it can panic, `# Examples` for the items a user starts from.

Write for a developer reading the item for the first time. No history ("moved from", "used to", "legacy"), no internal jargon.

## Tests

- Tests live in `tests/`, one file per area, and exercise the **public API** only. `src/` contains no `#[cfg(test)]` code, so every source file reads as documentation plus implementation.
- A file that needs a feature opens with its `//!` summary, then `#![cfg(feature = "…")]` (in that order: a false `cfg` placed first would also remove the summary). It must pass with only that feature enabled as well as with `--all-features`.
- One behaviour per test, named as a sentence: `a_5xx_never_renders_its_message`. A test that needs a reason gets a `///` line, not a comment inside the body.
- Cover every public item and every error branch, once. Two tests that fail for the same reason are one test too many.
- No network, no AWS account: remote ends are the local server in `tests/support/server.rs` or the SDK's in-process test client. Timing assertions use generous margins.
- Examples in doc comments and in the guide (`docs/*.md`) are compiled and run as doctests; prefer a runnable example to prose.

## Checks

```bash
scripts/check.sh            # everything CI runs
scripts/check.sh coverage   # line coverage per file (needs cargo-llvm-cov)
```

Use Rust 1.98.1 (pinned in `rust-toolchain.toml`) and the committed `Cargo.lock`.

## Licence

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this crate by you, as defined in the Apache-2.0
license, shall be dual licensed under the MIT and Apache-2.0 licences,
without any additional terms or conditions.
