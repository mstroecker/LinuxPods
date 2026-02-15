package aap

import "fmt"

// NoiseMode represents an AirPods noise control mode
type NoiseMode byte

const (
	NoiseModeOff          NoiseMode = 0x01
	NoiseModeANC          NoiseMode = 0x02
	NoiseModeTransparency NoiseMode = 0x03
	NoiseModeAdaptive     NoiseMode = 0x04
)

func (m NoiseMode) String() string {
	switch m {
	case NoiseModeOff:
		return "Off"
	case NoiseModeANC:
		return "Noise Cancellation"
	case NoiseModeTransparency:
		return "Transparency"
	case NoiseModeAdaptive:
		return "Adaptive"
	default:
		return fmt.Sprintf("Unknown(0x%02X)", byte(m))
	}
}

// IsNoiseControlModePacket checks if a packet is a noise control mode report.
// The 0x09 command family has many sub-commands; only sub-command 0x0D is noise control.
func IsNoiseControlModePacket(packet []byte) bool {
	return len(packet) >= 8 && packet[4] == 0x09 && packet[6] == 0x0D
}

// ParseNoiseControlPacket extracts the noise control mode from a packet.
// Call IsNoiseControlModePacket first to verify the packet type.
func ParseNoiseControlPacket(packet []byte) (NoiseMode, error) {
	if !IsNoiseControlModePacket(packet) {
		return 0, fmt.Errorf("not a noise control packet")
	}
	return NoiseMode(packet[7]), nil
}
