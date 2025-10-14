package main

import (
	"linuxpods/internal/util"
	"log"
	"os"

	"linuxpods/internal/bluez"
	"linuxpods/internal/indicator"
	"linuxpods/internal/podstate"
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
	// Create centralized AirPods state coordinator
	// This coordinates BLE scanning, AAP connections, and notifies all components via callbacks
	podCoord, err := podstate.NewPodStateCoordinator()
	if err != nil {
		log.Fatalf("Failed to create pod state coordinator: %v", err)
	}
	defer podCoord.Close()

	// === Create Bluez Provider ===
	bluezProvider := createBluezBatteryProvider(podCoord)
	if bluezProvider != nil {
		defer bluezProvider.Close()
	}

	// === Create System Tray ===
	tray := createTrayIndicator(podCoord)
	defer tray.Stop()

	// === Create GUI App ===
	app = adw.NewApplication(appID, 0)
	app.ConnectActivate(func() {
		window = ui.Activate(app, podCoord)
	})

	return app.Run(os.Args)
}

// createBluezBatteryProvider creates and configures the BlueZ battery provider
func createBluezBatteryProvider(podCoord *podstate.PodStateCoordinator) *bluez.BluezBatteryProvider {
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
			if err := podCoord.ConnectAAP(macAddr); err != nil {
				log.Printf("Warning: Failed to connect AAP: %v", err)
				log.Println("Falling back to BLE for battery monitoring (approximate)")
			}
		} else {
			log.Printf("AirPods disconnected: %s", devicePath)
			podCoord.DisconnectAAP()
		}
	})

	// Watch for AirPods connections
	if err := bluezProvider.WatchForAirPods(); err != nil {
		log.Printf("Warning: Failed to watch for AirPods: %v", err)
	}

	// Register a callback to update BlueZ provider when state data changes
	podCoord.RegisterCallback(func(state *podstate.PodState) {
		// Use the lowest battery for GNOME Settings (most useful for knowing when to charge)
		var batteryLevel = util.MinOr(state.LeftBattery, state.RightBattery, 0)
		if err := bluezProvider.UpdateBatteryPercentage("airpods_battery", uint8(batteryLevel)); err != nil {
			log.Printf("Update BlueZ battery: %v", err)
		}
	})

	return bluezProvider
}

// createTrayIndicator creates and configures the system tray indicator
func createTrayIndicator(podCoord *podstate.PodStateCoordinator) *indicator.Indicator {
	tray := indicator.New(
		showWindow,
		quitApp,
		func(mode indicator.NoiseMode) {
			log.Printf("Noise mode changed from tray: %s", mode)
		},
	)
	tray.Start()

	// Register callback to update tray when state data changes
	podCoord.RegisterCallback(func(state *podstate.PodState) {
		tray.UpdateBatteryLevels(
			state.LeftBattery,
			state.RightBattery,
			state.CaseBattery,
			state.LeftCharging,
			state.RightCharging,
			state.CaseCharging,
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
