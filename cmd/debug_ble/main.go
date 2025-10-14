// debug_ble is a debugging tool that scans for Apple AirPods BLE advertisements.
//
// This tool passively monitors Bluetooth Low Energy advertisements from AirPods,
// parsing the Apple Continuity proximity pairing protocol to extract battery levels,
// charging status, and in-ear detection without requiring an active connection.
//
// Usage:
//
//	go run ./cmd/debug_ble
//
// The scanner works even when AirPods are connected to another device (like an iPhone),
// making it useful for testing BLE advertisement parsing and understanding the protocol.
// Battery level accuracy is approximate (5-10% granularity).
//
// Press Ctrl+C to stop scanning.
package main

import (
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"
	"time"

	"linuxpods/internal/ble"
)

func main() {
	log.Println("=== AirPods BLE Scanner ===")
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
			data, err := scanner.ScanForAirPods(5 * time.Second)
			if err != nil {
				log.Printf("  No AirPods found in this scan window")
				continue
			}

			fmt.Println()
			fmt.Println("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")
			fmt.Println(data.String())
			fmt.Println("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━")
			fmt.Println()
		}
	}
}
