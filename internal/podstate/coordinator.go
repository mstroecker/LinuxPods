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
// The map key is the device MAC address
type UpdateCallback func(map[string]*PodState)

// PodStateCoordinator manages complete AirPods state and coordinates updates
type PodStateCoordinator struct {
	scanner   *ble.Scanner
	aapClient *aap.Client

	mu             sync.RWMutex
	callbacks      []UpdateCallback
	deviceStates   map[string]*PodState // MAC address -> PodState
	aapConnected   bool
	aapMacAddr     string            // MAC address of currently connected AAP device
	encryptionKeys map[string][]byte // MAC address -> ENC_KEY for decrypting BLE advertisements

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
		scanner:        scanner,
		callbacks:      make([]UpdateCallback, 0),
		deviceStates:   make(map[string]*PodState),
		encryptionKeys: make(map[string][]byte),
		stopChan:       make(chan struct{}),
	}

	// Start the state update loop
	go m.bleUpdateLoop()

	return m, nil
}

// RegisterCallback registers a callback to be notified of state updates
func (m *PodStateCoordinator) RegisterCallback(cb UpdateCallback) {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.callbacks = append(m.callbacks, cb)

	// If we have cached states, immediately notify the new callback
	if len(m.deviceStates) > 0 {
		// Create a copy of the states map
		statesCopy := make(map[string]*PodState, len(m.deviceStates))
		for addr, state := range m.deviceStates {
			statesCopy[addr] = state
		}
		go cb(statesCopy)
	}
}

// GetDeviceStates returns a copy of all device states
func (m *PodStateCoordinator) GetDeviceStates() map[string]*PodState {
	m.mu.RLock()
	defer m.mu.RUnlock()

	statesCopy := make(map[string]*PodState, len(m.deviceStates))
	for addr, state := range m.deviceStates {
		statesCopy[addr] = state
	}
	return statesCopy
}

// GetConnectedDeviceMac returns the MAC address of the currently connected AAP device
// Returns empty string if no AAP connection is active
func (m *PodStateCoordinator) GetConnectedDeviceMac() string {
	m.mu.RLock()
	defer m.mu.RUnlock()
	if m.aapConnected {
		return m.aapMacAddr
	}
	return ""
}

// bleUpdateLoop continuously scans for AirPods and updates battery data
func (m *PodStateCoordinator) bleUpdateLoop() {
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
				data, randomMac, err := m.scanner.ScanForAirPods(5 * time.Second)
				if err == nil {
					// Try to decrypt with all available keys to find the real device
					// BLE advertisements use randomized MAC addresses for privacy, so we need to
					// try all keys to identify which device this advertisement is from
					realMac := m.tryDecryptAndIdentify(data, randomMac)
					state := m.bleToState(data, realMac, randomMac)
					m.handleStateUpdate(realMac, state)
				}
			}

			// Wait before next scan
			time.Sleep(3 * time.Second)
		}
	}
}

// handleStateUpdate processes new state data and notifies all listeners
// macAddr is the MAC address of the device this state is for
func (m *PodStateCoordinator) handleStateUpdate(macAddr string, state *PodState) {
	m.mu.Lock()
	m.deviceStates[macAddr] = state

	// Create a copy of states to send to callbacks
	statesCopy := make(map[string]*PodState, len(m.deviceStates))
	for addr, s := range m.deviceStates {
		statesCopy[addr] = s
	}

	callbacks := make([]UpdateCallback, len(m.callbacks))
	copy(callbacks, m.callbacks)
	m.mu.Unlock()

	// Notify all registered callbacks
	for _, cb := range callbacks {
		cb(statesCopy)
	}
}

// ConnectAAP connects to AirPods via AAP for accurate battery monitoring
func (m *PodStateCoordinator) ConnectAAP(macAddr string) error {
	m.mu.Lock()
	defer m.mu.Unlock()

	// Close existing AAP connection if any
	if m.aapClient != nil {
		_ = m.aapClient.Close()
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
		_ = client.Close()
		return fmt.Errorf("failed to send handshake: %w", err)
	}

	// Wait for handshake to process
	time.Sleep(500 * time.Millisecond)

	// Request battery status
	if err := client.RequestBatteryStatus(); err != nil {
		_ = client.Close()
		return fmt.Errorf("failed to request battery: %w", err)
	}

	// Enable special features
	if err := client.EnableSpecialFeatures(); err != nil {
		_ = client.Close()
		return fmt.Errorf("failed to enable features: %w", err)
	}

	m.aapClient = client
	m.aapConnected = true
	m.aapMacAddr = macAddr

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
		_ = m.aapClient.Close()
		m.aapClient = nil
		m.aapConnected = false
		m.aapMacAddr = ""
		log.Println("AAP disconnected - resuming BLE scanning for battery data")
	}
}

