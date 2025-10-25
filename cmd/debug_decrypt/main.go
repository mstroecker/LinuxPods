// debug_decrypt_test is a simple test tool for parsing and decrypting BLE payloads.
//
// This tool takes hardcoded BLE data and shows the unencrypted fields.
// If an encryption key is provided, it will also decrypt the encrypted portion.
// Useful for quick testing and verification of parsing and decryption.
//
// Usage:
//
//	go run ./cmd/debug_decrypt_test [ENCRYPTION_KEY]
//
// Examples:
//
//	# Show only unencrypted data
//	go run ./cmd/debug_decrypt_test
//
//	# Show unencrypted + decrypted data
//	go run ./cmd/debug_decrypt_test a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6
package main

import (
	"encoding/hex"
	"fmt"
	"log"
	"os"

	"linuxpods/internal/ble"
)

// Test payload - full Apple Continuity proximity pairing advertisement
var testPayloadGood2 = []byte{
	// Type and length header
	0x07, 0x19,
	// Unencrypted portion (9 bytes) + encrypted portion (16 bytes)
	0x01, 0x27, 0x20, 0x0b, 0x99, 0x8f, 0x11, 0x00, 0x05,
	0x63, 0xfc, 0xfb, 0xb4, 0x39, 0x01, 0x1c, 0x61, 0xe7,
	0xe4, 0xaa, 0x95, 0x83, 0x2c, 0x5b, 0x57,
}

var testPayloadGood3 = []byte{
	// Type and length header
	0x07, 0x19,
	// Unencrypted portion (9 bytes) + encrypted portion (16 bytes)
	0x01, 0x27, 0x20, 0x55, 0xaa, 0xb0, 0x39, 0x00, 0x00,
	0x44, 0x34, 0xe2, 0xff, 0xf0, 0xd9, 0x1b, 0xc4, 0x48,
	0xad, 0xab, 0x2f, 0x38, 0x2c, 0x5a, 0x39,
}

var testPayloadGood = []byte{ // Bad
	// Type and length header
	0x07, 0x19,
	// Unencrypted portion (9 bytes) + encrypted portion (16 bytes)
	0x01, 0x24, 0x20, 0x55, 0xaa, 0xb4, 0x39, 0x00, 0x04,
	0xa7, 0x4f, 0xba, 0xd3, 0xc6, 0xfa, 0xd2, 0x67, 0xba,
	0xa6, 0x62, 0x49, 0xc4, 0x13, 0x84, 0x8f,
}

