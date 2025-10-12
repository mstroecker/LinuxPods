// Package battery provides centralized battery state management.
//
// The Manager handles:
//   - BLE scanning for AirPods battery data (approximate, 5-10% accuracy)
//   - AAP client for accurate battery data (1% accuracy, requires connection)
//   - Notifying UI and other components of battery updates via callbacks
//
// Battery Data Priority:
//   - AAP (accurate) is used when AirPods are connected
//   - BLE (approximate) is used when not connected or as fallback
package battery

import (
	"fmt"
	"log"
	"sync"
	"time"

	"linuxpods/internal/aap"
	"linuxpods/internal/ble"
)

// UpdateCallback is called when battery data is updated
type UpdateCallback func(*ble.ProximityData)

// Manager manages battery state and coordinates updates
type Manager struct {
	scanner   *ble.Scanner
	aapClient *aap.Client

	mu           sync.RWMutex
	callbacks    []UpdateCallback
	lastData     *ble.ProximityData
	aapConnected bool

	stopChan chan struct{}
}

// NewManager creates a new battery manager
func NewManager() (*Manager, error) {
	scanner, err := ble.NewScanner()
	if err != nil {
		return nil, fmt.Errorf("failed to create BLE scanner: %w", err)
	}

	// Start BLE discovery
	if err := scanner.StartDiscovery(); err != nil {
		scanner.Close()
		return nil, fmt.Errorf("failed to start BLE discovery: %w", err)
	}

	m := &Manager{
		scanner:   scanner,
		callbacks: make([]UpdateCallback, 0),
		stopChan:  make(chan struct{}),
	}

	// Start battery update loop
	go m.updateLoop()

	return m, nil
}

// RegisterCallback registers a callback to be notified of battery updates
func (m *Manager) RegisterCallback(cb UpdateCallback) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.callbacks = append(m.callbacks, cb)

	// If we have cached data, immediately notify the new callback
	if m.lastData != nil {
		go cb(m.lastData)
	}
}

// GetLastData returns the most recent battery data, or nil if none available
func (m *Manager) GetLastData() *ble.ProximityData {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.lastData
}

// updateLoop continuously scans for AirPods and updates battery data
func (m *Manager) updateLoop() {
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
				// Scan for AirPods with 5 second timeout
				data, err := m.scanner.ScanForAirPods(5 * time.Second)
				if err == nil {
					m.handleBatteryUpdate(data)
				}
			}

			// Wait before next scan
			time.Sleep(3 * time.Second)
		}
	}
}

// handleBatteryUpdate processes new battery data and notifies all listeners
func (m *Manager) handleBatteryUpdate(data *ble.ProximityData) {
	m.mu.Lock()
	m.lastData = data
	callbacks := make([]UpdateCallback, len(m.callbacks))
	copy(callbacks, m.callbacks)
	m.mu.Unlock()

	// Notify all registered callbacks
	for _, cb := range callbacks {
		cb(data)
	}
}

// ConnectAAP connects to AirPods via AAP for accurate battery monitoring
func (m *Manager) ConnectAAP(macAddr string) error {
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
func (m *Manager) DisconnectAAP() {
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
func (m *Manager) aapReadLoop() {
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

			// Try to parse battery packet
			batteryInfo, err := aap.ParseBatteryPacket(packet)
			if err == nil {
				// Convert AAP battery info to ProximityData format
				data := m.aapToProximityData(batteryInfo)
				m.handleBatteryUpdate(data)
			}
		}
	}
}

// aapToProximityData converts AAP battery info to BLE ProximityData format
func (m *Manager) aapToProximityData(info *aap.BatteryInfo) *ble.ProximityData {
	data := &ble.ProximityData{}

	if info.Left != nil {
		data.LeftBattery = &info.Left.Level
		data.LeftCharging = info.Left.Status == aap.StatusCharging
	}

	if info.Right != nil {
		data.RightBattery = &info.Right.Level
		data.RightCharging = info.Right.Status == aap.StatusCharging
	}

	if info.Case != nil {
		data.CaseBattery = &info.Case.Level
		data.CaseCharging = info.Case.Status == aap.StatusCharging
	}

	return data
}

// Close stops the battery manager and cleans up resources
func (m *Manager) Close() error {
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
