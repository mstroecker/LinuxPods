// debug_bluez_dbus_discover is a debugging tool for discovering and inspecting AirPods devices via BlueZ D-Bus.
//
// This tool queries the BlueZ D-Bus API (org.freedesktop.DBus.ObjectManager) to discover
// all paired Bluetooth devices and displays detailed information about any AirPods devices found.
//
// Usage:
//
//	go run ./cmd/debug_bluez_dbus_discover
//
// The tool displays:
//   - Device name and D-Bus object path
//   - Connection status
//   - All device properties (MAC address, RSSI, etc.)
//   - Available D-Bus interfaces (Device1, Battery1, etc.)
//   - Battery information (if org.bluez.Battery1 interface is present)
//   - Bluetooth service UUIDs (Audio Sink, Apple Continuity, etc.)
//
// This is useful for:
//   - Understanding BlueZ D-Bus API structure
//   - Debugging device discovery issues
//   - Checking what properties and interfaces BlueZ exposes
//   - Finding device object paths needed for AAP connections
//   - Verifying battery provider integration
//
// Requirements:
//   - AirPods must be paired with this Linux device
//   - BlueZ Bluetooth stack must be running
package main

import (
	"fmt"
	"log"
	"strings"

	"github.com/godbus/dbus/v5"
)

func main() {
	log.Println("=== AirPods Discovery Tool ===")
	log.Println()

	conn, err := dbus.ConnectSystemBus()
	if err != nil {
		log.Fatalf("Failed to connect to system bus: %v", err)
	}
	defer conn.Close()

	// Get all BlueZ managed objects
	obj := conn.Object("org.bluez", "/")
	var objects map[dbus.ObjectPath]map[string]map[string]dbus.Variant

	if err := obj.Call("org.freedesktop.DBus.ObjectManager.GetManagedObjects", 0).Store(&objects); err != nil {
		log.Fatalf("Failed to get managed objects: %v", err)
	}

	found := false

	// Look for AirPods devices
	for path, interfaces := range objects {
		if deviceProps, ok := interfaces["org.bluez.Device1"]; ok {
			alias := getStringProp(deviceProps, "Alias")

			// Check if it's an AirPods device
			if containsAirPods(alias) {
				found = true
				connected := getBoolProp(deviceProps, "Connected")

				fmt.Printf("Found AirPods: %s\n", alias)
				fmt.Printf("  Path: %s\n", path)
				fmt.Printf("  Connected: %v\n", connected)
				fmt.Printf("\n--- All Device Properties ---\n")

				// Print all properties
				for key, variant := range deviceProps {
					fmt.Printf("  %s: %v (type: %s)\n", key, variant.Value(), variant.Signature().String())
				}

				fmt.Printf("\n--- All Interfaces ---\n")
				// Check what interfaces are available
				for iface := range interfaces {
					fmt.Printf("  - %s\n", iface)
				}

				// If it has Battery1 interface, show battery info
				if batteryProps, ok := interfaces["org.bluez.Battery1"]; ok {
					fmt.Printf("\n--- Battery Information ---\n")
					for key, variant := range batteryProps {
						fmt.Printf("  %s: %v\n", key, variant.Value())
					}
				}

				// Check available UUIDs (services)
				if uuids := getStringArrayProp(deviceProps, "UUIDs"); len(uuids) > 0 {
					fmt.Printf("\n--- Available Services (UUIDs) ---\n")
					for _, uuid := range uuids {
						fmt.Printf("  - %s: %s\n", uuid, getServiceName(uuid))
					}
				}

				fmt.Println("\n" + strings.Repeat("=", 60) + "\n")
			}
		}
	}

	if !found {
		fmt.Println("No AirPods devices found!")
		fmt.Println("Make sure your AirPods are:")
		fmt.Println("  1. Paired with this device")
		fmt.Println("  2. Connected via Bluetooth")
	}
}

func getStringProp(props map[string]dbus.Variant, key string) string {
	if v, ok := props[key]; ok {
		if s, ok := v.Value().(string); ok {
			return s
		}
	}
	return ""
}

func getBoolProp(props map[string]dbus.Variant, key string) bool {
	if v, ok := props[key]; ok {
		if b, ok := v.Value().(bool); ok {
			return b
		}
	}
	return false
}

func getStringArrayProp(props map[string]dbus.Variant, key string) []string {
	if v, ok := props[key]; ok {
		if arr, ok := v.Value().([]string); ok {
			return arr
		}
	}
	return nil
}

func containsAirPods(s string) bool {
	return strings.Contains(s, "AirPods")
}

func getServiceName(uuid string) string {
	// Common Bluetooth service UUIDs
	services := map[string]string{
		"0000110b-0000-1000-8000-00805f9b34fb": "Audio Sink",
		"0000110c-0000-1000-8000-00805f9b34fb": "A/V Remote Control Target",
		"0000110e-0000-1000-8000-00805f9b34fb": "A/V Remote Control",
		"0000111e-0000-1000-8000-00805f9b34fb": "Handsfree",
		"00001132-0000-1000-8000-00805f9b34fb": "Message Access Server",
		"74ec2172-0bad-4d01-8f77-997b2be0722a": "Apple Media Service",
		"89d3502b-0f36-433a-8ef4-c502ad55f8dc": "Apple Notification Center Service",
		"d0611e78-bbb4-4591-a5f8-487910ae4366": "Apple Continuity",
	}

	if name, ok := services[uuid]; ok {
		return name
	}
	return "Unknown Service"
}
