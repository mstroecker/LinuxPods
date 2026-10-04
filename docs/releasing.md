# Releasing

A release is a `v*` tag on `main`. Pushing the tag runs
`.github/workflows/release.yml`, which builds, attests and publishes everything;
nothing is uploaded by hand.

## Steps

1. Bump the version and update the changelog on a branch:

   ```bash
   git switch -c release/v0.2.0
   # set version = "0.2.0" in Cargo.toml
   cargo check                              # updates Cargo.lock
   git cliff --tag v0.2.0 --output CHANGELOG.md
   git commit -am "chore(release): v0.2.0"
   ```

   Edit `CHANGELOG.md` if a generated entry reads badly; the release notes come
   from the commits, so fix the wording there as well for later releases.

2. Open a pull request and merge it once CI passes.

3. Tag the merge commit and push the tag:

   ```bash
   git switch main && git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```

The workflow refuses a tag that does not match the version in `Cargo.toml` or
does not point at a commit on `main`. Once published, a release and its tag
cannot be changed: fix a broken release with a new version.

## What a release contains

| File | Content |
|---|---|
| `linuxpods-<version>-x86_64-linux.tar.gz`, `…-aarch64-linux.tar.gz` | binary, `install.sh`, icons, LICENSE, README |
| `linuxpods-<version>.cdx.json` | CycloneDX SBOM |
| `SHA256SUMS` | checksums of the tarballs and the SBOM |
| `linuxpods-<version>.sigstore.json` | SLSA build provenance bundle |

The binaries are built with `cargo auditable`, which embeds the dependency list,
on Ubuntu 24.04, so they need glibc 2.39 or newer.

## Verifying a release

```bash
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify linuxpods-0.2.0-x86_64-linux.tar.gz -R mstroecker/LinuxPods \
  --signer-workflow mstroecker/LinuxPods/.github/workflows/release-build.yml
```

The attestation proves the file was built by `release-build.yml` from the tagged
commit. The tarballs also carry an SBOM attestation
(`--predicate-type https://cyclonedx.org/bom`).
