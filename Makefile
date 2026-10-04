.PHONY: all build build-release run run-debug test fmt lint secrets deny sbom hooks clean install uninstall dist

# Default target
all: fmt build

# Build the application
build:
	cargo build

build-release:
	cargo build --release

# Run the application
run:
	cargo run

# Run with the GTK inspector for UI debugging
run-debug:
	GTK_DEBUG=interactive RUST_LOG=linuxpods=debug cargo run

# Run with protocol tracing (BLE parse/decrypt, AAP packets)
run-trace:
	RUST_LOG=linuxpods=debug cargo run

test:
	cargo test

fmt:
	cargo fmt

lint:
	cargo clippy --all-targets

# Scan the full git history for secrets (rules: .gitleaks.toml)
secrets:
	gitleaks git --redact --no-banner .

# Check dependencies for advisories, licenses and sources (rules: deny.toml)
deny:
	@command -v cargo-deny >/dev/null || { echo "cargo-deny not found: cargo install --locked cargo-deny"; exit 1; }
	cargo deny --locked check

# Software bill of materials for the release binary (CycloneDX JSON). Dev- and
# build-only dependencies are left out: they never reach the binary. The
# timestamp is the last commit's, so the same commit gives the same file.
SBOM_DIR = target/sbom
sbom:
	@command -v cargo-cyclonedx >/dev/null || { echo "cargo-cyclonedx not found: cargo install --locked cargo-cyclonedx"; exit 1; }
	SOURCE_DATE_EPOCH=$$(git log -1 --format=%ct) cargo cyclonedx --format json \
		--spec-version 1.5 --describe binaries --no-build-deps
	mkdir -p $(SBOM_DIR)
	mv linuxpods_bin.cdx.json $(SBOM_DIR)/linuxpods.cdx.json
	@echo "SBOM: $(SBOM_DIR)/linuxpods.cdx.json"

# Install the pre-commit secret scan into this checkout
hooks:
	ln -sf ../../scripts/pre-commit "$$(git rev-parse --git-path hooks)/pre-commit"

clean:
	cargo clean

# Install for the current user: binary, icons, launcher and autostart entries
# (scripts/install.sh, which the release tarball ships too). PREFIX defaults to
# ~/.local, so none of this needs sudo.
install: build-release
	BIN=target/release/linuxpods ICONS=assets/icons scripts/install.sh

uninstall:
	scripts/install.sh --uninstall

# Release tarball for this machine's architecture: dist/linuxpods-<version>-<arch>-linux.tar.gz
dist: build-release
	scripts/package.sh
