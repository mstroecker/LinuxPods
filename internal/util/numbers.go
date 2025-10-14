package util

func MinOr(a, b *int, defaultValue int) int {
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
