// debug_aap_key_retrieval is a debugging tool for retrieving encryption keys from AirPods.
//
// This tool connects to AirPods via the AAP client and retrieves the proximity
// pairing encryption keys (IRK and ENC_KEY) needed to decrypt the encrypted portion
// of BLE proximity pairing advertisements.
//
// Usage:
//
//	go run ./cmd/debug_aap_key_retrieval <MAC_ADDRESS>
//	Example: go run ./cmd/debug_aap_key_retrieval 90:62:3F:59:00:2F
//
// The tool will:
// 1. Connect to AirPods via AAP (L2CAP PSM 4097)
// 2. Send handshake packet
// 3. Request proximity keys
// 4. Parse and display IRK (Identity Resolving Key) and ENC_KEY
//
// These keys can then be used to decrypt the encrypted 16-byte payload
// in BLE proximity pairing advertisements (last 16 bytes).
//
// Based on: https://github.com/kavishdevar/librepods/blob/main/proximity_keys.py
package main

import (
	"encoding/hex"
	"fmt"
	"log"
	"os"

	"linuxpods/internal/aap"
)

// readProximityKeys reads packets from the AirPods until a key response is received.
// The AirPods may send several non-key packets before the key packet arrives.
//
// This function will block until:
//   - A key packet is received and successfully parsed (returns keys, nil)
//   - maxAttempts packets have been read without finding keys (returns nil, error)
//   - A read error occurs (returns nil, error)
func readProximityKeys(client *aap.Client, maxAttempts int) ([]aap.ProximityKey, error) {
	for attempt := 1; attempt <= maxAttempts; attempt++ {
		packet, err := client.ReadPacket()
		if err != nil {
			return nil, fmt.Errorf("failed to read packet (attempt %d/%d): %w", attempt, maxAttempts, err)
		}

		if !aap.IsKeyPacket(packet) {
			continue // Not a key packet, keep waiting
		}

		keys, err := aap.ParseProximityKeys(packet)
		if err != nil {
			return nil, fmt.Errorf("failed to parse key packet: %w", err)
		}

		return keys, nil
	}

	return nil, fmt.Errorf("no key packet received after %d attempts", maxAttempts)
}

// retrieveProximityKeys is a convenience function that combines RequestProximityKeys()
// and readProximityKeys() into a single call.
//
// This function:
//  1. Sends the key request packet
//  2. Waits for and parses the key response (up to maxAttempts packets)
//  3. Returns the parsed keys
//
// The client must be connected, and handshake must be completed before calling this.
func retrieveProximityKeys(client *aap.Client, maxAttempts int) ([]aap.ProximityKey, error) {
	if err := client.RequestProximityKeys(); err != nil {
		return nil, err
	}

	return readProximityKeys(client, maxAttempts)
}

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintf(os.Stderr, "Usage: %s <MAC_ADDRESS>\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "Example: %s 90:62:3F:59:00:2F\n", os.Args[0])
		os.Exit(1)
	}

	macAddr := os.Args[1]
	log.Printf("Retrieving proximity keys from %s...", macAddr)

	// Create AAP client
	client, err := aap.NewClient(macAddr)
	if err != nil {
		log.Fatalf("Failed to create client: %v", err)
	}
	defer client.Close()

	// Connect
	log.Println("Connecting to AirPods...")
	if err := client.Connect(); err != nil {
		log.Fatalf("Failed to connect: %v", err)
	}

	// Handshake
	log.Println("Sending handshake...")
	if err := client.Handshake(); err != nil {
		log.Fatalf("Failed to handshake: %v", err)
	}

	// Retrieve keys
	log.Println("Requesting proximity keys...")
	keys, err := retrieveProximityKeys(client, 100)
	if err != nil {
		log.Fatalf("Failed to retrieve keys: %v", err)
	}

	// Display keys
	fmt.Println()
	fmt.Println("=== Extracted Keys ===")
	for i, key := range keys {
		fmt.Printf("\nKey %d:\n", i+1)
		fmt.Printf("  Type: %s\n", key.Type)
		fmt.Printf("  Data: %s\n", hex.EncodeToString(key.Data))
	}
	fmt.Println()

	// Highlight the encryption key
	encKey := aap.FindEncryptionKey(keys)
	if encKey != nil {
		fmt.Println("✅ Use this key for BLE decryption:")
		fmt.Printf("   %s\n", hex.EncodeToString(encKey))
		fmt.Println()
		fmt.Println("Test with:")
		fmt.Printf("  ./bin/debug_decrypt_test %s\n", hex.EncodeToString(encKey))
		fmt.Printf("  ./bin/debug_ble %s\n", hex.EncodeToString(encKey))
	}

	log.Println("✅ Keys successfully retrieved!")
}
