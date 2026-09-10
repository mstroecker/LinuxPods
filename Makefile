.PHONY: all build build-release run run-debug test fmt lint clean

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

clean:
	cargo clean
