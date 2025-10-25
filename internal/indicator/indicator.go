package indicator

import (
	"fmt"
	"linuxpods/internal/util"
	"log"
	"os"

	"fyne.io/systray"
)

// BatteryLevels holds the battery percentages for each component
type BatteryLevels struct {
	Left          *int // nil if unknown
	Right         *int // nil if unknown
	Case          *int // nil if unknown
	LeftCharging  bool
	RightCharging bool
	CaseCharging  bool
}

// NoiseMode represents the current noise control mode
type NoiseMode string

const (
	Transparency    NoiseMode = "transparency"
	Adaptive        NoiseMode = "adaptive"
	NoiseCancelling NoiseMode = "noise_cancelling"
	Off             NoiseMode = "off"
)

// Indicator manages the system tray icon and menu
type Indicator struct {
	batteries         BatteryLevels
	noiseMode         NoiseMode
	onShowWindow      func()
	onQuit            func()
	onNoiseModeChange func(NoiseMode)

	// Menu items
	batteryItems   [3]*systray.MenuItem
	noiseModeItems map[NoiseMode]*systray.MenuItem
}

// New creates and initializes a new system tray indicator
func New(onShowWindow, onQuit func(), onNoiseModeChange func(NoiseMode)) *Indicator {
	return &Indicator{
		batteries:         BatteryLevels{},
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
	iconData, err := loadIcon("assets/tray_icon3.png")
	if err != nil {
		log.Printf("Warning: Failed to load tray icon: %v", err)
	} else {
		systray.SetIcon(iconData)
	}

	systray.SetTitle("LinuxPods")
	systray.SetTooltip("Searching for AirPods...")

	// Create battery level display items (non-clickable)
	systray.AddMenuItem("Battery Levels", "Current battery status").Disable()
	systray.AddSeparator()

	ind.batteryItems[0] = systray.AddMenuItem("  Left:  --", "Left AirPod battery")
	ind.batteryItems[0].Disable()

	ind.batteryItems[1] = systray.AddMenuItem("  Right: --", "Right AirPod battery")
	ind.batteryItems[1].Disable()

	ind.batteryItems[2] = systray.AddMenuItem("  Case:  --", "Case battery")
	ind.batteryItems[2].Disable()

	systray.AddSeparator()

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

// onExit is called when 'systray' is exiting
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
func (ind *Indicator) UpdateBatteryLevels(left, right, caseLevel *int, leftCharging, rightCharging, caseCharging bool) {
	ind.batteries.Left = left
	ind.batteries.Right = right
	ind.batteries.Case = caseLevel
	ind.batteries.LeftCharging = leftCharging
	ind.batteries.RightCharging = rightCharging
	ind.batteries.CaseCharging = caseCharging

	// Find the lowest battery for tooltip
	lowest := util.MinOr(left, right, -1)

	if lowest != -1 {
		systray.SetTooltip(fmt.Sprintf("AirPods Pro - %d%%", lowest))
	} else {
		systray.SetTooltip("Searching for AirPods...")
	}

	// Update menu items with charging indicators
	updateBatteryMenuItem(ind.batteryItems[0], "Left", left, leftCharging)
	updateBatteryMenuItem(ind.batteryItems[1], "Right", right, rightCharging)
	updateBatteryMenuItem(ind.batteryItems[2], "Case", caseLevel, caseCharging)
}

// updateBatteryMenuItem updates a single battery menu item with level and charging status
func updateBatteryMenuItem(item *systray.MenuItem, label string, level *int, charging bool) {
	if item == nil {
		return
	}

	if level != nil {
		chargingIndicator := ""
		if charging {
			chargingIndicator = " ⚡"
		}
		item.SetTitle(fmt.Sprintf("  %-5s: %d%%%s", label, *level, chargingIndicator))
	} else {
		item.SetTitle(fmt.Sprintf("  %-5s: --", label))
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
