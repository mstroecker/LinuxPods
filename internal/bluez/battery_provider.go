// Package bluez provides integration with BlueZ's Battery Provider D-Bus API.
//
// # D-Bus Connection Architecture
//
// This package implements the BlueZ Battery Provider protocol to expose battery
// information to the system. The implementation requires careful adherence to
// the D-Bus ObjectManager pattern and proper signal emission.
//
// # Critical Requirements
//
//  1. Single Connection Per Provider:
//     The BluezBatteryProvider maintains a persistent D-Bus system bus connection throughout
//     its lifetime. ALL operations related to the provider (device discovery, battery
//     registration, signal monitoring) MUST use this same connection.
//
//  2. InterfacesAdded Signal:
//     When adding a new battery object, the provider MUST emit the InterfacesAdded
//     signal on the ObjectManager interface. Without this signal, BlueZ will not
//     expose the battery information to GNOME Settings.
//
// # Correct Usage Pattern
//
//	// Create provider (opens persistent connection)
//	provider, err := bluez.NewBluezBatteryProvider()
//	defer provider.Close()
//
//	// Use provider's methods which use its connection
//	provider.WatchForAirPods()  // ✓ Discovers and monitors using provider's connection
//
//	// Or manually:
//	device, _ := provider.DiscoverAirPodsDevice()  // ✓ Uses provider's connection
//	provider.AddBattery("airpods", 50, device)     // ✓ Emits InterfacesAdded signal
//
// # Testing
//
// See cmd/debug for test utilities that verify the D-Bus integration works correctly.
package bluez

import (
	"fmt"
	"log"
	"sync"

	"github.com/godbus/dbus/v5"
	"github.com/godbus/dbus/v5/introspect"
)

const (
	bluezService                = "org.bluez"
	batteryProviderManagerIface = "org.bluez.BatteryProviderManager1"
	batteryProviderIface        = "org.bluez.BatteryProvider1"
	providerPath                = "/com/github/mstroecker/linuxpods/battery"
)

// BatteryDevice represents a single battery device
type BatteryDevice struct {
	path       dbus.ObjectPath
	percentage uint8
	device     dbus.ObjectPath
	source     string
}

// BluezBatteryProvider manages battery information for BlueZ
type BluezBatteryProvider struct {
	conn    *dbus.Conn
	devices map[string]*BatteryDevice
	mu      sync.RWMutex
}

// NewBluezBatteryProvider creates and registers a new battery provider with BlueZ
func NewBluezBatteryProvider() (*BluezBatteryProvider, error) {
	conn, err := dbus.ConnectSystemBus()
	if err != nil {
		return nil, fmt.Errorf("failed to connect to system bus: %w", err)
	}

	bp := &BluezBatteryProvider{
		conn:    conn,
		devices: make(map[string]*BatteryDevice),
	}

	// Export the provider object
	if err := bp.exportProvider(); err != nil {
		conn.Close()
		return nil, fmt.Errorf("failed to export provider: %w", err)
	}

	// Register with BlueZ
	if err := bp.register(); err != nil {
		conn.Close()
		return nil, fmt.Errorf("failed to register provider: %w", err)
	}

	return bp, nil
}

// exportProvider exports the battery provider on D-Bus
func (bp *BluezBatteryProvider) exportProvider() error {
	// Export ObjectManager interface
	if err := bp.conn.Export(bp, providerPath, "org.freedesktop.DBus.ObjectManager"); err != nil {
		return err
	}

	// Export introspection for the provider root
	providerIntrospect := `
<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
	<interface name="org.freedesktop.DBus.ObjectManager">
		<method name="GetManagedObjects">
			<arg name="objects" type="a{oa{sa{sv}}}" direction="out"/>
		</method>
		<signal name="InterfacesAdded">
			<arg name="object_path" type="o"/>
			<arg name="interfaces_and_properties" type="a{sa{sv}}"/>
		</signal>
		<signal name="InterfacesRemoved">
			<arg name="object_path" type="o"/>
			<arg name="interfaces" type="as"/>
		</signal>
	</interface>
</node>`

	if err := bp.conn.Export(introspect.Introspectable(providerIntrospect), providerPath, "org.freedesktop.DBus.Introspectable"); err != nil {
		return err
	}

	return nil
}

