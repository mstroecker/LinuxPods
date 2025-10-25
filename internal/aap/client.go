// Package aap implements the Apple Accessory Protocol (AAP) for communicating with AirPods.
//
// The AAP protocol allows us to access AirPods-specific features that are not available
// through standard Bluetooth protocols:
//   - Per-earbud battery levels (left, right, case)
//   - Noise control modes (Transparency, ANC, Off)
//   - Ear detection status
//   - Conversation awareness
//   - Head gestures
//
// Communication happens over L2CAP (Logical Link Control and Adaptation Protocol)
// on PSM (Protocol/Service Multiplexer) 4097 (0x1001).
//
// Protocol Flow:
//  1. Open L2CAP connection to AirPods (PSM 4097)
//  2. Send a handshake packet
//  3. Request notifications for battery/status
//  4. Parse incoming packets
//
// Based on reverse engineering work from:
//   - LibrePods: https://github.com/kavishdevar/librepods
//   - OpenPods: https://github.com/adolfintel/OpenPods
package aap

import (
	"encoding/hex"
	"fmt"
	"syscall"
	"unsafe"
)

const (
	// AAPPSM L2CAP Protocol/Service Multiplexer for AAP
	AAPPSM = 0x1001 // 4097 in decimal

	// AF_BLUETOOTH Bluetooth address family
	AF_BLUETOOTH = 31

	// SOCK_SEQPACKET Socket type for L2CAP
	SOCK_SEQPACKET = 5

	// BTPROTO_L2CAP Bluetooth protocol for L2CAP
	BTPROTO_L2CAP = 0

	// BDADDR_LEN L2CAP socket address structure size
	BDADDR_LEN = 6
)

// AAP protocol packet constants
var (
	// packetHandshake is the initial handshake packet sent after connection
	packetHandshake = [16]byte{0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}

	// packetBatteryRequest requests battery status notification
	packetBatteryRequest = [10]byte{0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF}

	// packetEnableFeatures enables special features
	packetEnableFeatures = [14]byte{0x04, 0x00, 0x04, 0x00, 0x4d, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}

	// packetKeyRequest requests proximity pairing encryption keys
	packetKeyRequest = [8]byte{0x04, 0x00, 0x04, 0x00, 0x30, 0x00, 0x05, 0x00}
)

// Client represents an AAP client connected to AirPods
type Client struct {
	fd     int    // L2CAP socket file descriptor
	addr   string // Bluetooth MAC address of AirPods
	isOpen bool
}

// bdaddr_t represents a Bluetooth device address
type bdaddr_t [6]byte

// sockaddr_l2 represents the L2CAP socket address structure
type sockaddr_l2 struct {
	family      uint16
	psm         uint16
	bdaddr      bdaddr_t
	cid         uint16
	bdaddr_type uint8
}

// NewClient creates a new AAP client for the given Bluetooth MAC address
func NewClient(macAddr string) (*Client, error) {
	return &Client{
		addr: macAddr,
	}, nil
}

// Connect opens an L2CAP connection to the AirPods
func (c *Client) Connect() error {
	if c.isOpen {
		return fmt.Errorf("already connected")
	}

	// Create L2CAP socket
	fd, err := syscall.Socket(AF_BLUETOOTH, SOCK_SEQPACKET, BTPROTO_L2CAP)
	if err != nil {
		return fmt.Errorf("failed to create L2CAP socket: %w", err)
	}
	c.fd = fd

	bdAddr, err := parseMACAddress(c.addr)
	if err != nil {
		_ = syscall.Close(fd)
		return fmt.Errorf("invalid MAC address: %w", err)
	}

	// Prepare L2CAP socket address
	addr := sockaddr_l2{
		family:      AF_BLUETOOTH,
		psm:         AAPPSM,
		bdaddr:      bdAddr,
		cid:         0,
		bdaddr_type: 0, // BDADDR_BREDR (public address)
	}

	// Connect to AirPods
	_, _, errno := syscall.Syscall(syscall.SYS_CONNECT, uintptr(fd),
		uintptr(unsafe.Pointer(&addr)), unsafe.Sizeof(addr))
	if errno != 0 {
		_ = syscall.Close(fd)
		return fmt.Errorf("failed to connect to AirPods: %v", errno)
	}

	c.isOpen = true
	return nil
}

// Handshake sends the initial handshake packet to enable AAP communication
func (c *Client) Handshake() error {
	return c.sendPacket(packetHandshake[:], "handshake")
}

// RequestBatteryStatus requests battery status notifications
func (c *Client) RequestBatteryStatus() error {
	return c.sendPacket(packetBatteryRequest[:], "battery request")
}

// EnableSpecialFeatures enables conversational awareness and adaptive transparency
func (c *Client) EnableSpecialFeatures() error {
	return c.sendPacket(packetEnableFeatures[:], "feature enable")
}

// RequestProximityKeys sends the proximity key request packet.
// This packet requests the encryption keys (IRK and ENC_KEY) used to decrypt
// BLE proximity pairing advertisements.
//
// After calling this, use ReadProximityKeys() to wait for and parse the response.
func (c *Client) RequestProximityKeys() error {
	return c.sendPacket(packetKeyRequest[:], "key request")
}

// sendPacket sends a packet to the AirPods and verifies it was fully written.
// This is a common helper method used by all request methods.
func (c *Client) sendPacket(packet []byte, packetType string) error {
	if !c.isOpen {
		return fmt.Errorf("not connected")
	}

	n, err := syscall.Write(c.fd, packet)
	if err != nil {
		return fmt.Errorf("failed to send %s: %w", packetType, err)
	}
	if n != len(packet) {
		return fmt.Errorf("incomplete %s write: %d/%d bytes", packetType, n, len(packet))
	}

	return nil
}

// ReadPacket reads a single AAP packet from the AirPods
func (c *Client) ReadPacket() ([]byte, error) {
	if !c.isOpen {
		return nil, fmt.Errorf("not connected")
	}

	buf := make([]byte, 1024)
	n, err := syscall.Read(c.fd, buf)
	if err != nil {
		return nil, fmt.Errorf("failed to read packet: %w", err)
	}

	return buf[:n], nil
}

// Close closes the L2CAP connection
func (c *Client) Close() error {
	if !c.isOpen {
		return nil
	}

	err := syscall.Close(c.fd)
	c.isOpen = false
	return err
}

// parseMACAddress converts a MAC address string to bdaddr_t
// Format: "XX:XX:XX:XX:XX:XX"
func parseMACAddress(addr string) (bdaddr_t, error) {
	var bdaddr bdaddr_t

	// Remove colons and parse as hex
	cleaned := ""
	for _, c := range addr {
		if c != ':' {
			cleaned += string(c)
		}
	}

	if len(cleaned) != 12 {
		return bdaddr, fmt.Errorf("invalid MAC address length")
	}

	bytes, err := hex.DecodeString(cleaned)
	if err != nil {
		return bdaddr, fmt.Errorf("invalid hex in MAC address: %w", err)
	}

	// Bluetooth addresses are stored in reverse order
	for i := 0; i < 6; i++ {
		bdaddr[i] = bytes[5-i]
	}

	return bdaddr, nil
}