// aapReadLoop continuously reads AAP packets and updates battery data
func (m *PodStateCoordinator) aapReadLoop() {
	for {
		m.mu.RLock()
		client := m.aapClient
		connected := m.aapConnected
		macAddr := m.aapMacAddr
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
			if aap.IsBatteryPacket(packet) {
				batteryInfo, err := aap.ParseBatteryPacket(packet)
				if err != nil {
					log.Printf("AAP battery parse error: %v", err)
				}
				// Convert AAP battery info to PodState
				state := m.aapToState(batteryInfo, packet, macAddr)
				m.handleStateUpdate(macAddr, state)
			}

			// Try to parse the proximity keys
			if aap.IsKeyPacket(packet) {
				proximityKeys, err := aap.ParseProximityKeys(packet)
				if err == nil {
					// Extract and store the ENC_KEY
					encKey := aap.FindEncryptionKey(proximityKeys)
					if encKey != nil {
						m.mu.Lock()
						m.encryptionKeys[macAddr] = encKey

						// Update the existing state to include the encryption key
						if existingState, ok := m.deviceStates[macAddr]; ok {
							existingState.EncryptionKey = make([]byte, len(encKey))
							copy(existingState.EncryptionKey, encKey)
						}
						m.mu.Unlock()

						log.Printf("Stored encryption key for device %s (%d bytes)", macAddr, len(encKey))

						// Notify callbacks of the updated state
						m.mu.RLock()
						statesCopy := make(map[string]*PodState, len(m.deviceStates))
						for addr, s := range m.deviceStates {
							statesCopy[addr] = s
						}
						callbacks := make([]UpdateCallback, len(m.callbacks))
						copy(callbacks, m.callbacks)
						m.mu.RUnlock()

						for _, cb := range callbacks {
							cb(statesCopy)
						}
					}
				}
			}
		}
	}
}

