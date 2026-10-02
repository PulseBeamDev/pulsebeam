package pulsebeam_test

import (
	"crypto/ed25519"
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

type input struct {
	Project     string `json:"project_id"`
	Key         string `json:"key_id"`
	Secret      string `json:"secret"`
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
			got, err := pulsebeam.SignParticipantToken(v.Input.Project, v.Input.Key, v.Input.Secret, v.Input.Room, v.Input.Participant, exp)
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

// Build the compiler contract from the actual public function's runtime type.
// Values not representable by this typed API must fail before signing is possible.
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
	args := []string{`"project"`, `"key"`, `"secret"`, `"room"`, `"participant"`, "uint64(2000)"}
	index := map[string]int{"project_id": 0, "key_id": 1, "secret": 2, "room": 3, "participant": 4, "expiration": 5}[r.Field]
	if r.Value == nil {
		args = append(args[:index], args[index+1:]...)
	} else if r.Representation == "string" {
		args[index] = strconv.Quote(r.Value.(string))
	} else if r.Representation == "number" {
		// Native float64 is deliberately not implicitly assignable to uint64.
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
	base, _, invalid := fixture(t)
	for _, r := range invalid {
		t.Run(r.Name, func(t *testing.T) {
			value, isString := r.Value.(string)
			if !isString || r.Field == "expiration" {
				rejectsTypedCall(t, r)
				return
			}
			in := base
			switch r.Field {
			case "project_id":
				in.Project = value
			case "key_id":
				in.Key = value
			case "secret":
				in.Secret = value
			case "room":
				in.Room = value
			case "participant":
				in.Participant = value
			default:
				t.Fatal("unhandled field")
			}
			token, err := pulsebeam.SignParticipantToken(in.Project, in.Key, in.Secret, in.Room, in.Participant, 2000)
			if err == nil || token != "" {
				t.Fatal("invalid input accepted")
			}
			diagnostic := fmt.Sprintf("%v %#v", err, err)
			if (in.Secret != "" && strings.Contains(diagnostic, in.Secret)) || strings.Contains(diagnostic, "4ccd089b28ff96da") {
				t.Fatal("secret disclosed")
			}
		})
	}
}
