// debug_aap_noise_control is a validation tool for testing AAP noise control commands.
//
// This tool connects to AirPods via AAP and sends noise control mode change packets
// to verify the reverse-engineered packet format from the LibrePods project. It cycles
// through all four modes (Off, ANC, Transparency, Adaptive) and captures the response
// packets for each.
//
// Usage:
//
//	go run ./cmd/debug_aap_noise_control <MAC_ADDRESS>
//
// Example:
//
//	go run ./cmd/debug_aap_noise_control 90:62:3F:59:00:2F
//
// The tool will:
//  1. Connect and perform the standard AAP handshake
//  2. Enable special features (required for Adaptive mode)
//  3. Drain initial packets (battery status, etc.)
//  4. Send each noise control mode and capture the response
//  5. Report results with full hex dumps
//
// Requirements:
//   - AirPods must be paired and connected to this Linux device
//   - BlueZ Bluetooth stack must be running
//
// Press Ctrl+C to abort at any time.
package main

import (
	"encoding/hex"
	"fmt"
	"log"
	"os"
	"time"

	"linuxpods/internal/aap"
)

// noiseMode maps mode names to their byte values (from LibrePods, unverified)
var noiseModes = []struct {
	name string
	mode byte
}{
	{"Off", 0x01},
	{"Noise Cancellation (ANC)", 0x02},
	{"Transparency", 0x03},
	{"Adaptive", 0x04},
}

func main() {
	if len(os.Args) < 2 {
		fmt.Println("Usage: debug_aap_noise_control <MAC_ADDRESS>")
		fmt.Println()
		fmt.Println("Example: debug_aap_noise_control 90:62:3F:59:00:2F")
		fmt.Println()
		fmt.Println("Tests noise control mode switching over AAP protocol.")
		fmt.Println("Cycles through Off, ANC, Transparency, and Adaptive modes.")
		os.Exit(1)
	}

	macAddr := os.Args[1]

	log.Println("=== AAP Noise Control Validation Tool ===")
	log.Printf("Target device: %s\n\n", macAddr)

	// --- Step 1: Connect ---
	client, err := aap.NewClient(macAddr)
	if err != nil {
		log.Fatalf("Failed to create client: %v", err)
	}

	log.Println("1. Opening L2CAP connection (PSM 4097)...")
	if err := client.Connect(); err != nil {
		log.Fatalf("Failed to connect: %v", err)
	}
	defer client.Close()
	log.Println("   OK - Connected")

	// --- Step 2: Handshake ---
	log.Println("\n2. Sending handshake...")
	if err := client.Handshake(); err != nil {
		log.Fatalf("Failed to send handshake: %v", err)
	}
	log.Println("   OK - Handshake sent")
	time.Sleep(500 * time.Millisecond)

	// --- Step 3: Request battery (standard setup) ---
	log.Println("\n3. Requesting battery status...")
	if err := client.RequestBatteryStatus(); err != nil {
		log.Fatalf("Failed to request battery: %v", err)
	}
	log.Println("   OK - Battery request sent")
	time.Sleep(200 * time.Millisecond)

	// --- Step 4: Enable features (required for Adaptive) ---
	log.Println("\n4. Enabling special features (required for Adaptive mode)...")
	if err := client.EnableSpecialFeatures(); err != nil {
		log.Fatalf("Failed to enable features: %v", err)
	}
	log.Println("   OK - Features enabled")
	time.Sleep(500 * time.Millisecond)

	// --- Step 5: Drain initial response packets ---
	log.Println("\n5. Draining initial response packets...")
	drainPackets(client, 2*time.Second)

	// --- Step 6: Test each noise control mode ---
	log.Println("\n6. Testing noise control modes...")
	log.Println("   Expected packet format: 04 00 04 00 09 00 0D [mode] 00 00 00")
	log.Println()

	for i, nm := range noiseModes {
		log.Printf("--- Test %d/%d: %s (mode=0x%02X) ---\n", i+1, len(noiseModes), nm.name, nm.mode)

		// Build and display the command packet
		cmdPacket := buildNoiseControlPacket(nm.mode)
		log.Printf("   SEND: %s", hex.EncodeToString(cmdPacket))

		// Send the command
		if err := client.SetNoiseControl(nm.mode); err != nil {
			log.Printf("   FAIL: Send error: %v", err)
			continue
		}
		log.Println("   OK - Packet sent")

		// Read ALL response packets for 4 seconds (don't break early — need to
		// distinguish stale state reports from actual confirmations)
		responses := readResponsePackets(client, 4*time.Second)

		if len(responses) == 0 {
			log.Println("   WARN: No response packets received")
		} else {
			confirmed := false
			for j, resp := range responses {
				log.Printf("   RECV[%d]: %s (%d bytes)", j+1, hex.EncodeToString(resp), len(resp))

				if isNoiseControlModePacket(resp) {
					respMode := resp[7]
					log.Printf("   ^^^^ NOISE CONTROL MODE PACKET (sub=0x0D, mode=0x%02X)", respMode)
					if respMode == nm.mode {
						log.Println("   PASS: Confirmed mode matches sent mode")
						confirmed = true
					} else {
						log.Printf("   STALE/MISMATCH: Expected 0x%02X, got 0x%02X", nm.mode, respMode)
					}
				} else if len(resp) >= 7 && resp[4] == 0x09 {
					// Other 0x09 settings sub-command
					log.Printf("   (Settings packet: cmd=0x09, sub=0x%02X)", resp[6])
				} else if len(resp) >= 5 && resp[4] == 0x4B {
					log4BPacket(resp)
				} else if aap.IsBatteryPacket(resp) {
					batteryInfo, err := aap.ParseBatteryPacket(resp)
					if err == nil {
						log.Printf("   (Battery update: %s)", batteryInfo.String())
					}
				} else {
					log.Printf("   (Unknown packet type, cmd byte: 0x%02X)", safeGetByte(resp, 4))
				}
			}
			if !confirmed {
				log.Printf("   RESULT: No matching confirmation received (expected mode 0x%02X)", nm.mode)
			}
		}

		// Pause between tests — drain any remaining packets from the previous mode change
		if i < len(noiseModes)-1 {
			log.Println("   Draining before next test (3s)...")
			drainPackets(client, 3*time.Second)
		}
		log.Println()
	}

	log.Println("=== Validation Complete ===")
	log.Println("Check above output for PASS/FAIL/MISMATCH results.")
	log.Println("Listen for audible mode changes on the AirPods to confirm.")
}

