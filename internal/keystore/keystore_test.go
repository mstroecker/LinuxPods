package keystore

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

// must is a test helper that fails the test if err is not nil
func must(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatal(err)
	}
}

func TestNew(t *testing.T) {
	// Create temporary directory for testing
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	// Check that data directory was created
	expectedDir := filepath.Join(tmpDir, appName)
	if ks.dataDir != expectedDir {
		t.Errorf("Expected dataDir %s, got %s", expectedDir, ks.dataDir)
	}

	// Check that directory exists
	if _, err := os.Stat(expectedDir); os.IsNotExist(err) {
		t.Errorf("Data directory was not created: %s", expectedDir)
	}
}

func TestSetAndGet(t *testing.T) {
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	// Test Set
	macAddr := "AA:BB:CC:DD:EE:FF"
	key := []byte{0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10}

	must(t, ks.Set(macAddr, key))

	// Test Get
	retrievedKey, exists := ks.Get(macAddr)
	if !exists {
		t.Errorf("Get() returned exists=false for existing key")
	}

	if !reflect.DeepEqual(retrievedKey, key) {
		t.Errorf("Retrieved key doesn't match. Expected %v, got %v", key, retrievedKey)
	}

	// Test Get for non-existent key
	_, exists = ks.Get("11:22:33:44:55:66")
	if exists {
		t.Errorf("Get() returned exists=true for non-existent key")
	}

	// Test Set with empty key
	err = ks.Set("11:22:33:44:55:66", []byte{})
	if err == nil {
		t.Errorf("Set() should fail for empty key")
	}
}

func TestSaveAndLoad(t *testing.T) {
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	// Create keystore and add keys
	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	key1 := []byte{0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10}
	key2 := []byte{0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20}

	must(t, ks.Set("AA:BB:CC:DD:EE:FF", key1))
	must(t, ks.Set("11:22:33:44:55:66", key2))

	// Save to disk
	must(t, ks.Save())

	// Create new keystore and load
	ks2, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	keys, err := ks2.Load()
	if err != nil {
		t.Fatalf("Load() failed: %v", err)
	}

	// Check that both keys were loaded
	if len(keys) != 2 {
		t.Errorf("Expected 2 keys, got %d", len(keys))
	}

	// Verify key1
	loadedKey1, exists := ks2.Get("AA:BB:CC:DD:EE:FF")
	if !exists {
		t.Errorf("Key1 was not loaded")
	}
	if !reflect.DeepEqual(loadedKey1, key1) {
		t.Errorf("Loaded key1 doesn't match. Expected %v, got %v", key1, loadedKey1)
	}

	// Verify key2
	loadedKey2, exists := ks2.Get("11:22:33:44:55:66")
	if !exists {
		t.Errorf("Key2 was not loaded")
	}
	if !reflect.DeepEqual(loadedKey2, key2) {
		t.Errorf("Loaded key2 doesn't match. Expected %v, got %v", key2, loadedKey2)
	}
}

func TestLoadNonExistent(t *testing.T) {
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	// Load from non-existent file should return empty map
	keys, err := ks.Load()
	if err != nil {
		t.Fatalf("Load() failed: %v", err)
	}

	if len(keys) != 0 {
		t.Errorf("Expected empty map, got %d keys", len(keys))
	}
}

func TestDelete(t *testing.T) {
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	// Add a key
	key := []byte{0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10}
	must(t, ks.Set("AA:BB:CC:DD:EE:FF", key))

	// Verify it exists
	_, exists := ks.Get("AA:BB:CC:DD:EE:FF")
	if !exists {
		t.Errorf("Key was not added")
	}

	// Delete it
	ks.Delete("AA:BB:CC:DD:EE:FF")

	// Verify it's gone
	_, exists = ks.Get("AA:BB:CC:DD:EE:FF")
	if exists {
		t.Errorf("Key was not deleted")
	}
}

func TestList(t *testing.T) {
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	// Add multiple keys
	key := []byte{0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10}
	must(t, ks.Set("AA:BB:CC:DD:EE:FF", key))
	must(t, ks.Set("11:22:33:44:55:66", key))
	must(t, ks.Set("77:88:99:AA:BB:CC", key))

	// List should return all MAC addresses
	macs := ks.List()
	if len(macs) != 3 {
		t.Errorf("Expected 3 MACs, got %d", len(macs))
	}

	// Check that all MACs are present (order doesn't matter)
	expectedMacs := map[string]bool{
		"AA:BB:CC:DD:EE:FF": true,
		"11:22:33:44:55:66": true,
		"77:88:99:AA:BB:CC": true,
	}
	for _, mac := range macs {
		if !expectedMacs[mac] {
			t.Errorf("Unexpected MAC in list: %s", mac)
		}
	}
}

func TestClear(t *testing.T) {
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	// Add keys
	key := []byte{0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10}
	must(t, ks.Set("AA:BB:CC:DD:EE:FF", key))
	must(t, ks.Set("11:22:33:44:55:66", key))

	// Verify they exist
	if len(ks.List()) != 2 {
		t.Errorf("Expected 2 keys before clear")
	}

	// Clear
	ks.Clear()

	// Verify they're gone
	if len(ks.List()) != 0 {
		t.Errorf("Expected 0 keys after clear")
	}
}

func TestGetAll(t *testing.T) {
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	// Add keys
	key1 := []byte{0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x10}
	key2 := []byte{0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F, 0x20}
	must(t, ks.Set("AA:BB:CC:DD:EE:FF", key1))
	must(t, ks.Set("11:22:33:44:55:66", key2))

	// GetAll
	allKeys := ks.GetAll()

	if len(allKeys) != 2 {
		t.Errorf("Expected 2 keys, got %d", len(allKeys))
	}

	// Verify keys
	if !reflect.DeepEqual(allKeys["AA:BB:CC:DD:EE:FF"], key1) {
		t.Errorf("Key1 doesn't match")
	}
	if !reflect.DeepEqual(allKeys["11:22:33:44:55:66"], key2) {
		t.Errorf("Key2 doesn't match")
	}
}

func TestImmutability(t *testing.T) {
	tmpDir := t.TempDir()
	must(t, os.Setenv("XDG_DATA_HOME", tmpDir))
	defer os.Unsetenv("XDG_DATA_HOME") // nolint: errcheck

	ks, err := New()
	if err != nil {
		t.Fatalf("New() failed: %v", err)
	}

	// Add a key
	originalKey := []byte{0x01, 0x02, 0x03, 0x04}
	keyCopy := make([]byte, len(originalKey))
	copy(keyCopy, originalKey)

	must(t, ks.Set("AA:BB:CC:DD:EE:FF", keyCopy))

	// Modify the original slice
	keyCopy[0] = 0xFF

	// Get the key and verify it wasn't affected
	retrievedKey, _ := ks.Get("AA:BB:CC:DD:EE:FF")
	if !reflect.DeepEqual(retrievedKey, originalKey) {
		t.Errorf("Key was modified externally. Expected %v, got %v", originalKey, retrievedKey)
	}

	// Modify the retrieved key
	retrievedKey[0] = 0xEE

	// Get again and verify internal state wasn't affected
	retrievedKey2, _ := ks.Get("AA:BB:CC:DD:EE:FF")
	if !reflect.DeepEqual(retrievedKey2, originalKey) {
		t.Errorf("Internal key was modified. Expected %v, got %v", originalKey, retrievedKey2)
	}
}
