package util

func MinOr(a, b *uint8, defaultValue uint8) uint8 {
	if a == nil && b == nil {
		return defaultValue
	}
	if a == nil {
		return *b
	}
	if b == nil {
		return *a
	}
	return min(*a, *b)
}
