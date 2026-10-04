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
   git cliff --unreleased --tag v0.2.0 --prepend CHANGELOG.md
   git commit -am "chore(release): v0.2.0"
   ```

   `--prepend` adds only the new section, so earlier entries keep any edits.
   Edit the new one freely: the release notes are taken from this section of
   `CHANGELOG.md`.

2. Open a pull request and merge it once CI passes.

3. Tag the merge commit and push the tag:

   ```bash
   git switch main && git pull
   git tag v0.2.0
   git push origin v0.2.0
   ```

The workflow refuses a tag that does not match the version in `Cargo.toml`,
does not point at a commit on `main`, or has no section in `CHANGELOG.md`. Once published, a release and its tag
cannot be changed: fix a broken release with a new version.

## What a release contains

| File | Content |
|---|---|
| `linuxpods-<version>-x86_64-linux.tar.gz`, `…-aarch64-linux.tar.gz` | binary, `install.sh`, icons, LICENSE, README |
| `linuxpods-<version>-x86_64-linux.cdx.json`, `…-aarch64-linux.cdx.json` | CycloneDX SBOM of each build |
| `SHA256SUMS` | checksums of the tarballs and the SBOMs |
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
commit. Each tarball also carries an attestation of its own build's SBOM
(`--predicate-type https://cyclonedx.org/bom`).
