package ui

import (
	"fmt"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	"github.com/diamondburned/gotk4/pkg/glib/v2"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"

	"linuxpods/internal/battery"
	"linuxpods/internal/ble"
)

// BatteryWidgets holds references to UI elements for updating battery display
type BatteryWidgets struct {
	LeftLevel   *gtk.LevelBar
	RightLevel  *gtk.LevelBar
	CaseLevel   *gtk.LevelBar
	LeftLabel   *gtk.Label
	RightLabel  *gtk.Label
	CaseLabel   *gtk.Label
	StatusLabel *gtk.Label // For connection status, charging, etc.
}

func Activate(app *adw.Application, batteryMgr *battery.Manager) *adw.ApplicationWindow {
	win := adw.NewApplicationWindow(&app.Application)
	win.SetTitle("LinuxPods")
	win.SetDefaultSize(400, 500)

	batteryWidgets := setupUI(win)
	win.Present()

	// Register callback with battery manager to update UI
	batteryMgr.RegisterCallback(func(data *ble.ProximityData) {
		// Update UI on GTK main thread
		glib.IdleAdd(func() {
			updateBatteryDisplay(batteryWidgets, data)
		})
	})

	return win
}

func setupUI(win *adw.ApplicationWindow) *BatteryWidgets {
	// Create header bar with close button
	headerBar := adw.NewHeaderBar()

	// Create a view stack for tabs
	viewStack := adw.NewViewStack()

	// Create a view switcher for tab navigation
	viewSwitcher := adw.NewViewSwitcher()
	viewSwitcher.SetStack(viewStack)
	viewSwitcher.SetPolicy(adw.ViewSwitcherPolicyWide)
	headerBar.SetTitleWidget(viewSwitcher)

	// Create the Control tab content
	controlBox, batteryWidgets := createControlView()
	viewStack.AddTitledWithIcon(controlBox, "control", "Control", "audio-headphones-symbolic")

	// Create the Settings tab content (placeholder for now)
	settingsBox := createSettingsView()
	viewStack.AddTitledWithIcon(settingsBox, "settings", "Settings", "preferences-system-symbolic")

	// Use ToolbarView for seamless GNOME design (no visual separation)
	toolbarView := adw.NewToolbarView()
	toolbarView.AddTopBar(headerBar)
	toolbarView.SetContent(viewStack)

	// Set the toolbar view as the window's content
	win.SetContent(toolbarView)

	return batteryWidgets
}

