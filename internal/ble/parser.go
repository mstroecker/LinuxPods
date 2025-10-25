package ble

import "fmt"

const (
	proximityType = 0x07
)

// ProximityData represents Apple Continuity proximity pairing data.
// It can contain both unencrypted data (from BLE advertisement, ~10% accuracy)
// and decrypted data (if encryption key is available, 1% accuracy).
// When decrypted data is available, the battery/charging fields contain the
// more accurate decrypted values.
type ProximityData struct {
	DeviceModel     uint16
	Status          uint8
	LeftBattery     *uint8 // nil if unknown, accuracy depends on HasDecrypted
	RightBattery    *uint8 // nil if unknown, accuracy depends on HasDecrypted
	CaseBattery     *uint8 // nil if unknown, accuracy depends on HasDecrypted
	LeftCharging    bool
	RightCharging   bool
	CaseCharging    bool
	LeftInEar       bool
	RightInEar      bool
	LidOpen         bool
	Color           uint8
	ConnectionState uint8
	IsFlipped       bool   // true if right pod is primary
	RawData         []byte // raw unencrypted payload for debugging

	// Decrypted portion (only if encryption key was available)
	HasDecrypted bool   // true if decrypted data was processed
	RawDecrypted []byte // raw decrypted 16-byte payload for debugging
}

// ParseProximityData parses Apple Continuity proximity pairing advertisement.
// This function is exported for use in debugging tools.
func ParseProximityData(data []byte) (*ProximityData, error) {
	// Check minimum length and type
	if len(data) < 2 {
		return nil, fmt.Errorf("data too short")
	}

	// Check for proximity pairing type (0x07) and length
	if data[0] != proximityType {
		return nil, fmt.Errorf("not a proximity pairing message")
	}

	length := int(data[1])
	if len(data) < 2+length {
		return nil, fmt.Errorf("incomplete data")
	}

	payload := data[2 : 2+length]

	// Minimum payload: prefix(1) + model(2) + status(1) + battery(1) + charging(1) + case(1) + lid(1) + color(1) + suffix(1) = 10 bytes
	if len(payload) < 10 {
		return nil, fmt.Errorf("payload too short")
	}

	// Check prefix
	if payload[0] != 0x01 {
		return nil, fmt.Errorf("invalid prefix")
	}

	pd := &ProximityData{
		DeviceModel: uint16(payload[1])<<8 | uint16(payload[2]),
		Status:      payload[3],
		RawData:     append([]byte(nil), payload...), // Copy payload for debugging
	}

	// Parse color from byte 7
	if len(payload) > 7 {
		pd.Color = payload[7]
	}

	// Determine primary pod and orientation
	// Based on LibrePods implementation
	//
	// The AirPods broadcast which pod is "primary" (bit 5 of status byte).
	// When the right pod is primary (primaryLeft = false), the data is "flipped":
	// - Battery nibbles are swapped (left ↔ right)
	// - Charging bits are swapped (left ↔ right)
	// - Ear detection uses different bits
	//
	// This ensures that "left" and "right" values always correspond to the
	// physical left and right pods, regardless of which one is primary.
	statusByte := payload[3]
	primaryLeft := ((statusByte >> 5) & 0x01) == 1
	thisInCase := ((statusByte >> 6) & 0x01) == 1
	isFlipped := !primaryLeft
	xorFactor := primaryLeft != thisInCase // XOR operation for ear detection

	pd.IsFlipped = isFlipped

	// Parse battery levels from byte 4 using nibbles
	// Nibbles may be swapped based on orientation
	batteryByte := payload[4]
	var leftNibble, rightNibble uint8
	if isFlipped {
		leftNibble = batteryByte & 0x0F
		rightNibble = (batteryByte >> 4) & 0x0F
	} else {
		leftNibble = (batteryByte >> 4) & 0x0F
		rightNibble = batteryByte & 0x0F
	}
	pd.LeftBattery = DecodeBattery(leftNibble)
	pd.RightBattery = DecodeBattery(rightNibble)

	// Case battery from byte 5 - use simple decoding like AirPods batteries
	if len(payload) > 5 {
		caseBatteryRaw := payload[5]
		pd.CaseBattery = DecodeBattery(caseBatteryRaw & 0x0F)
	}

	// Parse charging status from byte 5
	chargingByte := payload[5]

	// | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 |
	// | ? | C | L | R | Case battery_ |
	// Second, third and fourth byte from the left
	pd.CaseCharging = ((chargingByte >> (8 - 2)) & 0x01) != 0
	pd.RightCharging = ((chargingByte >> (8 - 3)) & 0x01) != 0
	pd.LeftCharging = ((chargingByte >> (8 - 4)) & 0x01) != 0

	// Bits are swapped based on primary pod status
	if isFlipped {
		pd.LeftCharging, pd.RightCharging = pd.RightCharging, pd.LeftCharging
	}

	// Parse ear detection from status byte (byte 3)
	pd.LeftInEar = (statusByte & 0x08) != 0
	pd.RightInEar = (statusByte & 0x02) != 0
	// Bits may be swapped based on xorFactor
	if xorFactor {
		pd.LeftInEar, pd.RightInEar = pd.RightInEar, pd.LeftInEar
	}

	// Parse lid status from byte 8 (lid byte), bit 3
	// Based on LibrePods: ((lid >> 3) & 0x01) == 0 means lid is open
	// Encrypted?
	if len(payload) > 8 {
		lidByte := payload[8]
		pd.LidOpen = ((lidByte >> 3) & 0x01) == 0
	}

	// Parse connection state from byte 9
	// Encrypted?
	if len(payload) > 9 {
		pd.ConnectionState = payload[9]
	}

	return pd, nil
}

