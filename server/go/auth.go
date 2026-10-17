// Package pulsebeam provides server-side PulseBeam participant token signing.
package pulsebeam

import (
	"crypto/ed25519"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"errors"
	"strings"
)

// The public issuer and key IDs are copied into JWT fields as opaque strings.
// Standard x509 handles all private-key encoding and algorithm validation.
func parseAPIKey(apiKey string) (string, string, ed25519.PrivateKey, error) {
	parts := strings.Split(apiKey, ".")
	if len(parts) != 3 {
		return "", "", nil, errors.New("invalid API key")
	}
	der, err := base64.RawURLEncoding.DecodeString(parts[2])
	if err != nil {
		return "", "", nil, errors.New("invalid API key")
	}
	parsed, err := x509.ParsePKCS8PrivateKey(der)
	if err != nil {
		return "", "", nil, errors.New("invalid API key")
	}
	privateKey, ok := parsed.(ed25519.PrivateKey)
	if !ok {
		return "", "", nil, errors.New("invalid API key")
	}
	return parts[0], parts[1], privateKey, nil
}

// SignParticipantToken signs a participant JWT using an opaque server-only API key.
// expiration is absolute Unix seconds, not a TTL. No clock or network is consulted.
func SignParticipantToken(apiKey, room, participant string, expiration uint64) (string, error) {
	project, key, privateKey, err := parseAPIKey(apiKey)
	if err != nil {
		return "", err
	}
	// JSON serialization escapes untrusted text without interpreting ID formats.
	header, err := json.Marshal(struct {
		Alg string `json:"alg"`
		Kid string `json:"kid"`
		Typ string `json:"typ"`
	}{"EdDSA", key, "pb+jwt"})
	if err != nil {
		return "", errors.New("invalid JWT header")
	}
	claims, err := json.Marshal(struct {
		Iss  string `json:"iss"`
		Aud  string `json:"aud"`
		Sub  string `json:"sub"`
		Room string `json:"room"`
		Exp  uint64 `json:"exp"`
	}{project, "pb", participant, room, expiration})
	if err != nil {
		return "", errors.New("invalid JWT claims")
	}
	encoding := base64.RawURLEncoding
	input := encoding.EncodeToString(header) + "." + encoding.EncodeToString(claims)
	signature := ed25519.Sign(privateKey, []byte(input))
	return input + "." + encoding.EncodeToString(signature), nil
}
