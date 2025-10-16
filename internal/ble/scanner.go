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
	"log"
	"time"

	"github.com/godbus/dbus/v5"
)

const (
	bluezService   = "org.bluez"
	adapterPath    = "/org/bluez/hci0"
	appleCompanyID = 0x004C
)

// Scanner handles BLE advertisement scanning
type Scanner struct {
	conn   *dbus.Conn
	signal chan *dbus.Signal
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

		case signal, ok := <-s.signal:

			// Debugging message for an unexpected closed dbus channel
			if !ok {
				log.Println("Error: This should not happen. DBUS channel closed.")
				continue
			}

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
					if data, err := ParseProximityData(appleData); err == nil {
						return data, nil
					}
				}
			}
		}
	}
}

// Close closes the scanner
func (s *Scanner) Close() error {
	s.StopDiscovery()
	return s.conn.Close()
}
