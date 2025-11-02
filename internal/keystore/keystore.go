// Package keystore provides filesystem storage for AirPods encryption keys.
//
// Keys are stored in ~/.local/share/linuxpods/keys.json following the
// XDG Base Directory specification.
//
// Storage format:
//   - File format: JSON
//   - Data structure: {"version": 1, "keys": {"MAC": "base64-key", ...}}
//
// Usage:
//
//	store, err := keystore.New()
//	if err != nil {
//		log.Fatal(err)
//	}
//
//	// Save a key
//	key := []byte{0x01, 0x02, ...} // 16-byte encryption key
//	err = store.Set("AA:BB:CC:DD:EE:FF", key)
//	err = store.Save()
//
//	// Load all keys
//	keys, err := store.Load()
//
//	// Get a specific key
//	key, exists := store.Get("AA:BB:CC:DD:EE:FF")
package keystore

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

const (
	// XDG_DATA_HOME default path
	xdgDataHome = ".local/share"
	appName     = "linuxpods"
	keysFile    = "keys.json"
)

// Keystore manages storage of AirPods encryption keys
type Keystore struct {
	dataDir string            // XDG data directory path
	keys    map[string][]byte // In-memory cache: MAC address -> encryption key
}

// keyData represents the JSON structure of the stored keys
type keyData struct {
	Version int               `json:"version"`
	Keys    map[string]string `json:"keys"` // MAC address -> base64-encoded key
}

// New creates a new Keystore instance
// It creates the data directory if it doesn't exist
func New() (*Keystore, error) {
	// Determine XDG data directory
	dataDir, err := getDataDir()
	if err != nil {
		return nil, fmt.Errorf("failed to determine data directory: %w", err)
	}

	// Create the directory if it doesn't exist
	if err := os.MkdirAll(dataDir, 0700); err != nil {
		return nil, fmt.Errorf("failed to create data directory: %w", err)
	}

	ks := &Keystore{
		dataDir: dataDir,
		keys:    make(map[string][]byte),
	}

	return ks, nil
}

// Load reads all keys from disk
// Returns a map of MAC address -> encryption key
// If the file doesn't exist, returns an empty map (not an error)
func (ks *Keystore) Load() (map[string][]byte, error) {
	filePath := filepath.Join(ks.dataDir, keysFile)

	if _, err := os.Stat(filePath); os.IsNotExist(err) {
		// File doesn't exist - return an empty map
		return make(map[string][]byte), nil
	}

	data, err := os.ReadFile(filePath)
	if err != nil {
		return nil, fmt.Errorf("failed to read keys file: %w", err)
	}

	var keyData keyData
	if err := json.Unmarshal(data, &keyData); err != nil {
		return nil, fmt.Errorf("failed to parse keys JSON: %w", err)
	}

	// Decode base64 keys
	keys := make(map[string][]byte)
	for mac, keyB64 := range keyData.Keys {
		key, err := base64.StdEncoding.DecodeString(keyB64)
		if err != nil {
			return nil, fmt.Errorf("failed to decode key for %s: %w", mac, err)
		}
		keys[mac] = key
	}

	ks.keys = keys
	return keys, nil
}

// Save writes all keys to disk
func (ks *Keystore) Save() error {
	keysB64 := make(map[string]string)
	for mac, key := range ks.keys {
		keysB64[mac] = base64.StdEncoding.EncodeToString(key)
	}

	data := keyData{
		Version: 1,
		Keys:    keysB64,
	}

	jsonData, err := json.MarshalIndent(data, "", "  ")
	if err != nil {
		return fmt.Errorf("failed to marshal keys to JSON: %w", err)
	}

	// Write (owner read/write-only)
	filePath := filepath.Join(ks.dataDir, keysFile)
	if err := os.WriteFile(filePath, jsonData, 0600); err != nil {
		return fmt.Errorf("failed to write keys file: %w", err)
	}

	return nil
}

// Get retrieves a key for a specific MAC address from the in-memory cache
// Returns the key and true if found, or nil and false if not found
// Call Load() first to populate the cache from disk
func (ks *Keystore) Get(macAddr string) ([]byte, bool) {
	key, exists := ks.keys[macAddr]
	if !exists {
		return nil, false
	}

	// Return a copy to prevent external modification
	keyCopy := make([]byte, len(key))
	copy(keyCopy, key)
	return keyCopy, true
}

// Set stores a key for a specific MAC address in the in-memory cache
// Call Save() to persist to disk
func (ks *Keystore) Set(macAddr string, key []byte) error {
	if len(key) == 0 {
		return fmt.Errorf("encryption key cannot be empty")
	}

	// Store a copy to prevent external modification
	keyCopy := make([]byte, len(key))
	copy(keyCopy, key)
	ks.keys[macAddr] = keyCopy

	return nil
}

// Delete removes a key for a specific MAC address from the in-memory cache
// Call Save() to persist to disk
func (ks *Keystore) Delete(macAddr string) {
	delete(ks.keys, macAddr)
}

// List returns all MAC addresses that have stored keys
func (ks *Keystore) List() []string {
	macs := make([]string, 0, len(ks.keys))
	for mac := range ks.keys {
		macs = append(macs, mac)
	}
	return macs
}

// Clear removes all keys from the in-memory cache
// Call Save() to persist to disk
func (ks *Keystore) Clear() {
	ks.keys = make(map[string][]byte)
}

// GetAll returns a copy of all stored keys
func (ks *Keystore) GetAll() map[string][]byte {
	keys := make(map[string][]byte, len(ks.keys))
	for mac, key := range ks.keys {
		keyCopy := make([]byte, len(key))
		copy(keyCopy, key)
		keys[mac] = keyCopy
	}
	return keys
}

// getDataDir returns the XDG data directory for the application
// Follows XDG Base Directory specification:
//   - Uses $XDG_DATA_HOME if set
//   - Falls back to ~/.local/share
func getDataDir() (string, error) {
	// Check XDG_DATA_HOME environment variable
	dataHome := os.Getenv("XDG_DATA_HOME")
	if dataHome == "" {
		// Use default: ~/.local/share
		home, err := os.UserHomeDir()
		if err != nil {
			return "", fmt.Errorf("failed to get home directory: %w", err)
		}
		dataHome = filepath.Join(home, xdgDataHome)
	}

	appDataDir := filepath.Join(dataHome, appName)
	return appDataDir, nil
}