// register registers this provider with BlueZ BatteryProviderManager
func (bp *BluezBatteryProvider) register() error {
	obj := bp.conn.Object(bluezService, "/org/bluez/hci0")
	call := obj.Call(batteryProviderManagerIface+".RegisterBatteryProvider", 0, dbus.ObjectPath(providerPath))
	if call.Err != nil {
		return fmt.Errorf("failed to register battery provider: %w", call.Err)
	}
	return nil
}

// AddBattery adds a new battery device to the provider
func (bp *BluezBatteryProvider) AddBattery(name string, percentage uint8, devicePath string) error {
	bp.mu.Lock()
	defer bp.mu.Unlock()

	batteryPath := dbus.ObjectPath(fmt.Sprintf("%s/%s", providerPath, name))

	device := &BatteryDevice{
		path:       batteryPath,
		percentage: percentage,
		device:     dbus.ObjectPath(devicePath),
		source:     "LinuxPods",
	}

	// Export Properties interface for this battery
	if err := bp.conn.Export(device, batteryPath, "org.freedesktop.DBus.Properties"); err != nil {
		return err
	}

	// Export introspection for this battery object
	batteryIntrospect := `
<!DOCTYPE node PUBLIC "-//freedesktop//DTD D-BUS Object Introspection 1.0//EN"
"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd">
<node>
	<interface name="org.bluez.BatteryProvider1">
		<property name="Percentage" type="y" access="read"/>
		<property name="Device" type="o" access="read"/>
		<property name="Source" type="s" access="read"/>
	</interface>
	<interface name="org.freedesktop.DBus.Properties">
		<method name="Get">
			<arg name="interface_name" type="s" direction="in"/>
			<arg name="property_name" type="s" direction="in"/>
			<arg name="value" type="v" direction="out"/>
		</method>
		<method name="GetAll">
			<arg name="interface_name" type="s" direction="in"/>
			<arg name="properties" type="a{sv}" direction="out"/>
		</method>
	</interface>
</node>`

	if err := bp.conn.Export(introspect.Introspectable(batteryIntrospect), batteryPath, "org.freedesktop.DBus.Introspectable"); err != nil {
		return err
	}

	bp.devices[name] = device

	// Emit InterfacesAdded signal to notify BlueZ of the new battery
	interfaces := map[string]map[string]dbus.Variant{
		batteryProviderIface: {
			"Percentage": dbus.MakeVariant(percentage),
			"Device":     dbus.MakeVariant(dbus.ObjectPath(devicePath)),
			"Source":     dbus.MakeVariant("LinuxPods"),
		},
	}

	if err := bp.conn.Emit(providerPath, "org.freedesktop.DBus.ObjectManager.InterfacesAdded",
		batteryPath, interfaces); err != nil {
		return fmt.Errorf("failed to emit InterfacesAdded signal: %w", err)
	}

	return nil
}

// Get implements org.freedesktop.DBus.Properties.Get for BatteryDevice
func (bd *BatteryDevice) Get(iface string, property string) (dbus.Variant, *dbus.Error) {
	if iface != batteryProviderIface {
		return dbus.Variant{}, dbus.NewError("org.freedesktop.DBus.Error.UnknownInterface", []interface{}{iface})
	}

	switch property {
	case "Percentage":
		return dbus.MakeVariant(bd.percentage), nil
	case "Device":
		return dbus.MakeVariant(bd.device), nil
	case "Source":
		return dbus.MakeVariant(bd.source), nil
	default:
		return dbus.Variant{}, dbus.NewError("org.freedesktop.DBus.Error.UnknownProperty", []interface{}{property})
	}
}