func main() {
	if len(os.Args) > 2 {
		_, _ = fmt.Fprintf(os.Stderr, "Usage: %s [ENCRYPTION_KEY]\n", os.Args[0])
		_, _ = fmt.Fprintf(os.Stderr, "Example: %s a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6\n", os.Args[0])
		_, _ = fmt.Fprintf(os.Stderr, "\nIf no encryption key is provided, only unencrypted data will be shown.\n")
		os.Exit(1)
	}

	// Parse encryption key if provided
	var encryptionKey []byte
	var err error
	hasKey := false

	if len(os.Args) == 2 {
		keyHex := os.Args[1]
		encryptionKey, err = hex.DecodeString(keyHex)
		if err != nil {
			log.Fatalf("Invalid encryption key format: %v", err)
		}
		if len(encryptionKey) != 16 {
			log.Fatalf("Encryption key must be 16 bytes (32 hex characters), got %d bytes", len(encryptionKey))
		}
		hasKey = true
	}

	fmt.Println("=== BLE Decryption Test ===")
	if hasKey {
		fmt.Printf("Encryption key: %s\n", hex.EncodeToString(encryptionKey))
	} else {
		fmt.Println("No encryption key provided - showing unencrypted data only")
	}
	fmt.Println()

	// Show full test payload
	fmt.Printf("Full payload (%d bytes): %s\n", len(testPayloadGood), hex.EncodeToString(testPayloadGood))
	fmt.Println()

	// Parse the unencrypted portion using the parser
	fmt.Println("=== Parsing Unencrypted BLE Advertisement ===")
	data, err := ble.ParseProximityData(testPayloadGood)
	if err != nil {
		log.Fatalf("Failed to parse payload: %v", err)
	}

	// Show unencrypted data interpretation (uses parser's String() method)
	fmt.Println(data.String())
	fmt.Println()

	// Show raw unencrypted bytes with detailed breakdown
	fmt.Println("=== Unencrypted Raw Bytes (Detailed) ===")
	showUnencryptedBytes(data)
	fmt.Println()

	// Only proceed with decryption if encryption key was provided
	if !hasKey {
		fmt.Println("=== Encrypted Payload ===")
		fmt.Println("Skipped - no encryption key provided")
		fmt.Println()
		fmt.Println("To decrypt, run with an encryption key:")
		fmt.Printf("  %s <ENCRYPTION_KEY>\n", os.Args[0])
		return
	}

	// Extract and decrypt encrypted portion
	if len(testPayloadGood) < 18 { // 0x07, 0x19 + 16 bytes encrypted minimum
		log.Fatalf("Payload too short for encrypted data")
	}

	// Last 16 bytes are encrypted
	encryptedData := testPayloadGood[len(testPayloadGood)-16:]
	fmt.Println("=== Encrypted Payload (last 16 bytes) ===")
	fmt.Printf("Encrypted: %s\n", hex.EncodeToString(encryptedData))
	fmt.Println()

	// Decrypt
	decrypted, err := ble.DecryptProximityPayload(encryptedData, encryptionKey)
	if err != nil {
		log.Fatalf("Decryption error: %v", err)
	}

	fmt.Println("=== Decrypted Data ===")
	fmt.Printf("Decrypted: %s\n", hex.EncodeToString(decrypted))
	fmt.Println()

	// Merge decrypted data into the ProximityData (uses parser's AddDecryptedData method)
	if err := data.AddDecryptedData(decrypted); err != nil {
		log.Fatalf("Failed to merge decrypted data: %v", err)
	}

	// Show final merged result (uses parser's String() method)
	fmt.Println("=== Final Merged Data (Unencrypted + Decrypted) ===")
	fmt.Println(data.String())
	fmt.Println()

	// Show detailed battery byte analysis
	fmt.Println("=== Decrypted Battery Bytes (Detailed) ===")
	analyzeBatteryBytes(decrypted, data.IsFlipped)
	fmt.Println()

	// Full breakdown of all decrypted bytes
	fmt.Println("=== All 16 Decrypted Bytes ===")
	for i, b := range decrypted {
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

// showUnencryptedBytes shows detailed breakdown of unencrypted fields
func showUnencryptedBytes(pd *ble.ProximityData) {
	rawData := pd.RawData
	if len(rawData) < 9 {
		fmt.Println("Unencrypted data too short")
		return
	}

	fmt.Printf("Byte 0 (Prefix):       0x%02X\n", rawData[0])
	fmt.Printf("Bytes 1-2 (Model):     0x%04X\n", pd.DeviceModel)
	fmt.Printf("Byte 3 (Status):       0x%02X (%08b)\n", pd.Status, pd.Status)

	// Status byte breakdown
	statusByte := pd.Status
	primaryLeft := ((statusByte >> 5) & 0x01) == 1
	fmt.Printf("  Bit 5 (primary):     %v (left pod is primary: %v)\n", primaryLeft, primaryLeft)
	fmt.Printf("  Bit 6 (in case):     %v\n", ((statusByte>>6)&0x01) != 0)
	fmt.Printf("  Bit 3 (left ear):    %v\n", pd.LeftInEar)
	fmt.Printf("  Bit 1 (right ear):   %v\n", pd.RightInEar)

	// Parse battery from byte 4 using parser's DecodeBattery
	leftNibble := (rawData[4] >> 4) & 0x0F
	rightNibble := rawData[4] & 0x0F
	fmt.Printf("Byte 4 (Battery):      0x%02X (nibbles: left=0x%X, right=0x%X)\n",
		rawData[4], leftNibble, rightNibble)

	// Show decoded values using parser
	leftBat := ble.DecodeBattery(leftNibble)
	rightBat := ble.DecodeBattery(rightNibble)
	fmt.Printf("  Decoded left:        ")
	if leftBat != nil {
		fmt.Printf("%d%%\n", *leftBat)
	} else {
		fmt.Printf("Unknown\n")
	}
	fmt.Printf("  Decoded right:       ")
	if rightBat != nil {
		fmt.Printf("%d%%\n", *rightBat)
	} else {
		fmt.Printf("Unknown\n")
	}

	fmt.Printf("Byte 5 (Charging):     0x%02X (%08b)\n", rawData[5], rawData[5])
	fmt.Printf("  Bit 6 (case):        %v\n", pd.CaseCharging)
	fmt.Printf("  Bit 5 (left):        %v\n", pd.LeftCharging)
	fmt.Printf("  Bit 4 (right):       %v\n", pd.RightCharging)

	fmt.Printf("Byte 6 (Lid counter?): 0x%02X\n", rawData[6])
	fmt.Printf("Byte 7 (Color):        0x%02X (%s)\n", pd.Color, ble.DecodeColor(pd.Color))
	if len(rawData) > 8 {
		fmt.Printf("Byte 8 (Lid/Conn):     0x%02X\n", rawData[8])
		if pd.LidOpen {
			fmt.Printf("  Lid: Open\n")
		} else {
			fmt.Printf("  Lid: Closed\n")
		}
	}
}

// analyzeBatteryBytes shows detailed breakdown of decrypted battery bytes
func analyzeBatteryBytes(data []byte, isFlipped bool) {
	if len(data) < 4 {
		fmt.Println("Data too short for battery analysis")
		return
	}

	// Byte 1 - First pod
	byte1 := int(data[1])
	charging1 := (byte1 & 0x80) != 0
	battery1 := byte1 & 0x7F

	// Byte 2 - Second pod
	byte2 := int(data[2])
	charging2 := (byte2 & 0x80) != 0
	battery2 := byte2 & 0x7F

	// Byte 3 - Case
	byte3 := int(data[3])
	caseCharging := (byte3 & 0x80) != 0
	caseBattery := byte3 & 0x7F
	caseValid := byte3 != 0xFF && !(caseCharging && caseBattery == 127)

	fmt.Printf("Byte 1: 0x%02X (%08b)\n", data[1], data[1])
	fmt.Printf("  Bit 7 (charging): %v\n", charging1)
	fmt.Printf("  Bits 0-6 (level): %d%%\n", battery1)
	fmt.Println()

	fmt.Printf("Byte 2: 0x%02X (%08b)\n", data[2], data[2])
	fmt.Printf("  Bit 7 (charging): %v\n", charging2)
	fmt.Printf("  Bits 0-6 (level): %d%%\n", battery2)
	fmt.Println()

	fmt.Printf("Byte 3: 0x%02X (%08b)\n", data[3], data[3])
	fmt.Printf("  Bit 7 (charging): %v\n", caseCharging)
	fmt.Printf("  Bits 0-6 (level): %d%%\n", caseBattery)
	fmt.Printf("  Valid: %v", caseValid)
	if !caseValid {
		fmt.Printf(" (0xFF or charging+127 = unavailable)")
	}
	fmt.Println()
	fmt.Println()

	// Show correct interpretation based on flip status (parser handles this automatically)
	orientation := "NOT flipped (left pod is primary)"
	if isFlipped {
		orientation = "flipped (right pod is primary)"
	}
	fmt.Printf("Actual interpretation (%s):\n", orientation)

	if isFlipped {
		// When flipped: byte1=right, byte2=left
		fmt.Printf("  Left:  Byte 2 → %d%%", battery2)
		if charging2 {
			fmt.Printf(" ⚡")
		}
		fmt.Println()

		fmt.Printf("  Right: Byte 1 → %d%%", battery1)
		if charging1 {
			fmt.Printf(" ⚡")
		}
		fmt.Println()
	} else {
		// When NOT flipped: byte1=left, byte2=right
		fmt.Printf("  Left:  Byte 1 → %d%%", battery1)
		if charging1 {
			fmt.Printf(" ⚡")
		}
		fmt.Println()

		fmt.Printf("  Right: Byte 2 → %d%%", battery2)
		if charging2 {
			fmt.Printf(" ⚡")
		}
		fmt.Println()
	}

	if caseValid {
		fmt.Printf("  Case:  Byte 3 → %d%%", caseBattery)
		if caseCharging {
			fmt.Printf(" ⚡")
		}
		fmt.Println()
	} else {
		fmt.Printf("  Case:  N/A\n")
	}
}
