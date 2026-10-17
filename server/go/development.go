package pulsebeam

import (
	_ "embed"
	"encoding/json"
)

//go:embed development.json
var developmentJSON []byte

// InsecureDevAPIKey returns the public API key for local --dev servers only.
// Never use this with real users or an internet-facing server.
func InsecureDevAPIKey() string {
	var development struct {
		APIKey string `json:"apiKey"`
	}
	if err := json.Unmarshal(developmentJSON, &development); err != nil {
		panic("invalid embedded PulseBeam development fixture")
	}
	return development.APIKey
}