// bleToState converts BLE ProximityData to PodState
func (m *PodStateCoordinator) bleToState(data *ble.ProximityData, realMac string, bleMac string) *PodState {
	state := &PodState{
		Source:        DataSourceBLE,
		LeftCharging:  data.LeftCharging,
		RightCharging: data.RightCharging,
		CaseCharging:  data.CaseCharging,
		LeftInEar:     data.LeftInEar,
		RightInEar:    data.RightInEar,
		LidOpen:       data.LidOpen,
		DeviceModel:   data.DeviceModel,
		ModelName:     ble.DecodeModelName(data.DeviceModel),
		Color:         data.Color,
		RealMac:       realMac,
		CurrentBLEMac: bleMac,
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

	// Look up the encryption key for this device using the real MAC address
	m.mu.RLock()
	if encKey, ok := m.encryptionKeys[realMac]; ok {
		// Make a copy of the key
		state.EncryptionKey = make([]byte, len(encKey))
		copy(state.EncryptionKey, encKey)
	}
	m.mu.RUnlock()

	return state
}

// getBatteryFromAAP is a helper function that converts AAP Battery data to PodState fields.
// It returns the battery level (or nil if unavailable) and charging status.
func getBatteryFromAAP(battery *aap.Battery) (*int, bool) {
	if battery != nil {
		level := int(battery.Level)
		return &level, battery.Status == aap.StatusCharging
	}
	return nil, false
}

// aapToState converts AAP battery info to PodState
func (m *PodStateCoordinator) aapToState(info *aap.BatteryInfo, rawPacket []byte, macAddr string) *PodState {
	state := &PodState{
		Source:  DataSourceAAP,
		RealMac: macAddr, // AAP uses the real (permanent) MAC address
		// CurrentBLEMac is empty for AAP connections (no BLE randomization)
		RawData: rawPacket,
	}

	// Convert battery information from AAP to PodState
	state.LeftBattery, state.LeftCharging = getBatteryFromAAP(info.Left)
	state.RightBattery, state.RightCharging = getBatteryFromAAP(info.Right)
	state.CaseBattery, state.CaseCharging = getBatteryFromAAP(info.Case)

	// AAP doesn't provide in-ear detection, lid state, device model, color, or primary pod
	// These fields remain at their zero values

	// Look up the encryption key for this device
	m.mu.RLock()
	if encKey, ok := m.encryptionKeys[macAddr]; ok {
		// Make a copy of the key
		state.EncryptionKey = make([]byte, len(encKey))
		copy(state.EncryptionKey, encKey)
	}
	m.mu.RUnlock()

	return state
}

// RequestEncryptionKeys requests encryption keys from connected AirPods via AAP.
// This requires an active AAP connection to work.
// Returns an error if no AAP connection is active or if the request fails.
func (m *PodStateCoordinator) RequestEncryptionKeys() error {
	m.mu.RLock()
	client := m.aapClient
	connected := m.aapConnected
	m.mu.RUnlock()

	if !connected || client == nil {
		return fmt.Errorf("no active AAP connection - connect to AirPods first")
	}

	// Request the keys - they will be automatically stored when received in aapReadLoop
	if err := client.RequestProximityKeys(); err != nil {
		return fmt.Errorf("failed to request encryption keys: %w", err)
	}

	log.Println("Encryption key request sent - keys will be stored when received")
	return nil
}

// HasEncryptionKeys checks if any encryption keys have been stored
func (m *PodStateCoordinator) HasEncryptionKeys() bool {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return len(m.encryptionKeys) > 0
}

// GetEncryptionKey retrieves the encryption key for a specific device
func (m *PodStateCoordinator) GetEncryptionKey(macAddr string) []byte {
	m.mu.RLock()
	defer m.mu.RUnlock()
	return m.encryptionKeys[macAddr]
}

// GetAllEncryptionKeys returns a copy of all stored encryption keys
func (m *PodStateCoordinator) GetAllEncryptionKeys() map[string][]byte {
	m.mu.RLock()
	defer m.mu.RUnlock()

	keys := make(map[string][]byte, len(m.encryptionKeys))
	for addr, key := range m.encryptionKeys {
		keyCopy := make([]byte, len(key))
		copy(keyCopy, key)
		keys[addr] = keyCopy
	}
	return keys
}

// tryDecryptAndIdentify attempts to decrypt BLE data with all stored keys to identify the real device.
// BLE advertisements use randomized MAC addresses for privacy. By trying all encryption keys,
// we can identify which device the advertisement is from based on which key successfully decrypts it.
//
// Returns the real MAC address (from the key that worked), or the random MAC address if no key worked.
func (m *PodStateCoordinator) tryDecryptAndIdentify(data *ble.ProximityData, randomMac string) string {
	// Extract encrypted portion (bytes 9-24 of the payload)
	if len(data.RawData) < 25 {
		// No encrypted data or payload too short
		return randomMac
	}

	encryptedPortion := data.RawData[9:25]

	// Try all stored encryption keys
	m.mu.RLock()
	keysCopy := make(map[string][]byte, len(m.encryptionKeys))
	for mac, key := range m.encryptionKeys {
		keyCopy := make([]byte, len(key))
		copy(keyCopy, key)
		keysCopy[mac] = keyCopy
	}
	m.mu.RUnlock()

	// Try each key
	for realMac, key := range keysCopy {
		decrypted, err := ble.DecryptProximityPayload(encryptedPortion, key)
		if err != nil {
			// Decryption failed (wrong key or validation failed)
			continue
		}

		// Decryption succeeded, and validation passed - use this key
		err = data.AddDecryptedData(decrypted)
		if err == nil {
			log.Printf("BLE: Identified device %s (random MAC: %s) via encryption key", realMac, randomMac)
			return realMac
		}
	}

	// No key worked - return the random MAC address and log it
	if len(keysCopy) > 0 {
		log.Printf("BLE: Could not decrypt advertisement from %s with any stored key", randomMac)
	}
	return randomMac
}

// Close stops the pod state manager and cleans up resources
func (m *PodStateCoordinator) Close() error {
	close(m.stopChan)

	// Close AAP client first
	if m.aapClient != nil {
		_ = m.aapClient.Close()
	}

	if m.scanner != nil {
		if err := m.scanner.Close(); err != nil {
			return fmt.Errorf("scanner close: %w", err)
		}
	}

	return nil
}
