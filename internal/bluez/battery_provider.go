package bluez

import (
	"fmt"
	"sync"

	"github.com/godbus/dbus/v5"
	"github.com/godbus/dbus/v5/introspect"
)

const (
	bluezService                = "org.bluez"
	batteryProviderManagerIface = "org.bluez.BatteryProviderManager1"
	batteryProviderIface        = "org.bluez.BatteryProvider1"
	providerPath                = "/com/linuxpods/battery"
)

// BatteryDevice represents a single battery device
type BatteryDevice struct {
	path       dbus.ObjectPath
	percentage uint8
	device     dbus.ObjectPath
	source     string
}

// BatteryProvider manages battery information for BlueZ
type BatteryProvider struct {
	conn    *dbus.Conn
	devices map[string]*BatteryDevice
	mu      sync.RWMutex
}

// NewBatteryProvider creates and registers a new battery provider with BlueZ
func NewBatteryProvider() (*BatteryProvider, error) {
	conn, err := dbus.ConnectSystemBus()
	if err != nil {
		return nil, fmt.Errorf("failed to connect to system bus: %w", err)
	}

	bp := &BatteryProvider{
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
func (bp *BatteryProvider) exportProvider() error {
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
	</interface>
</node>`

	if err := bp.conn.Export(introspect.Introspectable(providerIntrospect), providerPath, "org.freedesktop.DBus.Introspectable"); err != nil {
		return err
	}

	return nil
}

// register registers this provider with BlueZ BatteryProviderManager
func (bp *BatteryProvider) register() error {
	obj := bp.conn.Object(bluezService, "/org/bluez/hci0")
	call := obj.Call(batteryProviderManagerIface+".RegisterBatteryProvider", 0, dbus.ObjectPath(providerPath))
	if call.Err != nil {
		return fmt.Errorf("failed to register battery provider: %w", call.Err)
	}
	return nil
}

// AddBattery adds a new battery device to the provider
func (bp *BatteryProvider) AddBattery(name string, percentage uint8, devicePath string) error {
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
func (bp *BatteryProvider) GetManagedObjects() (map[dbus.ObjectPath]map[string]map[string]dbus.Variant, *dbus.Error) {
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
func (bp *BatteryProvider) UpdateBatteryPercentage(name string, percentage uint8) error {
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

// Close unregisters the provider and closes the D-Bus connection
func (bp *BatteryProvider) Close() error {
	obj := bp.conn.Object(bluezService, "/org/bluez/hci0")
	call := obj.Call(batteryProviderManagerIface+".UnregisterBatteryProvider", 0, dbus.ObjectPath(providerPath))
	if call.Err != nil {
		return call.Err
	}
	bp.conn.Close()
	return nil
}
