// Package ble provides Bluetooth Low Energy scanning for Apple Continuity advertisements.
//
// This package scans for BLE advertisements from AirPods and other Apple devices
// without requiring an active connection. This allows reading battery levels
// while the AirPods are connected to another device (like an iPhone).
//
// # Important Accuracy Note
//
// BLE advertisements provide APPROXIMATE battery levels that may be 5-10% off
// from actual values. The advertisements update slowly and are not real-time.
// For accurate battery readings, use the AAP (Apple Accessory Protocol) client
// which requires an active L2CAP connection.
//
// The implementation uses BlueZ D-Bus API to:
//   - Start BLE discovery
//   - Monitor advertisement packets
//   - Parse Apple manufacturer data (company ID 0x004C)
//   - Extract proximity pairing information
package ble

import (
	"fmt"
	"time"

	"github.com/godbus/dbus/v5"
)

const (
	bluezService   = "org.bluez"
	adapterPath    = "/org/bluez/hci0"
	appleCompanyID = 0x004C
	proximityType  = 0x07
)

// Scanner handles BLE advertisement scanning
type Scanner struct {
	conn   *dbus.Conn
	signal chan *dbus.Signal
}

// ProximityData represents Apple Continuity proximity pairing data
type ProximityData struct {
	DeviceModel     uint16
	Status          uint8
	LeftBattery     *uint8 // nil if unknown
	RightBattery    *uint8 // nil if unknown
	CaseBattery     *uint8 // nil if unknown
	LeftCharging    bool
	RightCharging   bool
	CaseCharging    bool
	LeftInEar       bool
	RightInEar      bool
	LidOpen         bool
	Color           uint8
	ConnectionState uint8
	IsFlipped       bool   // true if right pod is primary
	RawData         []byte // raw payload for debugging
}

// NewScanner creates a new BLE scanner
func NewScanner() (*Scanner, error) {
	conn, err := dbus.ConnectSystemBus()
	if err != nil {
		return nil, fmt.Errorf("failed to connect to system bus: %w", err)
	}

	return &Scanner{
		conn:   conn,
		signal: make(chan *dbus.Signal, 10),
	}, nil
}

// StartDiscovery begins BLE scanning
func (s *Scanner) StartDiscovery() error {
	obj := s.conn.Object(bluezService, adapterPath)

	// Set discovery filter for LE only
	filter := map[string]interface{}{
		"Transport": "le",
	}

	if err := obj.Call("org.bluez.Adapter1.SetDiscoveryFilter", 0, filter).Err; err != nil {
		return fmt.Errorf("failed to set discovery filter: %w", err)
	}

	// Start discovery
	if err := obj.Call("org.bluez.Adapter1.StartDiscovery", 0).Err; err != nil {
		return fmt.Errorf("failed to start discovery: %w", err)
	}

	// Subscribe to PropertiesChanged signals
	rule := "type='signal',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged'"
	if err := s.conn.BusObject().Call("org.freedesktop.DBus.AddMatch", 0, rule).Err; err != nil {
		return fmt.Errorf("failed to add match rule: %w", err)
	}

	s.conn.Signal(s.signal)

	return nil
}

// StopDiscovery stops BLE scanning
func (s *Scanner) StopDiscovery() error {
	obj := s.conn.Object(bluezService, adapterPath)
	return obj.Call("org.bluez.Adapter1.StopDiscovery", 0).Err
}

// ScanForAirPods scans for AirPods advertisements and returns proximity data
func (s *Scanner) ScanForAirPods(timeout time.Duration) (*ProximityData, error) {
	timer := time.NewTimer(timeout)
	defer timer.Stop()

	for {
		select {
		case <-timer.C:
			return nil, fmt.Errorf("scan timeout")

		case signal := <-s.signal:
			if signal.Name != "org.freedesktop.DBus.Properties.PropertiesChanged" {
				continue
			}

			if len(signal.Body) < 2 {
				continue
			}

			iface, ok := signal.Body[0].(string)
			if !ok || iface != "org.bluez.Device1" {
				continue
			}

			changes, ok := signal.Body[1].(map[string]dbus.Variant)
			if !ok {
				continue
			}

			// Check for manufacturer data
			if mfgDataVar, ok := changes["ManufacturerData"]; ok {
				mfgData, ok := mfgDataVar.Value().(map[uint16]dbus.Variant)
				if !ok {
					continue
				}

				// Look for Apple manufacturer data
				if appleDataVar, ok := mfgData[appleCompanyID]; ok {
					appleData, ok := appleDataVar.Value().([]byte)
					if !ok {
						continue
					}

					// Parse proximity pairing data
					if data, err := parseProximityData(appleData); err == nil {
						return data, nil
					}
				}
			}
		}
	}
}

