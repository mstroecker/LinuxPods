package main

import (
	"log"
	"os"

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
	// Initialize battery provider for BlueZ integration
	provider, err := bluez.NewBatteryProvider()
	if err != nil {
		log.Printf("Warning: Failed to create battery provider: %v", err)
		log.Printf("Battery information will not appear in GNOME Settings")
	} else {
		// Watch for AirPods connections (handles both existing and new connections)
		if err := provider.WatchForAirPods(); err != nil {
			log.Printf("Warning: Failed to start AirPods monitoring: %v", err)
		}
		defer provider.Close()
	}

	// Create a GTK application
	app = adw.NewApplication(appID, 0)

	// Initialize system tray indicator
	tray := indicator.New(
		showWindow,
		quitApp,
		func(mode indicator.NoiseMode) {
			log.Printf("Noise mode changed from tray: %s", mode)
		},
	)
	tray.Start()

	app.ConnectActivate(func() {
		window = ui.Activate(app)
	})

	code := app.Run(os.Args)
	tray.Stop()
	os.Exit(code)
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
