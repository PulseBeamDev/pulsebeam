package pulsebeam

import (
	_ "embed"
	"encoding/json"
)

//go:embed development.json
var developmentJSON []byte

// DevelopmentCredentials contains public, insecure credentials for --dev servers.
type DevelopmentCredentials struct {
	APIKey    string `json:"apiKey"`
	ProjectID string `json:"projectId"`
	KeyID     string `json:"keyId"`
	Secret    string `json:"secret"`
	PublicKey string `json:"publicKey"`
}

// InsecureDevCredentials returns the embedded local development identity.
// Never use this with real users or an internet-facing server.
func InsecureDevCredentials() DevelopmentCredentials {
	var credentials DevelopmentCredentials
	if err := json.Unmarshal(developmentJSON, &credentials); err != nil {
		panic("invalid embedded PulseBeam development fixture")
	}
	return credentials
}