// decodeBattery decodes a battery nibble value
// 0x0-0x9: 0-90% in 10% increments
// 0xA-0xE: 100%
// 0xF: unknown/not available (returns nil)
func decodeBattery(nibble uint8) *uint8 {
	switch {
	case nibble <= 0x9:
		val := nibble * 10
		return &val
	case nibble >= 0xA && nibble <= 0xE:
		val := uint8(100)
		return &val
	case nibble == 0xF:
		return nil
	default:
		return nil
	}
}

// decodeColor decodes the color byte to a readable string
func decodeColor(color uint8) string {
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

// decodeConnectionState decodes the connection state byte to a readable string
func decodeConnectionState(state uint8) string {
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

// parseProximityData parses Apple Continuity proximity pairing advertisement
func parseProximityData(data []byte) (*ProximityData, error) {
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
	pd.LeftBattery = decodeBattery(leftNibble)
	pd.RightBattery = decodeBattery(rightNibble)

	// Case battery from byte 5 - use simple decoding like AirPods batteries
	if len(payload) > 5 {
		caseBatteryRaw := payload[5]
		pd.CaseBattery = decodeBattery(caseBatteryRaw & 0x0F)
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

// Close closes the scanner
func (s *Scanner) Close() error {
	s.StopDiscovery()
	return s.conn.Close()
}

func (pd *ProximityData) String() string {
	result := fmt.Sprintf("AirPods Battery (BLE - Approximate):\n")

	// Left AirPod
	result += fmt.Sprintf("  Left:  ")
	if pd.LeftBattery != nil {
		result += fmt.Sprintf("%d%% ", *pd.LeftBattery)
		if pd.LeftCharging {
			result += "(Charging) "
		}
		if pd.LeftInEar {
			result += "[In Ear]"
		}
	} else {
		result += "Unknown"
	}

	// Right AirPod
	result += fmt.Sprintf("\n  Right: ")
	if pd.RightBattery != nil {
		result += fmt.Sprintf("%d%% ", *pd.RightBattery)
		if pd.RightCharging {
			result += "(Charging) "
		}
		if pd.RightInEar {
			result += "[In Ear]"
		}
	} else {
		result += "Unknown"
	}

	// Case
	result += fmt.Sprintf("\n  Case:  ")
	if pd.CaseBattery != nil {
		result += fmt.Sprintf("%d%% ", *pd.CaseBattery)
		if pd.CaseCharging {
			result += "(Charging)"
		}
	} else {
		result += "Unknown"
	}

	// Lid status
	result += fmt.Sprintf("\n  Lid:   ")
	if pd.LidOpen {
		result += "Open"
	} else {
		result += "Closed"
	}

	result += fmt.Sprintf("\n  Model: 0x%04X", pd.DeviceModel)

	// Color
	result += fmt.Sprintf("\n  Color: %s", decodeColor(pd.Color))

	// Connection state
	result += fmt.Sprintf("\n  Connection: %s", decodeConnectionState(pd.ConnectionState))

	// Orientation
	result += fmt.Sprintf("\n  Orientation: ")
	if pd.IsFlipped {
		result += "Flipped (Right pod is primary)"
	} else {
		result += "Normal (Left pod is primary)"
	}

	// Raw data
	result += fmt.Sprintf("\n\n  Raw Data: ")
	for i, b := range pd.RawData {
		if i > 0 {
			result += " "
		}
		result += fmt.Sprintf("%02x", b)
	}

	result += fmt.Sprintf("\n\n  Note: BLE data may be 5-10%% off actual values")
	return result
}
