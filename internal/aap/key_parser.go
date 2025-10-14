package aap

import (
	"fmt"
)

// ProximityKeyType represents the type of encryption key
type ProximityKeyType uint8

const (
	KeyTypeUnknown ProximityKeyType = 0x00
	KeyTypeIRK     ProximityKeyType = 0x01 // Identity Resolving Key
	KeyTypeENCKEY  ProximityKeyType = 0x04 // Encryption Key
)

// String returns the human-readable name of the key type
func (k ProximityKeyType) String() string {
	switch k {
	case KeyTypeIRK:
		return "IRK (Identity Resolving Key)"
	case KeyTypeENCKEY:
		return "ENC_KEY (Encryption Key)"
	default:
		return fmt.Sprintf("UNKNOWN (0x%02X)", uint8(k))
	}
}

// ProximityKey represents a single encryption key retrieved from AirPods
type ProximityKey struct {
	Type ProximityKeyType
	Data []byte
}

// IsKeyPacket checks if a packet contains proximity key data
// A packet contains keys if:
//   - Length >= 7 bytes
//   - Byte [4] == 0x31 (key data marker)
func IsKeyPacket(packet []byte) bool {
	return len(packet) >= 7 && packet[4] == 0x31
}

// ParseProximityKeys parses encryption keys from an AAP key response packet
//
// Packet format:
//
//	Offset 0-1:  Header (04 00)
//	Offset 2-3:  Command type (04 00)
//	Offset 4:    Key data marker (0x31)
//	Offset 5:    Unknown
//	Offset 6:    Key count
//
// For each key:
//
//	+0:  Key type (0x01=IRK, 0x04=ENC_KEY)
//	+1:  Unknown
//	+2:  Key length (bytes)
//	+3:  Unknown
//	+4:  Key data (length bytes)
//
// Returns a slice of ProximityKey structs, or an error if parsing fails.
func ParseProximityKeys(packet []byte) ([]ProximityKey, error) {
	if len(packet) < 7 {
		return nil, fmt.Errorf("packet too short (need at least 7 bytes, got %d)", len(packet))
	}

	// Check for key data marker
	if packet[4] != 0x31 {
		return nil, fmt.Errorf("not a key packet (byte[4]=0x%02X, expected 0x31)", packet[4])
	}

	keyCount := int(packet[6])
	if keyCount == 0 {
		return nil, fmt.Errorf("no keys in packet (key count = 0)")
	}

	if keyCount > 10 {
		return nil, fmt.Errorf("suspicious key count: %d (expected 1-10)", keyCount)
	}

	keys := make([]ProximityKey, 0, keyCount)
	offset := 7

	for i := 0; i < keyCount; i++ {
		// Check if we have enough data for key header (4 bytes)
		if offset+3 >= len(packet) {
			return nil, fmt.Errorf("packet too short for key %d header (offset=%d, len=%d)", i+1, offset, len(packet))
		}

		// Parse key header
		keyType := ProximityKeyType(packet[offset])
		keyLength := int(packet[offset+2])

		// Skip 4-byte header
		offset += 4

		// Check if we have enough data for key
		if offset+keyLength > len(packet) {
			return nil, fmt.Errorf("packet too short for key %d data (need %d bytes, have %d)", i+1, keyLength, len(packet)-offset)
		}

		// Extract key data
		keyData := make([]byte, keyLength)
		copy(keyData, packet[offset:offset+keyLength])

		keys = append(keys, ProximityKey{
			Type: keyType,
			Data: keyData,
		})

		offset += keyLength
	}

	return keys, nil
}

// FindEncryptionKey searches for the ENC_KEY in a slice of proximity keys.
// Returns the key data if found, or nil if not found.
// The ENC_KEY (type 0x04) is the primary key used for decrypting BLE advertisements.
func FindEncryptionKey(keys []ProximityKey) []byte {
	for _, key := range keys {
		if key.Type == KeyTypeENCKEY {
			return key.Data
		}
	}
	return nil
}

// FindIRK searches for the IRK in a slice of proximity keys.
// Returns the key data if found, or nil if not found.
// The IRK (type 0x01) is used for resolving Bluetooth addresses.
func FindIRK(keys []ProximityKey) []byte {
	for _, key := range keys {
		if key.Type == KeyTypeIRK {
			return key.Data
		}
	}
	return nil
}