// AddDecryptedData merges decrypted battery data into an existing ProximityData struct.
// This overwrites the approximate battery levels from BLE with accurate (1%) levels.
//
// The decrypted format (based on LibrePods):
//
//	Byte 0: Unknown
//	Byte 1: First pod battery (bit 7 = charging, bits 0-6 = level)
//	Byte 2: Second pod battery (bit 7 = charging, bits 0-6 = level)
//	Byte 3: Case battery (bit 7 = charging, bits 0-6 = level)
//	Bytes 4-15: Unknown
//
// This method should be called after parsing the unencrypted portion.
func (pd *ProximityData) AddDecryptedData(decrypted []byte) error {
	if len(decrypted) != 16 {
		return fmt.Errorf("decrypted data must be 16 bytes, got %d", len(decrypted))
	}

	// Store raw decrypted data
	pd.HasDecrypted = true
	pd.RawDecrypted = append([]byte(nil), decrypted...) // Copy for debugging

	// Parse battery data from decrypted bytes
	if len(decrypted) >= 4 {
		// Byte 1 - First pod
		byte1 := int(decrypted[1])
		charging1 := (byte1 & 0x80) != 0
		battery1 := uint8(byte1 & 0x7F)

		// Byte 2 - Second pod
		byte2 := int(decrypted[2])
		charging2 := (byte2 & 0x80) != 0
		battery2 := uint8(byte2 & 0x7F)

		// Byte 3 - Case
		byte3 := int(decrypted[3])
		caseCharging := (byte3 & 0x80) != 0
		caseBattery := uint8(byte3 & 0x7F)

		// Check if batteries are valid (anything over 100% is invalid)
		var bat1Ptr, bat2Ptr *uint8
		if battery1 <= 100 {
			bat1Ptr = &battery1
		}
		if battery2 <= 100 {
			bat2Ptr = &battery2
		}

		// Assign batteries based on flip status
		// If NOT flipped: byte1=left, byte2=right
		// If flipped: byte1=right, byte2=left
		if pd.IsFlipped {
			pd.LeftBattery, pd.RightBattery = bat2Ptr, bat1Ptr
			pd.LeftCharging, pd.RightCharging = charging2, charging1
		} else {
			pd.LeftBattery, pd.RightBattery = bat1Ptr, bat2Ptr
			pd.LeftCharging, pd.RightCharging = charging1, charging2
		}

		// Case battery is independent of flip
		if caseBattery <= 100 {
			pd.CaseBattery = &caseBattery
			pd.CaseCharging = caseCharging
		} else {
			pd.CaseBattery = nil
		}
	}

	return nil
}

// DecodeBattery decodes a battery nibble value
// 0x0-0x9: 0-90% in 10% increments
// 0xA-0xE: 100%
// 0xF: unknown/not available (returns nil)
func DecodeBattery(nibble uint8) *uint8 {
	switch {
	case nibble <= 0x9:
		val := nibble * 10
		return &val
	case nibble <= 0xE:
		val := uint8(100)
		return &val
	default:
		return nil
	}
}

// DecodeColor decodes the color byte to a readable string
func DecodeColor(color uint8) string {
	switch color {
	case 0x00:
		return "White"
	case 0x01:
		return "Black"
	case 0x02:
		return "Red"
	case 0x03:
		return "Blue"
	case 0x04:
		return "Pink"
	case 0x05:
		return "Gray"
	case 0x06:
		return "Silver"
	case 0x07:
		return "Gold"
	case 0x08:
		return "Rose Gold"
	case 0x09:
		return "Space Gray"
	case 0x0A:
		return "Dark Blue"
	case 0x0B:
		return "Light Blue"
	case 0x0C:
		return "Yellow"
	default:
		return fmt.Sprintf("Unknown (0x%02X)", color)
	}
}

// DecodeConnectionState decodes the connection state byte to a readable string
func DecodeConnectionState(state uint8) string {
	switch state {
	case 0x00:
		return "Disconnected"
	case 0x04:
		return "Idle"
	case 0x05:
		return "Music"
	case 0x06:
		return "Call"
	case 0x07:
		return "Ringing"
	case 0x09:
		return "Hanging Up"
	case 0xFF:
		return "Unknown"
	default:
		return fmt.Sprintf("Unknown (0x%02X)", state)
	}
}

// DecodeModelName returns the human-readable model name for a device model code
func DecodeModelName(deviceModel uint16) string {
	switch deviceModel {
	case 0x0220:
		return "AirPods (2nd gen)"
	case 0x0e20:
		return "AirPods Pro"
	case 0x2420:
		return "AirPods Pro (2nd gen)"
	case 0x2720:
		return "AirPods Pro 3"
	default:
		return fmt.Sprintf("Unknown (0x%04X)", deviceModel)
	}
}
