// Package podstate provides centralized AirPods state management.
//
// PodStateCoordinator handles:
//   - BLE scanning for AirPods data (battery, charging, in-ear detection)
//   - AAP client for accurate data (1% accuracy, requires connection)
//   - Notifying UI and other components of state updates via callbacks
//
// Data Source Priority:
//   - AAP (accurate, 1%) is used when AirPods are connected
//   - BLE (approximate, 5-10%) is used when not connected or as fallback
package podstate

import (
	"fmt"
	"log"
	"sync"
	"time"

	"linuxpods/internal/aap"
	"linuxpods/internal/ble"
)

// UpdateCallback is called when AirPods state data is updated
type UpdateCallback func(*PodState)

// PodStateCoordinator manages complete AirPods state and coordinates updates
type PodStateCoordinator struct {
	scanner   *ble.Scanner
	aapClient *aap.Client

	mu           sync.RWMutex
	callbacks    []UpdateCallback
	lastState    *PodState
	aapConnected bool

	stopChan chan struct{}
}

// NewPodStateCoordinator creates a new AirPods state manager
func NewPodStateCoordinator() (*PodStateCoordinator, error) {
	scanner, err := ble.NewScanner()
	if err != nil {
		return nil, fmt.Errorf("failed to create BLE scanner: %w", err)
	}

	// Start BLE discovery
	if err := scanner.StartDiscovery(); err != nil {
		scanner.Close()
		return nil, fmt.Errorf("failed to start BLE discovery: %w", err)
	}

	m := &PodStateCoordinator{
		scanner:   scanner,
		callbacks: make([]UpdateCallback, 0),
		stopChan:  make(chan struct{}),
	}

	// Start state update loop
	go m.updateLoop()

	return m, nil
}

// RegisterCallback registers a callback to be notified of state updates
func (m *PodStateCoordinator) RegisterCallback(cb UpdateCallback) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.callbacks = append(m.callbacks, cb)

	// If we have cached state, immediately notify the new callback
	if m.lastState != nil {
		go cb(m.lastState)
	}
}

// GetLastState returns the most recent state data, or nil if none available
func (m *PodStateCoordinator) GetLastState() *PodState {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.lastState
}

// updateLoop continuously scans for AirPods and updates battery data
func (m *PodStateCoordinator) updateLoop() {
	for {
		select {
		case <-m.stopChan:
			return
		default:
			// Only scan BLE if AAP is not connected (AAP is more accurate)
			m.mu.RLock()
			aapActive := m.aapConnected
			m.mu.RUnlock()

			if !aapActive {
				// Scan for AirPods with 5-second timeout
				data, err := m.scanner.ScanForAirPods(5 * time.Second)
				if err == nil {
					state := m.bleToState(data)
					m.handleStateUpdate(state)
				}
			}

			// Wait before next scan
			time.Sleep(3 * time.Second)
		}
	}
}

// handleStateUpdate processes new state data and notifies all listeners
func (m *PodStateCoordinator) handleStateUpdate(state *PodState) {
	m.mu.Lock()
	m.lastState = state
	callbacks := make([]UpdateCallback, len(m.callbacks))
	copy(callbacks, m.callbacks)
	m.mu.Unlock()

	// Notify all registered callbacks
	for _, cb := range callbacks {
		cb(state)
	}
}

// ConnectAAP connects to AirPods via AAP for accurate battery monitoring
func (m *PodStateCoordinator) ConnectAAP(macAddr string) error {
	m.mu.Lock()
	defer m.mu.Unlock()

	// Close existing AAP connection if any
	if m.aapClient != nil {
		m.aapClient.Close()
		m.aapClient = nil
		m.aapConnected = false
	}

	// Create new AAP client
	client, err := aap.NewClient(macAddr)
	if err != nil {
		return fmt.Errorf("failed to create AAP client: %w", err)
	}

	// Connect to AirPods
	if err := client.Connect(); err != nil {
		return fmt.Errorf("failed to connect AAP: %w", err)
	}

	// Send handshake
	if err := client.Handshake(); err != nil {
		client.Close()
		return fmt.Errorf("failed to send handshake: %w", err)
	}

	// Wait for handshake to process
	time.Sleep(500 * time.Millisecond)

	// Request battery status
	if err := client.RequestBatteryStatus(); err != nil {
		client.Close()
		return fmt.Errorf("failed to request battery: %w", err)
	}

	// Enable special features
	if err := client.EnableSpecialFeatures(); err != nil {
		client.Close()
		return fmt.Errorf("failed to enable features: %w", err)
	}

	m.aapClient = client
	m.aapConnected = true

	log.Printf("AAP connected successfully to %s - using accurate battery data (1%% precision)", macAddr)
	log.Println("BLE scanning paused while AAP is active")

	// Start AAP reading loop
	go m.aapReadLoop()

	return nil
}

