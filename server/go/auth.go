// Package pulsebeam provides server-side PulseBeam participant token signing.
package pulsebeam

import (
	"crypto/ed25519"
	"encoding/base32"
	"encoding/base64"
	"errors"
	"fmt"
	"strings"
)

var crockford = base32.NewEncoding("0123456789ABCDEFGHJKMNPQRSTVWXYZ").WithPadding(base32.NoPadding)
var aliases = strings.NewReplacer("O", "0", "I", "1", "L", "1")

func decode(value, prefix string, length int, highPadding bool) ([]byte, error) {
	if len(value) != len(prefix)+2+length || !strings.HasPrefix(value, prefix+"_") || !strings.ContainsRune("0Oo", rune(value[len(prefix)+1])) {
		return nil, errors.New("invalid credential format")
	}
	text := value[len(prefix)+2:]
	if strings.IndexFunc(text, func(c rune) bool { return c > 127 }) >= 0 {
		return nil, errors.New("invalid credential encoding")
	}
	text = aliases.Replace(strings.ToUpper(text))
	if highPadding {
		// Twenty added zero bits align the seed's four high padding bits to three bytes.
		text = "0000" + text
	}
	decoded, err := crockford.DecodeString(text)
	// The standard decoder ignores newlines and unused low bits. Require canonical bytes.
	if err != nil || crockford.EncodeToString(decoded) != text {
		return nil, errors.New("invalid credential encoding")
	}
	if highPadding {
		if decoded[0] != 0 || decoded[1] != 0 || decoded[2] != 0 {
			return nil, errors.New("invalid signing secret padding")
		}
		decoded = decoded[3:]
	}
	return decoded, nil
}

func canonicalID(value, prefix string) (string, error) {
	uuid, err := decode(value, prefix, 26, false)
	if err != nil {
		return "", err
	}
	if uuid[6]>>4 != 7 || uuid[8]&0xc0 != 0x80 {
		return "", errors.New("invalid credential UUID")
	}
	return prefix + "_0" + crockford.EncodeToString(uuid), nil
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
