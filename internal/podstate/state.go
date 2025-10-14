package podstate

// DataSource indicates where the state data originated from
type DataSource int

const (
	DataSourceUnknown DataSource = iota
	DataSourceBLE                // BLE advertisements (approximate, 5-10% accuracy)
	DataSourceAAP                // AAP protocol (accurate, 1% accuracy)
)

func (d DataSource) String() string {
	switch d {
	case DataSourceBLE:
		return "BLE"
	case DataSourceAAP:
		return "AAP"
	default:
		return "Unknown"
	}
}

// PodSide indicates which AirPod is the primary pod
type PodSide int

const (
	PodSideUnknown PodSide = iota
	PodSideLeft
	PodSideRight
)

func (p PodSide) String() string {
	switch p {
	case PodSideLeft:
		return "Left"
	case PodSideRight:
		return "Right"
	default:
		return "Unknown"
	}
}

// PodState represents the complete state of AirPods, independent of data source.
// This is the unified state object that the PodStateCoordinator provides to all consumers.
type PodState struct {
	// Data source indicator
	Source DataSource

	// Battery levels (0-100), nil if unknown
	LeftBattery  *int
	RightBattery *int
	CaseBattery  *int

	// Charging status
	LeftCharging  bool
	RightCharging bool
	CaseCharging  bool

	// In-ear detection
	LeftInEar  bool
	RightInEar bool

	// Case state
	LidOpen bool

	// Device information
	DeviceModel uint16
	Color       uint8   // AirPods color code
	PrimaryPod  PodSide // Which pod is the primary (determines left/right orientation)

	// Raw data from source (for debugging/future use)
	RawData []byte
}

// HasBatteryData returns true if any battery level is available
func (p *PodState) HasBatteryData() bool {
	return p.LeftBattery != nil || p.RightBattery != nil || p.CaseBattery != nil
}

// LowestBattery returns the lowest battery level, or 0 if no data available
func (p *PodState) LowestBattery() int {
	lowest := 100
	hasData := false

	if p.LeftBattery != nil {
		if *p.LeftBattery < lowest {
			lowest = *p.LeftBattery
		}
		hasData = true
	}

	if p.RightBattery != nil {
		if *p.RightBattery < lowest {
			lowest = *p.RightBattery
		}
		hasData = true
	}

	if p.CaseBattery != nil {
		if *p.CaseBattery < lowest {
			lowest = *p.CaseBattery
		}
		hasData = true
	}

	if !hasData {
		return 0
	}

	return lowest
}