// DisconnectAAP disconnects the AAP client
func (m *PodStateCoordinator) DisconnectAAP() {
	m.mu.Lock()
	defer m.mu.Unlock()

	if m.aapClient != nil {
		m.aapClient.Close()
		m.aapClient = nil
		m.aapConnected = false
		log.Println("AAP disconnected - resuming BLE scanning for battery data")
	}
}

// aapReadLoop continuously reads AAP packets and updates battery data
func (m *PodStateCoordinator) aapReadLoop() {
	for {
		m.mu.RLock()
		client := m.aapClient
		connected := m.aapConnected
		m.mu.RUnlock()

		if !connected || client == nil {
			return
		}

		select {
		case <-m.stopChan:
			return
		default:
			packet, err := client.ReadPacket()
			if err != nil {
				log.Printf("AAP read error: %v", err)
				m.DisconnectAAP()
				return
			}

			// Try to parse the battery packet
			batteryInfo, err := aap.ParseBatteryPacket(packet)
			if err == nil {
				// Convert AAP battery info to PodState
				state := m.aapToState(batteryInfo, packet)
				m.handleStateUpdate(state)
			}
		}
	}
}

// bleToState converts BLE ProximityData to PodState
func (m *PodStateCoordinator) bleToState(data *ble.ProximityData) *PodState {
	state := &PodState{
		Source:        DataSourceBLE,
		LeftCharging:  data.LeftCharging,
		RightCharging: data.RightCharging,
		CaseCharging:  data.CaseCharging,
		LeftInEar:     data.LeftInEar,
		RightInEar:    data.RightInEar,
		LidOpen:       data.LidOpen,
		DeviceModel:   data.DeviceModel,
		Color:         data.Color,
		RawData:       data.RawData,
	}

	// Convert battery levels from *uint8 to *int
	if data.LeftBattery != nil {
		level := int(*data.LeftBattery)
		state.LeftBattery = &level
	}
	if data.RightBattery != nil {
		level := int(*data.RightBattery)
		state.RightBattery = &level
	}
	if data.CaseBattery != nil {
		level := int(*data.CaseBattery)
		state.CaseBattery = &level
	}

	// Convert IsFlipped to PrimaryPod
	if data.IsFlipped {
		state.PrimaryPod = PodSideRight
	} else {
		state.PrimaryPod = PodSideLeft
	}

	return state
}

// aapToState converts AAP battery info to PodState
func (m *PodStateCoordinator) aapToState(info *aap.BatteryInfo, rawPacket []byte) *PodState {
	state := &PodState{
		Source:  DataSourceAAP,
		RawData: rawPacket,
	}

	// Convert Left battery
	if info.Left != nil {
		level := int(info.Left.Level)
		state.LeftBattery = &level
		state.LeftCharging = info.Left.Status == aap.StatusCharging
	}

	// Convert Right battery
	if info.Right != nil {
		level := int(info.Right.Level)
		state.RightBattery = &level
		state.RightCharging = info.Right.Status == aap.StatusCharging
	}

	// Convert Case battery
	if info.Case != nil {
		level := int(info.Case.Level)
		state.CaseBattery = &level
		state.CaseCharging = info.Case.Status == aap.StatusCharging
	}

	// AAP doesn't provide in-ear detection, lid state, device model, color, or primary pod
	// These fields remain at their zero values

	return state
}

// Close stops the pod state manager and cleans up resources
func (m *PodStateCoordinator) Close() error {
	close(m.stopChan)

	// Close AAP client first
	if m.aapClient != nil {
		m.aapClient.Close()
	}

	if m.scanner != nil {
		if err := m.scanner.Close(); err != nil {
			return fmt.Errorf("scanner close: %w", err)
		}
	}

	return nil
}
