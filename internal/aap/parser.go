package aap

import (
	"fmt"
)

// BatteryComponent represents which component the battery belongs to
type BatteryComponent uint8

const (
	ComponentUnknown BatteryComponent = 0
	ComponentRight   BatteryComponent = 2
	ComponentLeft    BatteryComponent = 4
	ComponentCase    BatteryComponent = 8
)

func (c BatteryComponent) String() string {
	switch c {
	case ComponentRight:
		return "Right"
	case ComponentLeft:
		return "Left"
	case ComponentCase:
		return "Case"
	default:
		return "Unknown"
	}
}

// BatteryStatus represents the charging status
type BatteryStatus uint8

const (
	StatusUnknown      BatteryStatus = 0
	StatusCharging     BatteryStatus = 1
	StatusDischarging  BatteryStatus = 2
	StatusDisconnected BatteryStatus = 4
)

func (s BatteryStatus) String() string {
	switch s {
	case StatusCharging:
		return "Charging"
	case StatusDischarging:
		return "Discharging"
	case StatusDisconnected:
		return "Disconnected"
	default:
		return "Unknown"
	}
}

// Battery represents a single battery component
type Battery struct {
	Component BatteryComponent
	Level     uint8
	Status    BatteryStatus
}

// BatteryInfo contains battery information for all components
type BatteryInfo struct {
	Left  *Battery
	Right *Battery
	Case  *Battery
}

// ParseBatteryPacket parses a battery status packet
// Format: 04 00 04 00 04 00 [count] ([component] 01 [level] [status] 01)...
func ParseBatteryPacket(packet []byte) (*BatteryInfo, error) {
	// Check minimum length and header
	if len(packet) < 7 {
		return nil, fmt.Errorf("packet too short")
	}

	// Check for battery packet header: 04 00 04 00 04 00
	if packet[0] != 0x04 || packet[1] != 0x00 ||
		packet[2] != 0x04 || packet[3] != 0x00 ||
		packet[4] != 0x04 || packet[5] != 0x00 {
		return nil, fmt.Errorf("not a battery packet")
	}

	count := packet[6]
	info := &BatteryInfo{}

	offset := 7
	for i := 0; i < int(count); i++ {
		// Need at least 5 bytes: [component] 01 [level] [status] 01
		if offset+5 > len(packet) {
			return nil, fmt.Errorf("incomplete battery data at offset %d", offset)
		}

		component := BatteryComponent(packet[offset])
		// Skip separator (should be 01)
		level := packet[offset+2]
		status := BatteryStatus(packet[offset+3])
		// Skip separator (should be 01)

		battery := &Battery{
			Component: component,
			Level:     level,
			Status:    status,
		}

		switch component {
		case ComponentLeft:
			info.Left = battery
		case ComponentRight:
			info.Right = battery
		case ComponentCase:
			info.Case = battery
		}

		offset += 5
	}

	return info, nil
}

func (bi *BatteryInfo) String() string {
	result := "Battery Status:\n"
	if bi.Left != nil {
		result += fmt.Sprintf("  Left:  %d%% (%s)\n", bi.Left.Level, bi.Left.Status)
	}
	if bi.Right != nil {
		result += fmt.Sprintf("  Right: %d%% (%s)\n", bi.Right.Level, bi.Right.Status)
	}
	if bi.Case != nil {
		result += fmt.Sprintf("  Case:  %d%% (%s)\n", bi.Case.Level, bi.Case.Status)
	}
	return result
}
