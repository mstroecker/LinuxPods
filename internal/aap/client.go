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
//  2. Send handshake packet
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
	// L2CAP Protocol/Service Multiplexer for AAP
	AAPPSM = 0x1001 // 4097 in decimal

	// Bluetooth address family
	AF_BLUETOOTH = 31

	// Socket type for L2CAP
	SOCK_SEQPACKET = 5

	// Bluetooth protocol for L2CAP
	BTPROTO_L2CAP = 0

	// L2CAP socket address structure size
	BDADDR_LEN = 6
)

// Client represents an AAP client connected to AirPods
type Client struct {
	fd      int    // L2CAP socket file descriptor
	addr    string // Bluetooth MAC address of AirPods
	isOpen  bool
	readBuf []byte
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
		addr:    macAddr,
		readBuf: make([]byte, 1024),
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

	// Parse MAC address
	bdaddr, err := parseMACAddress(c.addr)
	if err != nil {
		syscall.Close(fd)
		return fmt.Errorf("invalid MAC address: %w", err)
	}

	// Prepare L2CAP socket address
	addr := sockaddr_l2{
		family:      AF_BLUETOOTH,
		psm:         AAPPSM,
		bdaddr:      bdaddr,
		cid:         0,
		bdaddr_type: 0, // BDADDR_BREDR (public address)
	}

	// Connect to AirPods
	_, _, errno := syscall.Syscall(syscall.SYS_CONNECT, uintptr(fd),
		uintptr(unsafe.Pointer(&addr)), unsafe.Sizeof(addr))
	if errno != 0 {
		syscall.Close(fd)
		return fmt.Errorf("failed to connect to AirPods: %v", errno)
	}

	c.isOpen = true
	return nil
}

// Handshake sends the initial handshake packet to enable AAP communication
func (c *Client) Handshake() error {
	if !c.isOpen {
		return fmt.Errorf("not connected")
	}

	// Handshake packet from librepods documentation
	handshake := []byte{0x00, 0x00, 0x04, 0x00, 0x01, 0x00, 0x02, 0x00,
		0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}

	n, err := syscall.Write(c.fd, handshake)
	if err != nil {
		return fmt.Errorf("failed to send handshake: %w", err)
	}
	if n != len(handshake) {
		return fmt.Errorf("incomplete handshake write: %d/%d bytes", n, len(handshake))
	}

	return nil
}

// RequestBatteryStatus requests battery status notifications
func (c *Client) RequestBatteryStatus() error {
	if !c.isOpen {
		return fmt.Errorf("not connected")
	}

	// Battery status notification request packet
	// From librepods AAP Definitions (using 0xFF variant that works with AirPods Pro)
	packet := []byte{0x04, 0x00, 0x04, 0x00, 0x0F, 0x00, 0xFF, 0xFF, 0xFF, 0xFF}

	n, err := syscall.Write(c.fd, packet)
	if err != nil {
		return fmt.Errorf("failed to send battery request: %w", err)
	}
	if n != len(packet) {
		return fmt.Errorf("incomplete battery request write: %d/%d bytes", n, len(packet))
	}

	return nil
}

// EnableSpecialFeatures enables conversational awareness and adaptive transparency
func (c *Client) EnableSpecialFeatures() error {
	if !c.isOpen {
		return fmt.Errorf("not connected")
	}

	// Special feature enabling packet
	packet := []byte{0x04, 0x00, 0x04, 0x00, 0x4d, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}

	n, err := syscall.Write(c.fd, packet)
	if err != nil {
		return fmt.Errorf("failed to send feature enable: %w", err)
	}
	if n != len(packet) {
		return fmt.Errorf("incomplete feature enable write: %d/%d bytes", n, len(packet))
	}

	return nil
}

// RequestProximityKeys sends the proximity key request packet.
// This packet requests the encryption keys (IRK and ENC_KEY) used to decrypt
// BLE proximity pairing advertisements.
//
// After calling this, use ReadProximityKeys() to wait for and parse the response.
func (c *Client) RequestProximityKeys() error {
	if !c.isOpen {
		return fmt.Errorf("not connected")
	}

	// Key request packet (from LibrePods proximity_keys.py)
	packet := []byte{0x04, 0x00, 0x04, 0x00, 0x30, 0x00, 0x05, 0x00}

	n, err := syscall.Write(c.fd, packet)
	if err != nil {
		return fmt.Errorf("failed to send key request: %w", err)
	}
	if n != len(packet) {
		return fmt.Errorf("incomplete key request write: %d/%d bytes", n, len(packet))
	}

	return nil
}

// ReadProximityKeys reads packets from the AirPods until a key response is received.
// The AirPods may send several non-key packets before the key packet arrives.
//
// This method will block until:
//   - A key packet is received and successfully parsed (returns keys, nil)
//   - maxAttempts packets have been read without finding keys (returns nil, error)
//   - A read error occurs (returns nil, error)
//
// Typical usage:
//
//	client.Handshake()
//	client.RequestProximityKeys()
//	keys, err := client.ReadProximityKeys(100)
func (c *Client) ReadProximityKeys(maxAttempts int) ([]ProximityKey, error) {
	if !c.isOpen {
		return nil, fmt.Errorf("not connected")
	}

	for attempt := 1; attempt <= maxAttempts; attempt++ {
		packet, err := c.ReadPacket()
		if err != nil {
			return nil, fmt.Errorf("failed to read packet (attempt %d/%d): %w", attempt, maxAttempts, err)
		}

		// Check if this packet contains keys
		if !IsKeyPacket(packet) {
			continue // Not a key packet, keep waiting
		}

		// Parse keys
		keys, err := ParseProximityKeys(packet)
		if err != nil {
			return nil, fmt.Errorf("failed to parse key packet: %w", err)
		}

		return keys, nil
	}

	return nil, fmt.Errorf("no key packet received after %d attempts", maxAttempts)
}

// RetrieveProximityKeys is a convenience method that combines RequestProximityKeys()
// and ReadProximityKeys() into a single call.
//
// This method:
//  1. Sends the key request packet
//  2. Waits for and parses the key response (up to maxAttempts packets)
//  3. Returns the parsed keys
//
// The client must be connected and handshake must be completed before calling this.
//
// Example:
//
//	client.Connect()
//	client.Handshake()
//	keys, err := client.RetrieveProximityKeys(100)
//	if err != nil {
//	    log.Fatal(err)
//	}
//	encKey := FindEncryptionKey(keys)
func (c *Client) RetrieveProximityKeys(maxAttempts int) ([]ProximityKey, error) {
	if err := c.RequestProximityKeys(); err != nil {
		return nil, err
	}

	return c.ReadProximityKeys(maxAttempts)
}

// ReadPacket reads a single AAP packet from the AirPods
func (c *Client) ReadPacket() ([]byte, error) {
	if !c.isOpen {
		return nil, fmt.Errorf("not connected")
	}

	n, err := syscall.Read(c.fd, c.readBuf)
	if err != nil {
		return nil, fmt.Errorf("failed to read packet: %w", err)
	}

	// Return a copy of the data
	packet := make([]byte, n)
	copy(packet, c.readBuf[:n])
	return packet, nil
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

// DumpPacket returns a hex dump of a packet for debugging
func DumpPacket(packet []byte) string {
	return hex.EncodeToString(packet)
}