func createControlView() (*gtk.Box, *BatteryWidgets) {
	// Create main vertical box to hold all control elements
	controlBox := gtk.NewBox(gtk.OrientationVertical, 20)
	controlBox.SetMarginTop(20)
	controlBox.SetMarginBottom(20)
	controlBox.SetMarginStart(20)
	controlBox.SetMarginEnd(20)

	// Create battery widgets structure
	widgets := &BatteryWidgets{}

	// Create horizontal box for battery indicators
	batteryBox := gtk.NewBox(gtk.OrientationHorizontal, 20)
	batteryBox.SetHAlign(gtk.AlignCenter)
	batteryBox.SetVAlign(gtk.AlignStart)

	// Define image paths for AirPods components
	imagePaths := []string{
		"assets/left_airpod.png",
		"assets/right_airpod.png",
		"assets/airpod_case.png",
	}

	// Create references for each battery component
	levelBars := []*gtk.LevelBar{}
	labels := []*gtk.Label{}

	// Create three battery indicators with images
	for i := 0; i < 3; i++ {
		// Create vertical box for each column (image + battery indicator)
		columnBox := gtk.NewBox(gtk.OrientationVertical, 10)
		columnBox.SetHAlign(gtk.AlignCenter)

		// Add AirPod image
		image := gtk.NewImageFromFile(imagePaths[i])
		image.SetPixelSize(64)
		columnBox.Append(image)

		// Add battery indicator (LevelBar)
		batteryLevel := gtk.NewLevelBar()
		batteryLevel.SetMode(gtk.LevelBarModeContinuous)
		batteryLevel.SetValue(0.0) // Start at 0, will be updated by scanner
		batteryLevel.SetSizeRequest(100, 20)
		columnBox.Append(batteryLevel)
		levelBars = append(levelBars, batteryLevel)

		// Add battery percentage label
		percentLabel := gtk.NewLabel("--")
		percentLabel.AddCSSClass("dim-label")
		columnBox.Append(percentLabel)
		labels = append(labels, percentLabel)

		// Add column to battery box
		batteryBox.Append(columnBox)
	}

	// Store widget references
	widgets.LeftLevel = levelBars[0]
	widgets.RightLevel = levelBars[1]
	widgets.CaseLevel = levelBars[2]
	widgets.LeftLabel = labels[0]
	widgets.RightLabel = labels[1]
	widgets.CaseLabel = labels[2]

	// Add battery indicators to control box
	controlBox.Append(batteryBox)

	// Add status label for connection state, charging, etc.
	statusLabel := gtk.NewLabel("Searching for AirPods...")
	statusLabel.AddCSSClass("dim-label")
	statusLabel.SetMarginTop(10)
	controlBox.Append(statusLabel)
	widgets.StatusLabel = statusLabel

	// Create Noise Control section using Adwaita PreferencesGroup
	noiseControlGroup := adw.NewPreferencesGroup()
	noiseControlGroup.SetTitle("Noise Control")

	// Define noise control options
	options := []struct {
		id    string
		title string
		desc  string
	}{
		{"transparency", "Transparency", "Hear the world around you"},
		{"adaptive", "Adaptive", "Automatically adjusts to your environment"},
		{"noise_cancelling", "Noise Cancelling", "Block out background noise"},
		{"off", "Off", "Noise control disabled"},
	}

	var firstButton *gtk.CheckButton
	for i, opt := range options {
		// Create action row
		row := adw.NewActionRow()
		row.SetTitle(opt.title)
		row.SetSubtitle(opt.desc)

		// Create radio button
		var radioButton *gtk.CheckButton
		if i == 0 {
			radioButton = gtk.NewCheckButton()
			radioButton.SetActive(true) // Set first option as default
			firstButton = radioButton
		} else {
			radioButton = gtk.NewCheckButton()
			radioButton.SetGroup(firstButton)
		}

		// Connect signal handler
		radioButton.Connect("toggled", func() {
			if radioButton.Active() {
				println("Noise Control changed to:", opt.title, "("+opt.id+")")
				// Add your logic here to actually change the noise control setting
			}
		})

		row.AddPrefix(radioButton)
		row.SetActivatableWidget(radioButton)

		noiseControlGroup.Add(row)
	}

	// Add noise control section to control box
	controlBox.Append(noiseControlGroup)

	// Create Conversation Awareness section
	conversationGroup := adw.NewPreferencesGroup()
	conversationGroup.SetTitle("Features")

	conversationRow := adw.NewActionRow()
	conversationRow.SetTitle("Conversation Awareness")
	conversationRow.SetSubtitle("Lower media volume when you start speaking")

	conversationSwitch := gtk.NewSwitch()
	conversationSwitch.SetActive(false)
	conversationSwitch.SetVAlign(gtk.AlignCenter)
	conversationRow.AddSuffix(conversationSwitch)
	conversationRow.SetActivatableWidget(conversationSwitch)

	conversationSwitch.Connect("notify::active", func() {
		if conversationSwitch.Active() {
			println("Conversation Awareness enabled")
		} else {
			println("Conversation Awareness disabled")
		}
	})

	conversationGroup.Add(conversationRow)

	// Add conversation awareness section to control box
	controlBox.Append(conversationGroup)

	return controlBox, widgets
}

