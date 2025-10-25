.PHONY: all build run clean fmt test tools

# Default target
all: fmt build

# Build the main GUI application
build:
	go build -o linuxpods ./cmd/gui

# Build with race detector (for development)
build-race:
	go build -race -o linuxpods ./cmd/gui

# Run the application
run:
	./linuxpods

# Run with GTK inspector (for UI debugging)
run-debug:
	GTK_DEBUG=interactive ./linuxpods

# Build debugging tools
tools:
	go build -o bin/debug_ble ./cmd/debug_ble
	go build -o bin/debug_aap ./cmd/debug_aap
	go build -o bin/debug_bluez_dbus_battery ./cmd/debug_bluez_dbus_battery
	go build -o bin/debug_bluez_dbus_discover ./cmd/debug_bluez_dbus_discover
	go build -o bin/debug_aap_key_retrieval ./cmd/debug_aap_key_retrieval
	go build -o bin/debug_decrypt ./cmd/debug_decrypt

# Format code
fmt:
	go fmt ./...

# Run tests
test:
	go test ./...

# Clean build artifacts
clean:
	rm -f linuxpods
	rm -rf bin/

# Download dependencies
deps:
	go mod download
	go mod tidy
