package main

import (
	"fmt"
	"log"
	"os"
	"time"

	"linuxpods/internal/bluez"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Println("Usage: debug <command>")
		fmt.Println("\nCommands:")
		fmt.Println("  full           - Full integration test")
		os.Exit(1)
	}

	switch os.Args[1] {
	case "full":
		testFullIntegration()
	default:
		log.Fatalf("Unknown command: %s", os.Args[1])
	}
}

func testFullIntegration() {
	log.Println("=== Testing Full Integration (GUI Scenario) ===")

	log.Println("\n1. Creating battery provider...")
	provider, err := bluez.NewBatteryProvider()
	if err != nil {
		log.Fatalf("Failed to create provider: %v", err)
	}
	log.Println("   Provider created successfully")

	log.Println("\n2. Discovering device using provider's connection...")
	device, err := provider.DiscoverAirPodsDevice()
	if err != nil {
		log.Printf("   No AirPods found: %v", err)
		return
	}
	log.Printf("   Found: %s", device)

	log.Println("\n3. Adding battery with discovered device (36%)...")
	if err := provider.AddBattery("airpods_battery", 36, device); err != nil {
		log.Printf("   ERROR: Failed to add battery! %v", err)
		return
	}
	log.Println("   SUCCESS: Battery added at 36%")

	log.Println("\n4. CHECK GNOME SETTINGS NOW - Battery should show 36%")
	log.Println("   Waiting 3 seconds before updating...")
	time.Sleep(3 * time.Second)

	log.Println("\n5. Updating battery to 69%...")
	if err := provider.UpdateBatteryPercentage("airpods_battery", 69); err != nil {
		log.Printf("   ERROR: Failed to update battery! %v", err)
		return
	}
	log.Println("   SUCCESS: Battery updated to 69%")
	log.Println("   CHECK GNOME SETTINGS - Battery should now show 69%")

	log.Println("\n6. Waiting 3 seconds before removing...")
	time.Sleep(3 * time.Second)

	log.Println("\n7. Removing battery...")
	if err := provider.RemoveBattery("airpods_battery"); err != nil {
		log.Printf("   ERROR: Failed to remove battery! %v", err)
		return
	}
	log.Println("   SUCCESS: Battery removed")
	log.Println("   CHECK GNOME SETTINGS - Battery should disappear")

	log.Println("\n8. Waiting 3 seconds before re-adding...")
	time.Sleep(3 * time.Second)

	log.Println("\n9. Re-adding battery at 50%...")
	if err := provider.AddBattery("airpods_battery", 50, device); err != nil {
		log.Printf("   ERROR: Failed to re-add battery! %v", err)
		return
	}
	log.Println("   SUCCESS: Battery re-added at 50%")
	log.Println("   CHECK GNOME SETTINGS - Battery should reappear at 50%")

	log.Println("\n10. Keeping provider alive - Press Ctrl+C to exit")

	// Keep running
	select {}
}