// buildNoiseControlPacket constructs the expected packet for display purposes
func buildNoiseControlPacket(mode byte) []byte {
	return []byte{0x04, 0x00, 0x04, 0x00, 0x09, 0x00, 0x0D, mode, 0x00, 0x00, 0x00}
}

// drainPackets reads and discards packets for the given duration
func drainPackets(client *aap.Client, timeout time.Duration) {
	deadline := time.Now().Add(timeout)
	count := 0
	for time.Now().Before(deadline) {
		// Use a short read with the remaining time
		packet := readWithTimeout(client, time.Until(deadline))
		if packet == nil {
			break
		}
		count++
		log.Printf("   Drained packet #%d (%d bytes): %s", count, len(packet), hex.EncodeToString(packet))

		if aap.IsBatteryPacket(packet) {
			batteryInfo, err := aap.ParseBatteryPacket(packet)
			if err == nil {
				log.Printf("   (Battery: %s)", batteryInfo.String())
			}
		} else if isNoiseControlModePacket(packet) {
			log.Printf("   (Noise control state: mode=0x%02X)", packet[7])
		} else if len(packet) >= 5 && packet[4] == 0x4B {
			log4BPacket(packet)
		} else if len(packet) >= 7 && packet[4] == 0x09 {
			log.Printf("   (Settings packet: cmd=0x09, sub=0x%02X, data=0x%02X)", packet[6], safeGetByte(packet, 7))
		}
	}
	log.Printf("   Drained %d initial packet(s)", count)
}

// isNoiseControlModePacket checks if a packet is specifically a noise control mode
// report (cmd=0x09, sub-cmd=0x0D). The 0x09 command family has many sub-commands;
// only 0x0D is noise control.
func isNoiseControlModePacket(packet []byte) bool {
	return len(packet) >= 8 && packet[4] == 0x09 && packet[6] == 0x0D
}

// readResponsePackets reads ALL response packets within the timeout period.
// Does not break early — collects everything so we can distinguish stale state
// reports from actual confirmations.
func readResponsePackets(client *aap.Client, timeout time.Duration) [][]byte {
	var responses [][]byte
	deadline := time.Now().Add(timeout)

	for time.Now().Before(deadline) {
		packet := readWithTimeout(client, time.Until(deadline))
		if packet == nil {
			break
		}
		responses = append(responses, packet)
	}
	return responses
}

// readWithTimeout attempts to read a packet with a timeout.
// Returns nil if the timeout expires.
func readWithTimeout(client *aap.Client, timeout time.Duration) []byte {
	done := make(chan []byte, 1)
	go func() {
		packet, err := client.ReadPacket()
		if err != nil {
			done <- nil
			return
		}
		done <- packet
	}()

	select {
	case packet := <-done:
		return packet
	case <-time.After(timeout):
		return nil
	}
}

// log4BPacket logs a detailed breakdown of a 0x4B packet.
// Known format so far: 04 00 04 00 4B 00 LL 00 DD DD
// where LL may be a length/type and DD DD are data bytes.
func log4BPacket(packet []byte) {
	log.Printf("   ^^^^ 0x4B PACKET BREAKDOWN:")
	log.Printf("        Header:  %s", hex.EncodeToString(packet[0:4]))
	log.Printf("        Cmd:     0x%02X", packet[4])
	if len(packet) > 5 {
		log.Printf("        Byte 5:  0x%02X", packet[5])
	}
	if len(packet) > 6 {
		log.Printf("        Byte 6:  0x%02X (possible length/type)", packet[6])
	}
	if len(packet) > 7 {
		log.Printf("        Byte 7:  0x%02X", packet[7])
	}
	if len(packet) > 8 {
		log.Printf("        Data[8:]: %s", hex.EncodeToString(packet[8:]))
	}
	// Check if trailing bytes reference the settings family
	for i := 6; i < len(packet); i++ {
		if packet[i] == 0x09 {
			log.Printf("        ** Byte %d = 0x09 (settings family reference?)", i)
		}
		if packet[i] == 0x0D {
			log.Printf("        ** Byte %d = 0x0D (noise control sub-cmd reference?)", i)
		}
	}
}

// safeGetByte safely retrieves a byte from a slice, returning 0 if out of bounds
func safeGetByte(data []byte, index int) byte {
	if index < len(data) {
		return data[index]
	}
	return 0
}