func createSettingsView() *gtk.Box {
	// Create main vertical box for settings
	settingsBox := gtk.NewBox(gtk.OrientationVertical, 20)
	settingsBox.SetMarginTop(20)
	settingsBox.SetMarginBottom(20)
	settingsBox.SetMarginStart(20)
	settingsBox.SetMarginEnd(20)

	// Create a preferences group for settings
	settingsGroup := adw.NewPreferencesGroup()
	settingsGroup.SetTitle("General")
	settingsGroup.SetDescription("Application preferences")

	// Add a sample setting row
	autoConnectRow := adw.NewActionRow()
	autoConnectRow.SetTitle("Auto-connect")
	autoConnectRow.SetSubtitle("Automatically connect when AirPods are detected")

	autoConnectSwitch := gtk.NewSwitch()
	autoConnectSwitch.SetActive(true)
	autoConnectSwitch.SetVAlign(gtk.AlignCenter)
	autoConnectRow.AddSuffix(autoConnectSwitch)
	autoConnectRow.SetActivatableWidget(autoConnectSwitch)

	settingsGroup.Add(autoConnectRow)

	// Add another setting
	notificationsRow := adw.NewActionRow()
	notificationsRow.SetTitle("Battery notifications")
	notificationsRow.SetSubtitle("Show notification when battery is low")

	notificationsSwitch := gtk.NewSwitch()
	notificationsSwitch.SetActive(false)
	notificationsSwitch.SetVAlign(gtk.AlignCenter)
	notificationsRow.AddSuffix(notificationsSwitch)
	notificationsRow.SetActivatableWidget(notificationsSwitch)

	settingsGroup.Add(notificationsRow)

	settingsBox.Append(settingsGroup)

	// Add About section
	aboutGroup := adw.NewPreferencesGroup()
	aboutGroup.SetTitle("About")

	aboutRow := adw.NewActionRow()
	aboutRow.SetTitle("LinuxPods")
	aboutRow.SetSubtitle("Version 0.1.0")

	aboutGroup.Add(aboutRow)

	settingsBox.Append(aboutGroup)

	return settingsBox
}

// updateBatteryDisplay updates the UI with battery data from BLE scanner
func updateBatteryDisplay(widgets *BatteryWidgets, data *ble.ProximityData) {
	// Update left AirPod
	if data.LeftBattery != nil {
		widgets.LeftLevel.SetValue(float64(*data.LeftBattery) / 100.0)
		charging := ""
		if data.LeftCharging {
			charging = " ⚡"
		}
		inEar := ""
		if data.LeftInEar {
			inEar = " 👂"
		}
		widgets.LeftLabel.SetText(fmt.Sprintf("%d%%%s%s", *data.LeftBattery, charging, inEar))
	} else {
		widgets.LeftLevel.SetValue(0.0)
		widgets.LeftLabel.SetText("--")
	}

	// Update right AirPod
	if data.RightBattery != nil {
		widgets.RightLevel.SetValue(float64(*data.RightBattery) / 100.0)
		charging := ""
		if data.RightCharging {
			charging = " ⚡"
		}
		inEar := ""
		if data.RightInEar {
			inEar = " 👂"
		}
		widgets.RightLabel.SetText(fmt.Sprintf("%d%%%s%s", *data.RightBattery, charging, inEar))
	} else {
		widgets.RightLevel.SetValue(0.0)
		widgets.RightLabel.SetText("--")
	}

	// Update case
	if data.CaseBattery != nil {
		widgets.CaseLevel.SetValue(float64(*data.CaseBattery) / 100.0)
		charging := ""
		if data.CaseCharging {
			charging = " ⚡"
		}
		widgets.CaseLabel.SetText(fmt.Sprintf("%d%%%s", *data.CaseBattery, charging))
	} else {
		widgets.CaseLevel.SetValue(0.0)
		widgets.CaseLabel.SetText("--")
	}

	// Update status label with connection state and other info
	statusText := fmt.Sprintf("Model: 0x%04X", data.DeviceModel)
	if data.LidOpen {
		statusText += " • Lid: Open"
	} else {
		statusText += " • Lid: Closed"
	}
	widgets.StatusLabel.SetText(statusText)
}