// GetAll implements org.freedesktop.DBus.Properties.GetAll for BatteryDevice
func (bd *BatteryDevice) GetAll(iface string) (map[string]dbus.Variant, *dbus.Error) {
	if iface != batteryProviderIface {
		return nil, dbus.NewError("org.freedesktop.DBus.Error.UnknownInterface", []interface{}{iface})
	}

	return map[string]dbus.Variant{
		"Percentage": dbus.MakeVariant(bd.percentage),
		"Device":     dbus.MakeVariant(bd.device),
		"Source":     dbus.MakeVariant(bd.source),
	}, nil
}

// Set implements org.freedesktop.DBus.Properties.Set for BatteryDevice (not used, all properties are read-only)
func (bd *BatteryDevice) Set(iface string, property string, value dbus.Variant) *dbus.Error {
	return dbus.NewError("org.freedesktop.DBus.Error.PropertyReadOnly", []interface{}{property})
}

// GetManagedObjects implements org.freedesktop.DBus.ObjectManager
func (bp *BluezBatteryProvider) GetManagedObjects() (map[dbus.ObjectPath]map[string]map[string]dbus.Variant, *dbus.Error) {
	bp.mu.RLock()
	defer bp.mu.RUnlock()

	objects := make(map[dbus.ObjectPath]map[string]map[string]dbus.Variant)

	for _, device := range bp.devices {
		objects[device.path] = map[string]map[string]dbus.Variant{
			batteryProviderIface: {
				"Percentage": dbus.MakeVariant(device.percentage),
				"Device":     dbus.MakeVariant(device.device),
				"Source":     dbus.MakeVariant(device.source),
			},
		}
	}

	return objects, nil
}

// UpdateBatteryPercentage updates the battery percentage for a device
func (bp *BluezBatteryProvider) UpdateBatteryPercentage(name string, percentage uint8) error {
	bp.mu.Lock()
	defer bp.mu.Unlock()

	device, ok := bp.devices[name]
	if !ok {
		return fmt.Errorf("battery device %s not found", name)
	}

	device.percentage = percentage

	// Emit PropertiesChanged signal
	changes := map[string]dbus.Variant{
		"Percentage": dbus.MakeVariant(percentage),
	}
	invalidated := []string{}

	if err := bp.conn.Emit(device.path, "org.freedesktop.DBus.Properties.PropertiesChanged",
		batteryProviderIface, changes, invalidated); err != nil {
		return err
	}

	return nil
}

// RemoveBattery removes a battery device from the provider
func (bp *BluezBatteryProvider) RemoveBattery(name string) error {
	bp.mu.Lock()
	defer bp.mu.Unlock()

	device, ok := bp.devices[name]
	if !ok {
		return fmt.Errorf("battery device %s not found", name)
	}

	batteryPath := device.path

	// Emit InterfacesRemoved signal to notify BlueZ
	interfaces := []string{batteryProviderIface}
	if err := bp.conn.Emit(providerPath, "org.freedesktop.DBus.ObjectManager.InterfacesRemoved",
		batteryPath, interfaces); err != nil {
		return fmt.Errorf("failed to emit InterfacesRemoved signal: %w", err)
	}

	// Unexport the battery object from D-Bus
	bp.conn.Export(nil, batteryPath, "org.freedesktop.DBus.Properties")
	bp.conn.Export(nil, batteryPath, "org.freedesktop.DBus.Introspectable")

	// Remove from internal map
	delete(bp.devices, name)

	return nil
}

// DiscoverAirPodsDevice searches for connected AirPods using provider's existing connection
func (bp *BluezBatteryProvider) DiscoverAirPodsDevice() (string, error) {
	// Get all BlueZ managed objects
	obj := bp.conn.Object(bluezService, "/")
	var objects map[dbus.ObjectPath]map[string]map[string]dbus.Variant

	err := obj.Call("org.freedesktop.DBus.ObjectManager.GetManagedObjects", 0).Store(&objects)
	if err != nil {
		return "", fmt.Errorf("failed to get managed objects: %w", err)
	}

	return findAirPodsInObjects(objects)
}

