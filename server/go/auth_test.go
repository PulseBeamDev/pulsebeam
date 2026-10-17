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
	"strconv"
	"strings"
	"testing"

	pulsebeam "github.com/PulseBeamDev/pulsebeam/server/go"
)

const fixtureAPIKey = "p_0067MYZ1001R010000000000064.kid_0067MYZ1005R030010000000068.MC4CAQAwBQYDK2VwBCIEIEzNCJso_5banbbDRuwRTg9bijGfNaumJNqM9u1PuKb7"

type input struct {
	Room        string `json:"room"`
	Participant string `json:"participant"`
	Expiration  string `json:"expiration"`
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
			got, err := pulsebeam.SignParticipantToken(fixtureAPIKey, v.Input.Room, v.Input.Participant, exp)
			if err != nil || got != v.Token {
				t.Fatalf("token mismatch: %v", err)
			}
			parts := strings.Split(got, ".")
			for i, expected := range []string{v.Header, v.Claims} {
				decoded, err := base64.RawURLEncoding.DecodeString(parts[i])
				if err != nil || string(decoded) != expected {
					t.Fatal("JSON mismatch")
				}
			}
			if strings.Join(parts[:2], ".") != v.SigningInput || parts[2] != v.Signature {
				t.Fatal("signing input/signature mismatch")
			}
			pub, _ := hex.DecodeString(v.Public)
			seed, _ := hex.DecodeString(v.Seed)
			if !reflect.DeepEqual(ed25519.NewKeyFromSeed(seed).Public().(ed25519.PublicKey), ed25519.PublicKey(pub)) {
				t.Fatal("seed/public mismatch")
			}
			sig, _ := base64.RawURLEncoding.DecodeString(v.Signature)
			if !ed25519.Verify(pub, []byte(v.SigningInput), sig) {
				t.Fatal("independent signature verification failed")
			}
		})
	}
}

// Check the compiler contract using the actual public function signature.
func rejectsTypedCall(t *testing.T, r rejection) {
	t.Helper()
	functionType := reflect.TypeOf(pulsebeam.SignParticipantToken)
	params := make([]*types.Var, functionType.NumIn())
	for i := range params {
		kind := types.String
		if functionType.In(i).Kind() == reflect.Uint64 {
			kind = types.Uint64
		}
		params[i] = types.NewVar(token.NoPos, nil, "", types.Typ[kind])
	}
	pkg := types.NewPackage("consumer", "consumer")
	pkg.Scope().Insert(types.NewFunc(token.NoPos, pkg, "Sign", types.NewSignatureType(nil, nil, nil, types.NewTuple(params...), types.NewTuple(), false)))
	args := []string{`"key"`, `"room"`, `"participant"`, "uint64(2000)"}
	index := map[string]int{"room": 1, "participant": 2, "expiration": 3}[r.Field]
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
	} else {
		args[index] = fmt.Sprint(r.Value)
	}
	set := token.NewFileSet()
	file, err := parser.ParseFile(set, "consumer.go", "package consumer; func call(){ Sign("+strings.Join(args, ",")+") }", 0)
	if err != nil {
		t.Fatal(err)
	}
	config := &types.Config{}
	checker := types.NewChecker(config, set, pkg, nil)
	if checker.Files([]*ast.File{file}) == nil {
		t.Fatal("invalid input type was accepted")
	}
}

func TestInvalidInputsAndDiagnostics(t *testing.T) {
	_, _, invalid := fixture(t)
	for _, r := range invalid {
		// Expiration is the only public signing input checked by Go's type system.
		if r.Field != "expiration" {
			continue
		}
		t.Run(r.Name, func(t *testing.T) { rejectsTypedCall(t, r) })
	}
}

func TestOpaqueIdentityStrings(t *testing.T) {
	parts := strings.Split(pulsebeam.InsecureDevAPIKey(), ".")
	parts[0], parts[1] = "unknown-issuer", "unknown-key"
	token, err := pulsebeam.SignParticipantToken(strings.Join(parts, "."), "room with spaces", "participant/with/slashes", 2000)
	if err != nil {
		t.Fatal(err)
	}
	encoded := strings.Split(token, ".")
	header, err := base64.RawURLEncoding.DecodeString(encoded[0])
	if err != nil { t.Fatal(err) }
	claims, err := base64.RawURLEncoding.DecodeString(encoded[1])
	if err != nil { t.Fatal(err) }
	var h struct { Kid string `json:"kid"` }
	var c struct { Iss string `json:"iss"`; Room string `json:"room"`; Sub string `json:"sub"` }
	if json.Unmarshal(header, &h) != nil || json.Unmarshal(claims, &c) != nil {
		t.Fatal("invalid signed JWT")
	}
	if h.Kid != parts[1] || c.Iss != parts[0] || c.Room != "room with spaces" || c.Sub != "participant/with/slashes" {
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
		APIKey string `json:"apiKey"`
		PublicKey string `json:"publicKey"`
	}
	if err := json.Unmarshal(data, &expected); err != nil {
		t.Fatal(err)
	}
	actual := pulsebeam.InsecureDevAPIKey()
	if actual != expected.APIKey {
		t.Fatal("embedded API key differs from canonical JSON")
	}
	jwt, err := pulsebeam.SignParticipantToken(actual, "demo", "alice", 2000000000)
	if err != nil || jwt == "" {
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
	parts := strings.Split(jwt, ".")
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
		_, err := pulsebeam.SignParticipantToken(bad, "demo", "alice", 2000000000)
		if err == nil {
			t.Fatal("invalid API key accepted")
		}
	}
}
