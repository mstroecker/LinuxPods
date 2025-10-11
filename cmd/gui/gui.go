package main

import (
    "log"
    "os"

    "github.com/diamondburned/gotk4-adwaita/pkg/adw"
    "github.com/diamondburned/gotk4/pkg/glib/v2"
    "linuxpods/internal/bluez"
    "linuxpods/internal/indicator"
    "linuxpods/internal/ui"
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
        // Register one battery with BlueZ for GNOME Settings integration
        airpodsDevice := "/org/bluez/hci0/dev_90_62_3F_59_00_2F"

        if err := provider.AddBattery("airpods_battery", 36, airpodsDevice); err != nil {
            log.Printf("Failed to add AirPods battery: %v", err)
        }
        defer provider.Close()
        log.Println("Battery provider registered with BlueZ")
        log.Println("Note: GNOME Settings shows one battery per device. Use LinuxPods app for all three batteries.")
    }

    // Create GTK application
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
