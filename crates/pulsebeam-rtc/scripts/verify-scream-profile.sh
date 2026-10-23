#!/bin/sh
set -eu

if [ "$#" -gt 1 ]; then
    echo "usage: $0 [libwebrtc-source-root]" >&2
    exit 2
fi
source_root="${1-}"
if [ -n "$source_root" ] && [ ! -d "$source_root" ]; then
    echo "libwebrtc source root is not a directory: $source_root" >&2
    exit 2
fi

revision='326fd06bb4d93900ed6947205b94cefd2413afca'
script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT HUP INT TERM
count=0

while read -r expected_sha256 expected_bytes path; do
    if [ -n "$source_root" ]; then
        artifact="$source_root/$path"
    else
        url="https://webrtc.googlesource.com/src/+/$revision/$path?format=TEXT"
        curl --fail --location --silent --show-error --max-time 30 \
            "$url" --output "$scratch/encoded"
        base64 --decode "$scratch/encoded" > "$scratch/artifact"
        artifact="$scratch/artifact"
    fi

    actual_bytes="$(wc -c < "$artifact" | tr -d ' ')"
    actual_sha256="$(sha256sum "$artifact" | cut -d ' ' -f 1)"
    if [ "$actual_bytes" != "$expected_bytes" ] || [ "$actual_sha256" != "$expected_sha256" ]; then
        echo "SCReAM source mismatch: $path" >&2
        echo "expected $expected_bytes bytes, $expected_sha256" >&2
        echo "actual   $actual_bytes bytes, $actual_sha256" >&2
        exit 1
    fi
    case "$path" in
        LICENSE|PATENTS)
            if ! cmp -s "$artifact" "$script_dir/../src/congestion/screamv2/$path"; then
                echo "retained upstream notice differs: $path" >&2
                exit 1
            fi
            ;;
    esac
    count=$((count + 1))
done <<'SOURCES'
deec8645f2470d59f64b3a01806acecec1eec4dbec71fcf0cd7a355f510a2880 19739 modules/congestion_controller/scream/scream_v2.cc
c0abc8940d37084c310fd2342d75a21abf5a8cdd7ee0c56223d26411074f9258 8516 modules/congestion_controller/scream/scream_v2.h
f52d23653bd7cea721bae25c45f18dd9d8daf93a91a0fe1a638e5c45a486e68a 7189 modules/congestion_controller/scream/scream_v2_parameters.cc
2cdd8d55f59e283e4aad770a1dd8daaad3ed8e48de0c14200154b961d95a06ae 10439 modules/congestion_controller/scream/scream_v2_parameters.h
ca4165cfad409543675e3735af1d900b7e3e7ec0848e4f4fee114a86ba068c87 13400 modules/congestion_controller/scream/scream_network_controller.cc
7c704751f42fcf071e1f30993042645566948b1bdc8722270e22a6b4e368f26c 4104 modules/congestion_controller/scream/scream_network_controller.h
69f7081c94fa790e881727a6393d67292d21a23a587fbabd6d37b00130f90c6a 32673 modules/congestion_controller/scream/scream_v2_unittest.cc
7cf0462ed5c399a34398ca894c02171f74f1cad59448bc75d1bb9355cb0e034c 50484 modules/congestion_controller/scream/scream_network_controller_unittest.cc
faef0903e872ec3863c65b28cbc4cdef8449150f98df56cf0e920e646aca839b 5342 modules/congestion_controller/scream/scream_feedback.cc
d46faf7b202a4b35de6feab5436a6e64b82301f5ca2cdac0ee68d4d37dd40f75 2551 modules/congestion_controller/scream/scream_feedback.h
b91b85edf78c20a0e5572d3970510863f6d4c671117f3f3012f3af514ab6f6ca 12892 modules/congestion_controller/scream/scream_feedback_unittest.cc
8bda4bcd632de3dbae259471f46531c78048f7806226d7022883178773d82642 3554 modules/congestion_controller/scream/loss_estimator.cc
3cc84740ea6735150a8406463d473fe5c86552601ecb1997af06ce0d77182f8b 3711 modules/congestion_controller/scream/loss_estimator.h
e7b8764a5803d70ca0a909090c05fd6b73e70a81a1dc21a3460d447388ec6f7b 4336 modules/congestion_controller/scream/loss_estimator_unittest.cc
adc5358033086839357d08bf42f3802a9b3a392efb99afc9707e3a5db2ef156e 5787 modules/congestion_controller/scream/delay_based_congestion_control.cc
9842fed2029c1768d3aae96a655d3f22cc54b0184f87fbcb044be9bb258d646d 4502 modules/congestion_controller/scream/delay_based_congestion_control.h
940ec1a3516ebe560cc26150a93c7ec6b5e7ee71b4aaeb5fe8a6263d59341565 17283 modules/congestion_controller/scream/delay_based_congestion_control_unittest.cc
5995abc81b4bd9f55432c47e35fb527d2a7fdfcbef84492751721e19710e3c5b 34235 modules/congestion_controller/scream/g3doc/implementation_diff.md
b4f4d95ee346039a0f418e51eb2c1011d4e47e3e159dd0fe8be63e9d8e128e5f 28102 modules/pacing/pacing_controller.cc
131637ad4b57f44c0125faea089a58fadd8e990416c15767da0de2d7efb63f0b 11604 modules/pacing/pacing_controller.h
eb70cea8fe3a207815ba5bfea25dd27fd162645bbd6b1e2ecf4441d4cfefc92e 21748 modules/congestion_controller/rtp/transport_feedback_adapter.cc
d11c7a21558d64b41bd84e89e286cabb1a034bcdadc0e24f01e16be4e4dda989 12847 api/transport/network_types.h
93d26fb352e8c079ab0493fe66011f1367e05a0d0706e7de33b44fdbbc4ccc0c 30631 call/bitrate_allocator.cc
ab00a482b6a3902e40211b43c5d0441962ea99b6cc7c25c0f243fa270b78d482 1511 LICENSE
01462e2068d1a04c2274f3389773014c14ed9bc3446b28303543bd3e3c064145 1388 PATENTS
SOURCES

echo "verified libwebrtc $revision ($count pinned artifacts; no C++ build)"
