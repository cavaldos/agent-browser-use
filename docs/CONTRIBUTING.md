# Contributing

## Releasing (maintainers)

```sh
make release V=v1.2.3         # or: scripts/release.sh v1.2.3 (--dry-run to only run the checks)
```

The script runs on `main` only, needs a clean tree that matches `origin/main`, runs vet and tests, then creates
and pushes the annotated tag. The tag starts [.github/workflows/release.yml](.github/workflows/release.yml), which builds
`darwin` and `linux` binaries (arm64 and amd64) with the version stamped in, and publishes the GitHub Release with
`SHA256SUMS`. Every push and pull request runs [.github/workflows/ci.yml](.github/workflows/ci.yml) (gofmt, vet, tests on Linux and macOS).

