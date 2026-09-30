# Contributing

Thanks for helping improve ZeroSyntax v2. The project uses maintainer-reviewed pull
requests with CI as the merge gate.

## Development setup

Install Rust 1.75 or newer, Node.js 20 or newer, and npm.

```sh
cargo test --locked
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings

cd editors/vscode
npm ci
npm test
```

The optional real-game corpus is not committed. Fetch it only when you need the
ignored corpus test:

```sh
scripts/fetch-corpus.sh
cargo test --release -p zerosyntax-analysis --test corpus -- --ignored --nocapture
```

On Windows, use `pwsh scripts/fetch-corpus.ps1`.

## Performance

Run the existing synthetic benchmarks and real-server driver before changing a
hot path:

```sh
cargo bench -p zerosyntax-syntax --bench parse
cargo bench -p zerosyntax-analysis --bench analyze
cargo build --release -p zerosyntax-server
python crates/server/tests/typing_latency.py target/release/zerosyntax-lsp
```

Pull requests warn when a probe regresses by 20% and fail at 50%; loose absolute
latency ceilings catch emergency-level slowdowns. Profile a failing probe before
weakening its threshold.

## Pull requests

- Keep changes focused. Split unrelated fixes into separate PRs.
- Add or update the smallest test that would catch the behavior you changed.
- Run the commands above before requesting review.
- Do not commit `GeneralsCode/`, `corpus/`, `examples/`, build outputs,
  extension packages, secrets, or local editor state.

## Issue reports

For bugs, include a small INI sample, the expected behavior, the actual
diagnostic or editor behavior, and your OS/editor version. For feature requests,
describe the workflow the feature should improve.

## Releases

Maintainers release from the `dev` and `prod` branches. See the
[release process](docs/release.md).