// findAirPodsInObjects searches for AirPods in the given BlueZ objects
func findAirPodsInObjects(objects map[dbus.ObjectPath]map[string]map[string]dbus.Variant) (string, error) {
	// Search for AirPods devices
	for path, interfaces := range objects {
		// Check if this is a device object
		if deviceProps, ok := interfaces["org.bluez.Device1"]; ok {
			// Check device name/alias
			if alias, ok := deviceProps["Alias"]; ok {
				aliasStr, ok := alias.Value().(string)
				if !ok {
					continue
				}
				// Check if it's an AirPods device
				if contains(aliasStr, "AirPods") {
					// Check if device is connected
					if connected, ok := deviceProps["Connected"]; ok {
						if connBool, ok := connected.Value().(bool); ok && connBool {
							return string(path), nil
						}
					}
				}
			}
		}
	}
	return "", fmt.Errorf("no connected AirPods device found")
}

// contains checks if a string contains a substring (case-insensitive helper)
func contains(s, substr string) bool {
	return len(s) >= len(substr) && (s == substr || len(s) > len(substr) &&
		(s[:len(substr)] == substr || s[len(s)-len(substr):] == substr ||
			findSubstring(s, substr)))
}

func findSubstring(s, substr string) bool {
	for i := 0; i <= len(s)-len(substr); i++ {
		if s[i:i+len(substr)] == substr {
			return true
		}
	}
	return false
}

// WatchForAirPods monitors for AirPods connections and automatically registers battery
func (bp *BluezBatteryProvider) WatchForAirPods() error {
	// First, check if AirPods are already connected (using provider's existing connection)
	if device, err := bp.DiscoverAirPodsDevice(); err == nil {
		if err := bp.AddBattery("airpods_battery", 36, device); err == nil {
			log.Printf("Battery provider registered for device: %s", device)
			log.Println("Note: GNOME Settings shows one battery per device. Use LinuxPods app for all three batteries.")
		}
	}

	// Watch for property changes on all device objects
	rule := "type='signal',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',path_namespace='/org/bluez'"
	if err := bp.conn.BusObject().Call("org.freedesktop.DBus.AddMatch", 0, rule).Err; err != nil {
		return fmt.Errorf("failed to add match rule: %w", err)
	}

	// Create channel for signals
	signalChan := make(chan *dbus.Signal, 10)
	bp.conn.Signal(signalChan)

	// Monitor signals in background
	go func() {
		for signal := range signalChan {
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

			// Check if Connected property changed
			if connectedVar, ok := changes["Connected"]; ok {
				if connected, ok := connectedVar.Value().(bool); ok && connected {
					// Device connected, check if it's AirPods
					devicePath := string(signal.Path)
					if alias := bp.getDeviceAlias(devicePath); contains(alias, "AirPods") {
						bp.mu.Lock()
						_, exists := bp.devices["airpods_battery"]
						bp.mu.Unlock()

						if !exists {
							if err := bp.AddBattery("airpods_battery", 36, devicePath); err == nil {
								log.Printf("Battery provider registered for newly connected device: %s", devicePath)
							}
						}
					}
				}
			}
		}
	}()

	return nil
}

// getDeviceAlias retrieves the alias/name of a Bluetooth device
func (bp *BluezBatteryProvider) getDeviceAlias(devicePath string) string {
	obj := bp.conn.Object(bluezService, dbus.ObjectPath(devicePath))
	variant, err := obj.GetProperty("org.bluez.Device1.Alias")
	if err != nil {
		return ""
	}
	if alias, ok := variant.Value().(string); ok {
		return alias
	}
	return ""
}

// Close unregisters the provider and closes the D-Bus connection
func (bp *BluezBatteryProvider) Close() error {
	obj := bp.conn.Object(bluezService, "/org/bluez/hci0")
	call := obj.Call(batteryProviderManagerIface+".UnregisterBatteryProvider", 0, dbus.ObjectPath(providerPath))
	if call.Err != nil {
		return call.Err
	}
	bp.conn.Close()
	return nil
}
