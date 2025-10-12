package main

import (
	"log"
	"os"

	"linuxpods/internal/battery"
	"linuxpods/internal/ble"
	"linuxpods/internal/bluez"
	"linuxpods/internal/indicator"
	"linuxpods/internal/ui"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	"github.com/diamondburned/gotk4/pkg/glib/v2"
)

const appID = "com.linuxpods.app"

var (
	app    *adw.Application
	window *adw.ApplicationWindow
)

func main() {
	os.Exit(run())
}

func run() int {
	// Create centralized battery manager
	// This handles BLE scanning and notifies all components via callbacks
	batteryMgr, err := battery.NewManager()
	if err != nil {
		log.Fatalf("Failed to create battery manager: %v", err)
	}
	defer batteryMgr.Close()

	// Create components
	bluezProvider := createBluezBatteryProvider(batteryMgr)
	if bluezProvider != nil {
		defer bluezProvider.Close()
	}

	tray := createTrayIndicator(batteryMgr)
	defer tray.Stop()

	app = adw.NewApplication(appID, 0)
	app.ConnectActivate(func() {
		window = ui.Activate(app, batteryMgr)
	})

	return app.Run(os.Args)
}

// createBluezBatteryProvider creates and configures the BlueZ battery provider
func createBluezBatteryProvider(batteryMgr *battery.Manager) *bluez.BluezBatteryProvider {
	bluezProvider, err := bluez.NewBluezBatteryProvider()
	if err != nil {
		log.Printf("Warning: Failed to create BlueZ battery provider: %v", err)
		log.Println("Battery won't appear in GNOME Settings, but UI will still work")
		return nil
	}

	// Set connection callback to manage AAP connection
	bluezProvider.SetConnectionCallback(func(connected bool, devicePath string, macAddr string) {
		if connected {
			log.Printf("AirPods connected: %s (MAC: %s)", devicePath, macAddr)
			if err := batteryMgr.ConnectAAP(macAddr); err != nil {
				log.Printf("Warning: Failed to connect AAP: %v", err)
				log.Println("Falling back to BLE for battery monitoring (approximate)")
			}
		} else {
			log.Printf("AirPods disconnected: %s", devicePath)
			batteryMgr.DisconnectAAP()
		}
	})

	// Watch for AirPods connections
	if err := bluezProvider.WatchForAirPods(); err != nil {
		log.Printf("Warning: Failed to watch for AirPods: %v", err)
	}

	// Register callback to update BlueZ provider when battery data changes
	batteryMgr.RegisterCallback(func(data *ble.ProximityData) {
		// Use lowest battery for GNOME Settings (most useful for knowing when to charge)
		var batteryLevel uint8 = 100
		hasAnyBattery := false

		if data.LeftBattery != nil {
			hasAnyBattery = true
			if *data.LeftBattery < batteryLevel {
				batteryLevel = *data.LeftBattery
			}
		}
		if data.RightBattery != nil {
			hasAnyBattery = true
			if *data.RightBattery < batteryLevel {
				batteryLevel = *data.RightBattery
			}
		}

		if !hasAnyBattery {
			batteryLevel = 0
		}

		if err := bluezProvider.UpdateBatteryPercentage("airpods_battery", batteryLevel); err != nil {
			log.Printf("Failed to update BlueZ battery provider: %v", err)
		}
	})

	return bluezProvider
}

// createTrayIndicator creates and configures the system tray indicator
func createTrayIndicator(batteryMgr *battery.Manager) *indicator.Indicator {
	tray := indicator.New(
		showWindow,
		quitApp,
		func(mode indicator.NoiseMode) {
			log.Printf("Noise mode changed from tray: %s", mode)
		},
	)
	tray.Start()

	// Register callback to update tray when battery data changes
	batteryMgr.RegisterCallback(func(data *ble.ProximityData) {
		tray.UpdateBatteryLevels(
			data.LeftBattery,
			data.RightBattery,
			data.CaseBattery,
			data.LeftCharging,
			data.RightCharging,
			data.CaseCharging,
		)
	})

	return tray
}

// showWindow displays the main application window
func showWindow() {
	if window != nil {
		glib.IdleAdd(func() {
			window.Present()
		})
	}
}

// quitApp quits the entire application
func quitApp() {
	if app != nil {
		glib.IdleAdd(func() {
			app.Quit()
		})
	}
}
