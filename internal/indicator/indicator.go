package indicator

import (
	"fmt"
	"log"
	"os"

	"fyne.io/systray"
)

// BatteryLevels holds the battery percentages for each component
type BatteryLevels struct {
	Left  uint8
	Right uint8
	Case  uint8
}

// NoiseMode represents the current noise control mode
type NoiseMode string

const (
	Transparency     NoiseMode = "transparency"
	Adaptive         NoiseMode = "adaptive"
	NoiseCancelling  NoiseMode = "noise_cancelling"
	Off              NoiseMode = "off"
)

// Indicator manages the system tray icon and menu
type Indicator struct {
	batteries        BatteryLevels
	noiseMode        NoiseMode
	onShowWindow     func()
	onQuit           func()
	onNoiseModeChange func(NoiseMode)

	// Menu items
	batteryItems     [3]*systray.MenuItem
	noiseModeItems   map[NoiseMode]*systray.MenuItem
}

// New creates and initializes a new system tray indicator
func New(onShowWindow, onQuit func(), onNoiseModeChange func(NoiseMode)) *Indicator {
	return &Indicator{
		batteries: BatteryLevels{
			Left:  36,
			Right: 63,
			Case:  69,
		},
		noiseMode:         Transparency,
		onShowWindow:      onShowWindow,
		onQuit:            onQuit,
		onNoiseModeChange: onNoiseModeChange,
		noiseModeItems:    make(map[NoiseMode]*systray.MenuItem),
	}
}

// Start initializes the system tray indicator
func (ind *Indicator) Start() {
	go systray.Run(ind.onReady, ind.onExit)
}

// Stop terminates the system tray indicator
func (ind *Indicator) Stop() {
	systray.Quit()
}

// onReady is called when systray is ready
func (ind *Indicator) onReady() {
	// Set icon from file
	iconData, err := loadIcon("assets/tray_icon.png")
	if err != nil {
		log.Printf("Warning: Failed to load tray icon: %v", err)
		// Continue without icon
	} else {
		systray.SetIcon(iconData)
	}

	systray.SetTitle("LinuxPods")
	systray.SetTooltip(fmt.Sprintf("AirPods Pro - %d%%", ind.batteries.Left))

	// Create battery level display items (non-clickable)
	systray.AddMenuItem("Battery Levels", "Current battery status").Disable()
	systray.AddSeparator()

	ind.batteryItems[0] = systray.AddMenuItem(fmt.Sprintf("  Left:  %d%%", ind.batteries.Left), "Left AirPod battery")
	ind.batteryItems[0].Disable()

	ind.batteryItems[1] = systray.AddMenuItem(fmt.Sprintf("  Right: %d%%", ind.batteries.Right), "Right AirPod battery")
	ind.batteryItems[1].Disable()

	ind.batteryItems[2] = systray.AddMenuItem(fmt.Sprintf("  Case:  %d%%", ind.batteries.Case), "Case battery")
	ind.batteryItems[2].Disable()

	systray.AddSeparator()

	// Noise Control section
	systray.AddMenuItem("Noise Control", "Noise control mode").Disable()

	ind.noiseModeItems[Transparency] = systray.AddMenuItemCheckbox("Transparency", "Hear the world around you", true)
	ind.noiseModeItems[Adaptive] = systray.AddMenuItemCheckbox("Adaptive", "Automatically adjusts", false)
	ind.noiseModeItems[NoiseCancelling] = systray.AddMenuItemCheckbox("Noise Cancelling", "Block background noise", false)
	ind.noiseModeItems[Off] = systray.AddMenuItemCheckbox("Off", "Noise control disabled", false)

	systray.AddSeparator()

	// Actions
	mOpen := systray.AddMenuItem("Open LinuxPods", "Show the main window")
	mQuit := systray.AddMenuItem("Quit", "Exit LinuxPods")

	// Handle menu clicks
	go func() {
		for {
			select {
			case <-ind.noiseModeItems[Transparency].ClickedCh:
				ind.setNoiseMode(Transparency)
			case <-ind.noiseModeItems[Adaptive].ClickedCh:
				ind.setNoiseMode(Adaptive)
			case <-ind.noiseModeItems[NoiseCancelling].ClickedCh:
				ind.setNoiseMode(NoiseCancelling)
			case <-ind.noiseModeItems[Off].ClickedCh:
				ind.setNoiseMode(Off)
			case <-mOpen.ClickedCh:
				if ind.onShowWindow != nil {
					ind.onShowWindow()
				}
			case <-mQuit.ClickedCh:
				if ind.onQuit != nil {
					ind.onQuit()
				}
				return
			}
		}
	}()
}

// onExit is called when systray is exiting
func (ind *Indicator) onExit() {
	log.Println("System tray indicator exited")
}

// setNoiseMode updates the noise control mode
func (ind *Indicator) setNoiseMode(mode NoiseMode) {
	// Uncheck all modes
	for _, item := range ind.noiseModeItems {
		item.Uncheck()
	}

	// Check the selected mode
	ind.noiseModeItems[mode].Check()
	ind.noiseMode = mode

	// Call the callback
	if ind.onNoiseModeChange != nil {
		ind.onNoiseModeChange(mode)
	}

	log.Printf("Noise mode changed to: %s", mode)
}

// UpdateBatteryLevels updates the displayed battery levels
func (ind *Indicator) UpdateBatteryLevels(left, right, caseLevel uint8) {
	ind.batteries.Left = left
	ind.batteries.Right = right
	ind.batteries.Case = caseLevel

	// Update tooltip with lowest battery
	lowest := left
	if right < lowest {
		lowest = right
	}
	if caseLevel < lowest {
		lowest = caseLevel
	}

	systray.SetTooltip(fmt.Sprintf("AirPods Pro - %d%%", lowest))

	// Update menu items
	if ind.batteryItems[0] != nil {
		ind.batteryItems[0].SetTitle(fmt.Sprintf("  Left:  %d%%", left))
	}
	if ind.batteryItems[1] != nil {
		ind.batteryItems[1].SetTitle(fmt.Sprintf("  Right: %d%%", right))
	}
	if ind.batteryItems[2] != nil {
		ind.batteryItems[2].SetTitle(fmt.Sprintf("  Case:  %d%%", caseLevel))
	}
}

// loadIcon loads icon data from a file
func loadIcon(path string) ([]byte, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read icon file: %w", err)
	}
	return data, nil
}
