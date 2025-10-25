package ble

import (
	"crypto/aes"
	"fmt"
)

// DecryptProximityPayload decrypts the encrypted portion of a proximity pairing advertisement.
// The encrypted portion is bytes 9-24 (16 bytes) of the BLE advertisement payload.
//
// Parameters:
//   - encryptedData: The 16-byte encrypted payload (bytes 9-24 from advertisement)
//   - key: The 16-byte encryption key (IRK or ENC_KEY from proximity keys)
//
// Returns the decrypted 16-byte payload.
func DecryptProximityPayload(encryptedData []byte, key []byte) ([]byte, error) {
	if len(encryptedData) != 16 {
		return nil, fmt.Errorf("encrypted data must be 16 bytes, got %d", len(encryptedData))
	}

	if len(key) != 16 {
		return nil, fmt.Errorf("encryption key must be 16 bytes, got %d", len(key))
	}

	// Create AES cipher block
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, fmt.Errorf("failed to create AES cipher: %w", err)
	}

	// Decrypt using ECB mode (single block)
	// AES-ECB is likely used here since we're decrypting a single 16-byte block
	// and there's no IV/nonce in the advertisement
	decrypted := make([]byte, 16)
	block.Decrypt(decrypted, encryptedData)

	// Validate decrypted data using known magic bytes
	// If wrong key is used, AES will "succeed" but produce garbage data
	// These patterns help identify correct decryption:
	//   - Byte 0, upper nibble (bits 4-7): Must be 0x0
	//   - Byte 4: Must be 0x2D (magic/validation marker)
	if len(decrypted) >= 5 {
		if (decrypted[0]&0xF0) != 0 || decrypted[4] != 0x2D {
			return nil, fmt.Errorf("decryption validation failed: incorrect encryption key")
		}
	}

	return decrypted, nil
}
