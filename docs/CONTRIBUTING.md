# Contributing

## Releasing (maintainers)

```sh
make release V=v1.2.3         # or: scripts/release.sh v1.2.3 (--dry-run to only run the checks)
```

The script runs on `main` only, needs a clean tree that matches `origin/main`, runs fmt, clippy and tests, then creates
and pushes the annotated tag. The tag starts [.github/workflows/release.yml](.github/workflows/release.yml), which builds
raw binaries (`agent-browser-use-<os>-<arch>[.exe]`) for darwin and linux (arm64, amd64) and windows (amd64) on
native runners, with the version stamped in and `cloudflared` embedded, smoke-tests each one, and publishes the GitHub Release. Every push and pull request runs [.github/workflows/ci.yml](.github/workflows/ci.yml) (rustfmt, clippy, tests on Linux and macOS).


## Development

```sh
cd app
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

`make test-release` (or `ARGS=--quick`) dry-runs the whole release pipeline locally without tagging.
