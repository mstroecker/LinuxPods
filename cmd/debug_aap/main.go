// debug_aap is a debugging tool for testing the Apple Accessory Protocol (AAP) implementation.
//
// This tool establishes a direct L2CAP connection to AirPods on PSM 4097 and communicates
// using Apple's proprietary AAP protocol to retrieve accurate battery status (1% precision),
// in-ear detection, and other device information.
//
// Usage:
//
//	go run ./cmd/debug_aap <MAC_ADDRESS>
//
// Example:
//
//	go run ./cmd/debug_aap 90:62:3F:59:00:2F
//
// Requirements:
//   - AirPods must be paired and connected to this Linux device via Bluetooth
//   - BlueZ Bluetooth stack must be running
//
// The tool performs a full AAP handshake, enables battery notifications, and continuously
// reads packets from the AirPods, parsing and displaying battery information in real-time.
// This is useful for debugging the AAP protocol implementation and understanding packet formats.
//
// Press Ctrl+C to stop and disconnect.
package main

import (
	"fmt"
	"log"
	"os"
	"time"

	"linuxpods/internal/aap"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Println("Usage: aap <MAC_ADDRESS>")
		fmt.Println()
		fmt.Println("Example: aap 90:62:3F:59:00:2F")
		fmt.Println()
		fmt.Println("This tool connects to AirPods via the Apple Accessory Protocol (AAP)")
		fmt.Println("to retrieve battery status and other proprietary features.")
		os.Exit(1)
	}

	macAddr := os.Args[1]

	log.Printf("=== AAP Client for AirPods ===\n")
	log.Printf("Connecting to: %s\n\n", macAddr)

	// Create AAP client
	client, err := aap.NewClient(macAddr)
	if err != nil {
		log.Fatalf("Failed to create client: %v", err)
	}

	// Connect to AirPods
	log.Println("1. Opening L2CAP connection (PSM 4097)...")
	if err := client.Connect(); err != nil {
		log.Fatalf("Failed to connect: %v", err)
	}
	defer client.Close()
	log.Println("   ✓ Connected successfully")

	// Send handshake
	log.Println("\n2. Sending handshake packet...")
	if err := client.Handshake(); err != nil {
		log.Fatalf("Failed to send handshake: %v", err)
	}
	log.Println("   ✓ Handshake sent")

	// Wait a bit for handshake to process
	time.Sleep(500 * time.Millisecond)

	// Request battery status
	log.Println("\n3. Requesting battery status notifications...")
	if err := client.RequestBatteryStatus(); err != nil {
		log.Fatalf("Failed to request battery: %v", err)
	}
	log.Println("   ✓ Battery notifications enabled")

	// Wait a bit
	time.Sleep(200 * time.Millisecond)

	// Enable special features
	log.Println("\n4. Enabling special features...")
	if err := client.EnableSpecialFeatures(); err != nil {
		log.Fatalf("Failed to enable features: %v", err)
	}
	log.Println("   ✓ Special features enabled")

	// Read responses
	log.Println("\n5. Reading packets from AirPods...")
	log.Println("   (Press Ctrl+C to stop)\n")

	packetCount := 0
	for {
		packet, err := client.ReadPacket()
		if err != nil {
			log.Printf("Error reading packet: %v", err)
			continue
		}

		packetCount++
		log.Printf("Packet #%d (%d bytes): %s", packetCount, len(packet), aap.DumpPacket(packet))

		// Try to parse battery packet
		batteryInfo, err := aap.ParseBatteryPacket(packet)
		if err == nil {
			log.Printf("\n✨ %s", batteryInfo.String())
		} else {
			log.Printf("Parse error: %v", err)
		}
	}
}
