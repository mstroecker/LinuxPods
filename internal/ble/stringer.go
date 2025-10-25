package ble

import "fmt"

// String returns a human-readable representation of the ProximityData
func (pd *ProximityData) String() string {
	accuracy := "BLE - Approximate (~10%)"
	if pd.HasDecrypted {
		accuracy = "Decrypted - Accurate (1%)"
	}
	result := fmt.Sprintf("AirPods Battery (%s):\n", accuracy)

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
	result += fmt.Sprintf("\n  Color: %s", DecodeColor(pd.Color))

	// Connection state
	result += fmt.Sprintf("\n  Connection: %s", DecodeConnectionState(pd.ConnectionState))

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

	if pd.HasDecrypted {
		result += fmt.Sprintf("\n\n  Note: Battery levels from decrypted data (1%% accuracy)")
	} else {
		result += fmt.Sprintf("\n\n  Note: BLE data may be 5-10%% off actual values")
	}
	return result
}
