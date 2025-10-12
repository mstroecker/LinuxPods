// Package battery provides centralized battery state management.
//
// The Manager handles:
//   - BLE scanning for AirPods battery data
//   - Notifying UI and other components of battery updates via callbacks
package battery

import (
	"fmt"
	"sync"
	"time"

	"linuxpods/internal/ble"
)

// UpdateCallback is called when battery data is updated
type UpdateCallback func(*ble.ProximityData)

// Manager manages battery state and coordinates updates
type Manager struct {
	scanner *ble.Scanner

	mu        sync.RWMutex
	callbacks []UpdateCallback
	lastData  *ble.ProximityData

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
			// Scan for AirPods with 5 second timeout
			data, err := m.scanner.ScanForAirPods(5 * time.Second)
			if err == nil {
				m.handleBatteryUpdate(data)
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

// Close stops the battery manager and cleans up resources
func (m *Manager) Close() error {
	close(m.stopChan)

	if m.scanner != nil {
		if err := m.scanner.Close(); err != nil {
			return fmt.Errorf("scanner close: %w", err)
		}
	}

	return nil
}
