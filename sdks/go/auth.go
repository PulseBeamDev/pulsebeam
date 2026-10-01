// Package pulsebeam provides server-side PulseBeam participant token signing.
package pulsebeam

import (
	"crypto/ed25519"
	"encoding/base64"
	"errors"
	"fmt"
	"strings"
)

const alphabet = "0123456789ABCDEFGHJKMNPQRSTVWXYZ"

func decode(value, prefix string, length int, highPadding bool) ([]byte, error) {
	if len(value) != len(prefix)+2+length || !strings.HasPrefix(value, prefix+"_") || !strings.ContainsRune("0Oo", rune(value[len(prefix)+1])) {
		return nil, errors.New("invalid credential format")
	}
	text := value[len(prefix)+2:]
	var buffer uint16
	var bits uint
	result := make([]byte, 0, length*5/8)
	for i, c := range text {
		if c >= 'a' && c <= 'z' {
			c -= 'a' - 'A'
		}
		switch c {
		case 'O':
			c = '0'
		case 'I', 'L':
			c = '1'
		}
		digit := strings.IndexRune(alphabet, c)
		if digit < 0 {
			return nil, errors.New("invalid credential encoding")
		}
		if i == 0 && highPadding {
			if digit > 1 {
				return nil, errors.New("invalid signing secret padding")
			}
			buffer = uint16(digit)
			bits = 1
		} else {
			buffer = buffer<<5 | uint16(digit)
			bits += 5
		}
		if bits >= 8 {
			bits -= 8
			result = append(result, byte(buffer>>bits))
			buffer &= (1 << bits) - 1
		}
	}
	if buffer != 0 {
		return nil, errors.New("invalid credential padding")
	}
	return result, nil
}

func canonicalID(value, prefix string) (string, error) {
	uuid, err := decode(value, prefix, 26, false)
	if err != nil {
		return "", err
	}
	if uuid[6]>>4 != 7 || uuid[8]&0xc0 != 0x80 {
		return "", errors.New("invalid credential UUID")
	}
	var result strings.Builder
	result.WriteString(prefix + "_0")
	var buffer uint16
	var bits uint
	for _, b := range uuid {
		buffer = buffer<<8 | uint16(b)
		bits += 8
		for bits >= 5 {
			bits -= 5
			result.WriteByte(alphabet[(buffer>>bits)&31])
			buffer &= (1 << bits) - 1
		}
	}
	result.WriteByte(alphabet[buffer<<(5-bits)])
	return result.String(), nil
}

func validExternal(value string) bool {
	if len(value) < 1 || len(value) > 36 {
		return false
	}
	for _, c := range value {
		if !(c >= 'A' && c <= 'Z' || c >= 'a' && c <= 'z' || c >= '0' && c <= '9' || c == '_' || c == '-') {
			return false
		}
	}
	return true
}

// SignParticipantToken signs a participant JWT with explicitly supplied credentials.
// expiration is mandatory absolute Unix seconds, not a TTL. No clock is consulted.
// Keep secret server-side. Registry matching is performed only by the server.
func SignParticipantToken(projectID, keyID, secret, room, participant string, expiration uint64) (string, error) {
	project, err := canonicalID(projectID, "p")
	if err != nil {
		return "", err
	}
	key, err := canonicalID(keyID, "kid")
	if err != nil {
		return "", err
	}
	seed, err := decode(secret, "sk", 52, true)
	if err != nil {
		return "", err
	}
	if !validExternal(room) || !validExternal(participant) {
		return "", errors.New("invalid room or participant external ID")
	}
	header := fmt.Sprintf(`{"alg":"EdDSA","kid":"%s","typ":"pb+jwt"}`, key)
	claims := fmt.Sprintf(`{"iss":"%s","aud":"pb","sub":"%s","room":"%s","exp":%d}`, project, participant, room, expiration)
	encoding := base64.RawURLEncoding
	input := encoding.EncodeToString([]byte(header)) + "." + encoding.EncodeToString([]byte(claims))
	signature := ed25519.Sign(ed25519.NewKeyFromSeed(seed), []byte(input))
	return input + "." + encoding.EncodeToString(signature), nil
}
