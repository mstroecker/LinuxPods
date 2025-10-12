package ui

import (
	"fmt"

	"github.com/diamondburned/gotk4-adwaita/pkg/adw"
	"github.com/diamondburned/gotk4/pkg/gtk/v4"
)

func Activate(app *adw.Application) *adw.ApplicationWindow {
	win := adw.NewApplicationWindow(&app.Application)
	win.SetTitle("LinuxPods")
	win.SetDefaultSize(400, 500)

	setupUI(win)
	win.Present()

	return win
}

func setupUI(win *adw.ApplicationWindow) {
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
	controlBox := createControlView()
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
}

func createControlView() *gtk.Box {
	// Create main vertical box to hold all control elements
	controlBox := gtk.NewBox(gtk.OrientationVertical, 20)
	controlBox.SetMarginTop(20)
	controlBox.SetMarginBottom(20)
	controlBox.SetMarginStart(20)
	controlBox.SetMarginEnd(20)

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

	// Battery percentages for left, right, and case
	batteryPercentages := []float64{0.36, 0.63, 0.69} // 36%, 63%, 69%

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
		batteryLevel.SetValue(batteryPercentages[i])
		batteryLevel.SetSizeRequest(100, 20)
		columnBox.Append(batteryLevel)

		// Add battery percentage label
		percentLabel := gtk.NewLabel(fmt.Sprintf("%.0f%%", batteryPercentages[i]*100))
		percentLabel.AddCSSClass("dim-label")
		columnBox.Append(percentLabel)

		// Add column to battery box
		batteryBox.Append(columnBox)
	}

	// Add battery indicators to control box
	controlBox.Append(batteryBox)

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

	return controlBox
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
