package pulsebeam_test

import (
	"crypto/ed25519"
	"crypto/x509"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"go/types"
	"os"
	"reflect"
	"regexp"
	"strconv"
	"strings"
	"testing"

	pulsebeam "github.com/PulseBeamDev/pulsebeam/server/go"
)

const fixtureAPIKey = "p_0067MYZ1001R010000000000064.kid_0067MYZ1005R030010000000068.MC4CAQAwBQYDK2VwBCIEIEzNCJso_5banbbDRuwRTg9bijGfNaumJNqM9u1PuKb7"

type input struct {
	Grants      []pulsebeam.Grant `json:"grants"`
	Participant string            `json:"participant"`
	Expiration  string            `json:"expiration"`
}

type vector struct {
	Name         string `json:"name"`
	Input        input  `json:"input"`
	Header       string `json:"header"`
	Claims       string `json:"claims"`
	SigningInput string `json:"signing_input"`
	Signature    string `json:"signature"`
	Token        string `json:"token"`
	Seed         string `json:"seed_hex"`
	Public       string `json:"public_key_hex"`
}

type rejection struct {
	Name           string `json:"name"`
	Field          string `json:"field"`
	Value          any    `json:"value"`
	Representation string `json:"representation"`
}

func fixture(t *testing.T) (input, []vector, []rejection) {
	t.Helper()
	path := os.Getenv("PULSEBEAM_VECTORS")
	if path == "" {
		path = "../auth/vectors.json"
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var document struct {
		Base    input       `json:"base"`
		Valid   []vector    `json:"valid"`
		Invalid []rejection `json:"invalid"`
	}
	if err := json.Unmarshal(data, &document); err != nil {
		t.Fatal(err)
	}
	return document.Base, document.Valid, document.Invalid
}

func TestConformance(t *testing.T) {
	_, vectors, _ := fixture(t)
	for _, v := range vectors {
		t.Run(v.Name, func(t *testing.T) {
			exp, err := strconv.ParseUint(v.Input.Expiration, 10, 64)
			if err != nil {
				t.Fatal(err)
			}
			issue := func() pulsebeam.SignedParticipantToken {
				t.Helper()
				got, err := pulsebeam.SignParticipantToken(fixtureAPIKey, v.Input.Grants, v.Input.Participant, exp)
				if err != nil {
					t.Fatal(err)
				}
				return got
			}
			got := issue()
			if !regexp.MustCompile(`^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$`).MatchString(got.JTI) || got.JTI == issue().JTI {
				t.Fatal("issuance did not generate a fresh canonical UUIDv4")
			}
			if v.Name == "encoded-token-limit" && len(got.Token) != 8192 {
				t.Fatal("encoded token boundary changed")
			}
			parts := strings.Split(got.Token, ".")
			var fixtureClaims struct {
				JTI string `json:"jti"`
			}
			if err := json.Unmarshal([]byte(v.Claims), &fixtureClaims); err != nil {
				t.Fatal(err)
			}
			for i, expected := range []string{v.Header, strings.Replace(v.Claims, fixtureClaims.JTI, got.JTI, 1)} {
				decoded, err := base64.RawURLEncoding.DecodeString(parts[i])
				if err != nil || string(decoded) != expected {
					t.Fatal("JSON mismatch")
				}
			}
			pub, _ := hex.DecodeString(v.Public)
			sig, _ := base64.RawURLEncoding.DecodeString(parts[2])
			if !ed25519.Verify(pub, []byte(strings.Join(parts[:2], ".")), sig) {
				t.Fatal("independent signature verification failed")
			}
		})
	}
}

// Reflect the linked public signature and require a valid baseline before negatives.
func typedCallError(args []string) error {
	functionType := reflect.TypeOf(pulsebeam.SignParticipantToken)
	pkg := types.NewPackage("consumer", "consumer")
	grantName := types.NewTypeName(token.NoPos, pkg, "Grant", nil)
	grant := types.NewNamed(grantName, types.NewStruct(nil, nil), nil)
	pkg.Scope().Insert(grantName)
	params := make([]*types.Var, functionType.NumIn())
	for i := range params {
		var parameter types.Type
		switch functionType.In(i).Kind() {
		case reflect.String:
			parameter = types.Typ[types.String]
		case reflect.Uint64:
			parameter = types.Typ[types.Uint64]
		case reflect.Slice:
			if functionType.In(i) != reflect.TypeOf([]pulsebeam.Grant{}) {
				return fmt.Errorf("unexpected public slice type")
			}
			parameter = types.NewSlice(grant)
		default:
			return fmt.Errorf("unexpected public parameter type")
		}
		params[i] = types.NewVar(token.NoPos, nil, "", parameter)
	}
	pkg.Scope().Insert(types.NewFunc(token.NoPos, pkg, "Sign", types.NewSignatureType(nil, nil, nil, types.NewTuple(params...), types.NewTuple(), false)))
	set := token.NewFileSet()
	file, err := parser.ParseFile(set, "consumer.go", "package consumer; func call(){ Sign("+strings.Join(args, ",")+") }", 0)
	if err != nil {
		return err
	}
	config := &types.Config{}
	return types.NewChecker(config, set, pkg, nil).Files([]*ast.File{file})
}

func rejectsTypedCall(t *testing.T, r rejection) {
	t.Helper()
	args := []string{`"key"`, `[]Grant{}`, `"participant"`, "uint64(2000)"}
	if err := typedCallError(args); err != nil {
		t.Fatalf("valid signature baseline: %v", err)
	}
	index := map[string]int{"grants": 1, "participant": 2, "expiration": 3}[r.Field]
	if r.Value == nil {
		args = append(args[:index], args[index+1:]...)
	} else if r.Representation == "string" {
		args[index] = strconv.Quote(r.Value.(string))
	} else if r.Representation == "number" {
		value := r.Value.(string)
		if value == "NaN" || value == "Infinity" {
			value = "0"
		}
		args[index] = "float64(" + value + ")"
		if r.Name == "expiration-negative" || r.Name == "expiration-overflow" || r.Name == "expiration-fractional" {
			args[index] = value
		}
	} else if value, ok := r.Value.(string); ok {
		args[index] = strconv.Quote(value)
	} else {
		args[index] = fmt.Sprint(r.Value)
	}
	if typedCallError(args) == nil {
		t.Fatal("invalid input type was accepted")
	}
}

func TestInvalidInputsAndDiagnostics(t *testing.T) {
	base, _, invalid := fixture(t)
	for _, r := range invalid {
		if r.Field != "expiration" && r.Field != "participant" && r.Field != "grants" {
			continue
		}
		t.Run(r.Name, func(t *testing.T) {
			if r.Field == "expiration" {
				rejectsTypedCall(t, r)
				return
			}
			grants, participant := base.Grants, base.Participant
			if r.Field == "grants" {
				if _, ok := r.Value.([]any); !ok {
					rejectsTypedCall(t, r)
					return
				}
				encoded, err := json.Marshal(r.Value)
				if err != nil {
					t.Fatal(err)
				}
				if err := json.Unmarshal(encoded, &grants); err != nil {
					t.Fatal(err)
				}
			} else {
				value, ok := r.Value.(string)
				if !ok {
					rejectsTypedCall(t, r)
					return
				}
				participant = value
			}
			token, err := pulsebeam.SignParticipantToken(fixtureAPIKey, grants, participant, 2000)
			if err == nil || token.Token != "" {
				t.Fatal("invalid input was accepted")
			}
			if strings.Contains(err.Error(), fixtureAPIKey) {
				t.Fatal("diagnostic leaked the API key")
			}
		})
	}
}

func grants(pattern string) []pulsebeam.Grant {
	return []pulsebeam.Grant{{Rooms: []string{pattern}, Allow: []pulsebeam.Action{pulsebeam.Join}}}
}

func TestOpaqueIdentityStrings(t *testing.T) {
	parts := strings.Split(pulsebeam.InsecureDevAPIKey(), ".")
	parts[0], parts[1] = "unknown-issuer", "unknown-key"
	token, err := pulsebeam.SignParticipantToken(strings.Join(parts, "."), grants("room with spaces"), "Participant_With-MixedCase", 2000)
	if err != nil {
		t.Fatal(err)
	}
	encoded := strings.Split(token.Token, ".")
	header, err := base64.RawURLEncoding.DecodeString(encoded[0])
	if err != nil {
		t.Fatal(err)
	}
	claims, err := base64.RawURLEncoding.DecodeString(encoded[1])
	if err != nil {
		t.Fatal(err)
	}
	var h struct {
		Kid string `json:"kid"`
	}
	var c struct {
		Iss    string            `json:"iss"`
		Grants []pulsebeam.Grant `json:"grants"`
		Sub    string            `json:"sub"`
	}
	if json.Unmarshal(header, &h) != nil || json.Unmarshal(claims, &c) != nil {
		t.Fatal("invalid signed JWT")
	}
	if h.Kid != parts[1] || c.Iss != parts[0] || len(c.Grants) != 1 || c.Grants[0].Rooms[0] != "room with spaces" || c.Sub != "Participant_With-MixedCase" {
		t.Fatal("signer modified opaque identities")
	}
}

func TestEmbeddedDevelopmentAPIKey(t *testing.T) {
	path := os.Getenv("PULSEBEAM_DEVELOPMENT")
	if path == "" {
		path = "../auth/development.json"
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var expected struct {
		APIKey    string `json:"apiKey"`
		PublicKey string `json:"publicKey"`
	}
	if err := json.Unmarshal(data, &expected); err != nil {
		t.Fatal(err)
	}
	actual := pulsebeam.InsecureDevAPIKey()
	if actual != expected.APIKey {
		t.Fatal("embedded API key differs from canonical JSON")
	}
	jwt, err := pulsebeam.SignParticipantToken(actual, grants("demo"), "alice", 2000000000)
	if err != nil || jwt.Token == "" {
		t.Fatalf("development token: %v", err)
	}
	publicDER, err := base64.RawURLEncoding.DecodeString(expected.PublicKey)
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := x509.ParsePKIXPublicKey(publicDER)
	if err != nil {
		t.Fatal(err)
	}
	publicKey, ok := parsed.(ed25519.PublicKey)
	if !ok {
		t.Fatal("SPKI is not an Ed25519 public key")
	}
	parts := strings.Split(jwt.Token, ".")
	signature, err := base64.RawURLEncoding.DecodeString(parts[2])
	if err != nil || !ed25519.Verify(publicKey, []byte(parts[0]+"."+parts[1]), signature) {
		t.Fatal("SPKI key cannot verify development JWT")
	}
	for _, bad := range []string{
		strings.Replace(actual, ".MC4CAQ", ".invalid", 1),
		actual[:len(actual)-1],
		actual + ".extra",
		strings.Replace(actual, ".", "..", 1),
	} {
		_, err := pulsebeam.SignParticipantToken(bad, grants("demo"), "alice", 2000000000)
		if err == nil {
			t.Fatal("invalid API key accepted")
		}
	}
}
