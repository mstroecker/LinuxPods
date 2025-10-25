// debug_ble is a debugging tool that scans for Apple AirPods BLE advertisements.
//
// This tool passively monitors Bluetooth Low Energy advertisements from AirPods,
// parsing the Apple Continuity proximity pairing protocol to extract battery levels,
// charging status, and in-ear detection without requiring an active connection.
//
// Usage:
//
//	go run ./cmd/debug_ble [ENCRYPTION_KEY]
//
// Examples:
//
//	# Show unencrypted data only (~10% battery accuracy)
//	go run ./cmd/debug_ble
//
//	# Show decrypted data (1% battery accuracy)
//	go run ./cmd/debug_ble a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6
//
// The scanner works even when AirPods are connected to another device (like an iPhone),
// making it useful for testing BLE advertisement parsing and understanding the protocol.
//
// Press Ctrl+C to stop scanning.
package main

import (
	"encoding/hex"
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	"linuxpods/internal/ble"
)

func main() {
	// Parse optional encryption key
	var encryptionKey []byte
	hasKey := false

	if len(os.Args) > 2 {
		fmt.Fprintf(os.Stderr, "Usage: %s [ENCRYPTION_KEY]\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "\nExamples:\n")
		fmt.Fprintf(os.Stderr, "  %s                                    # Unencrypted data only\n", os.Args[0])
		fmt.Fprintf(os.Stderr, "  %s a1b2c3...d6  # With decryption\n", os.Args[0])
		os.Exit(1)
	}

	if len(os.Args) == 2 {
		keyHex := os.Args[1]
		var err error
		encryptionKey, err = hex.DecodeString(keyHex)
		if err != nil {
			log.Fatalf("Invalid encryption key format: %v", err)
		}
		if len(encryptionKey) != 16 {
			log.Fatalf("Encryption key must be 16 bytes (32 hex characters), got %d bytes", len(encryptionKey))
		}
		hasKey = true
	}

	log.Println("=== AirPods BLE Scanner ===")
	if hasKey {
		log.Printf("Decryption: ENABLED (1%% battery accuracy)")
		log.Printf("Key: %s\n", hex.EncodeToString(encryptionKey))
	} else {
		log.Println("Decryption: DISABLED (~10% battery accuracy)")
	}
	log.Println("Scanning for AirPods advertisements (passive, no connection required)")
	log.Println()

	// Create scanner
	scanner, err := ble.NewScanner()
	if err != nil {
		log.Fatalf("Failed to create scanner: %v", err)
	}
	defer scanner.Close()

	// Start discovery
	log.Println("Starting BLE discovery...")
	if err := scanner.StartDiscovery(); err != nil {
		log.Fatalf("Failed to start discovery: %v", err)
	}
	defer scanner.StopDiscovery()

	log.Println("✓ Scanning for AirPods advertisements...")
	log.Println("  (This works even if AirPods are connected to another device)")
	log.Println()

	// Handle Ctrl+C
	sigChan := make(chan os.Signal, 1)
	signal.Notify(sigChan, os.Interrupt, syscall.SIGTERM)

	// Scan loop
	ticker := time.NewTicker(5 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-sigChan:
			log.Println("\nStopping scanner...")
			return

		case <-ticker.C:
			// Try to scan for AirPods
			log.Println("Scanning...")
			data, tempMacAdress, err := scanner.ScanForAirPods(5 * time.Second)
			if err != nil {
				log.Printf("  No AirPods found in this scan window")
				continue
			}

			// If encryption key is available, decrypt and merge
			if hasKey && len(data.RawData) >= 16 {
				// Extract encrypted portion (last 16 bytes)
				encryptedData := data.RawData[len(data.RawData)-16:]

				// Decrypt
				decrypted, err := ble.DecryptProximityPayload(encryptedData, encryptionKey)
				if err != nil {
					log.Printf("⚠️  Decryption failed: %v", err)
				} else {
					// Merge decrypted data into ProximityData
					if err := data.AddDecryptedData(decrypted); err != nil {
						log.Printf("⚠️  Failed to merge decrypted data: %v", err)
					}
				}
			}

			// Display the data (will show "Decrypted" accuracy if decryption succeeded)
			fmt.Println()
			fmt.Println("━━━━━━━━━━ %s ━━━━━━━━━━━━", tempMacAdress)
			fmt.Println(data.String())
			fmt.Println("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")
			fmt.Println()

			if data.HasDecrypted {
				// Full breakdown of all decrypted bytes
				fmt.Println("=== All 16 Decrypted Bytes ===")
				for i, b := range data.RawDecrypted {
					fmt.Printf("Byte %2d: 0x%02X (%3d) %08b", i, b, b, b)

					// Add annotations
					switch i {
					case 1:
						fmt.Printf("  ← First pod battery")
					case 2:
						fmt.Printf("  ← Second pod battery")
					case 3:
						fmt.Printf("  ← Case battery")
					}
					fmt.Println()
				}
			}
		}
	}
}
