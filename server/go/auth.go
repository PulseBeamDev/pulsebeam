// Package pulsebeam provides server-side PulseBeam participant token signing.
package pulsebeam

import (
	"crypto/ed25519"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"errors"
	"strings"
	"unicode"
	"unicode/utf8"
)

const maxCompactTokenLen = 8192

type Action string

const (
	Join      Action = "join"
	Publish   Action = "publish"
	Discover  Action = "discover"
	Subscribe Action = "subscribe"
)

type TrackKind string

const (
	Audio TrackKind = "audio"
	Video TrackKind = "video"
	Data  TrackKind = "data"
)

type Grant struct {
	Rooms       []string   `json:"rooms"`
	Allow       []Action   `json:"allow"`
	Participant *string    `json:"participant,omitempty"`
	Kind        *TrackKind `json:"kind,omitempty"`
	Label       *string    `json:"label,omitempty"`
}

func validParticipantName(value string) bool {
	if len(value) == 0 || len(value) > 36 {
		return false
	}
	for _, b := range []byte(value) {
		if !(b >= 'A' && b <= 'Z' || b >= 'a' && b <= 'z' || b >= '0' && b <= '9' || b == '_' || b == '-') {
			return false
		}
	}
	return true
}

func validScope(value string) bool {
	if len(value) == 0 || len(value) > 256 || !utf8.ValidString(value) {
		return false
	}
	for _, character := range value {
		if unicode.IsControl(character) {
			return false
		}
	}
	return true
}

func validateGrants(grants []Grant) error {
	if len(grants) > 32 {
		return errors.New("invalid grant count")
	}
	patterns := 0
	for _, grant := range grants {
		patterns += len(grant.Rooms)
		if len(grant.Rooms) == 0 || patterns > 64 {
			return errors.New("invalid room pattern count")
		}
		for _, room := range grant.Rooms {
			if !validScope(room) || (strings.Contains(room, "*") && room != "*" && !(strings.HasSuffix(room, "/*") && strings.Count(room, "*") == 1)) {
				return errors.New("invalid room pattern")
			}
		}
		if len(grant.Allow) == 0 || len(grant.Allow) > 4 {
			return errors.New("invalid actions")
		}
		var seen uint8
		for _, action := range grant.Allow {
			var bit uint8
			switch action {
			case Join:
				bit = 1
			case Publish:
				bit = 2
			case Discover:
				bit = 4
			case Subscribe:
				bit = 8
			default:
				return errors.New("invalid action")
			}
			if seen&bit != 0 {
				return errors.New("duplicate action")
			}
			seen |= bit
		}
		if (grant.Participant != nil && !validScope(*grant.Participant)) || (grant.Label != nil && !validScope(*grant.Label)) {
			return errors.New("invalid grant filter")
		}
		if grant.Kind != nil && *grant.Kind != Audio && *grant.Kind != Video && *grant.Kind != Data {
			return errors.New("invalid track kind")
		}
		if seen&1 != 0 && (grant.Participant != nil || grant.Kind != nil || grant.Label != nil) {
			return errors.New("invalid join filters")
		}
		if seen&2 != 0 && grant.Participant != nil {
			return errors.New("invalid publish participant filter")
		}
	}
	return nil
}

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
func SignParticipantToken(apiKey string, grants []Grant, participant string, expiration uint64) (string, error) {
	if !validParticipantName(participant) {
		return "", errors.New("invalid participant identifier")
	}
	if err := validateGrants(grants); err != nil {
		return "", err
	}
	if grants == nil {
		grants = []Grant{}
	}
	project, key, privateKey, err := parseAPIKey(apiKey)
	if err != nil {
		return "", err
	}
	header, err := json.Marshal(struct {
		Alg string `json:"alg"`
		Kid string `json:"kid"`
		Typ string `json:"typ"`
	}{"EdDSA", key, "pb+jwt"})
	if err != nil {
		return "", errors.New("invalid JWT header")
	}
	claims, err := json.Marshal(struct {
		Iss    string  `json:"iss"`
		Aud    string  `json:"aud"`
		Sub    string  `json:"sub"`
		Exp    uint64  `json:"exp"`
		Grants []Grant `json:"grants"`
	}{project, "pb", participant, expiration, grants})
	if err != nil {
		return "", errors.New("invalid JWT claims")
	}
	encoding := base64.RawURLEncoding
	input := encoding.EncodeToString(header) + "." + encoding.EncodeToString(claims)
	if len(input)+1+encoding.EncodedLen(ed25519.SignatureSize) > maxCompactTokenLen {
		return "", errors.New("participant token exceeds the 8192-byte limit")
	}
	signature := ed25519.Sign(privateKey, []byte(input))
	return input + "." + encoding.EncodeToString(signature), nil
}
