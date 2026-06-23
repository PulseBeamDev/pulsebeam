#!/usr/bin/env bash

# Hardcoded commit hashes (Replace these placeholder hashes with your actual ones)
VERSION_1="e61896405556543cac4a209c97936476c0e27c8d"
VERSION_2="d656eaf8a2c3d555c12de0d2ba489d7a6ff9ca17"
VERSION_3="0d7d34749857397a907d530a31b2df4aac014fae"
VERSION_4="efcc58bec628f46f3c63072bdca90753e4aae0c8"

echo "------------------------------------------------"
echo " Select a Pulsebeam SFU version to run:"
echo "------------------------------------------------"

# Create an interactive menu
select OPTION in "Version 1 ($VERSION_1)" "Version 2 ($VERSION_2)" "Version 3 ($VERSION_3)" "Version 4 ($VERSION_4)" "Quit"; do
  case $OPTION in
  "Version 1 ($VERSION_1)")
    HASH=$VERSION_1
    LOG="1-baseline"
    break
    ;;
  "Version 2 ($VERSION_2)")
    HASH=$VERSION_2
    LOG="2-tpc"
    break
    ;;
  "Version 3 ($VERSION_3)")
    HASH=$VERSION_3
    LOG="3-jemalloc-tuned"
    break
    ;;
  "Version 4 ($VERSION_4)")
    HASH=$VERSION_4
    LOG="4-arc"
    break
    ;;
  "Quit")
    echo "Exiting script."
    exit 0
    ;;
  *)
    echo "Invalid selection. Please choose a number between 1 and 5."
    ;;
  esac
done

echo "--> Launching Podman with commit: $HASH"

# Your exact command with the chosen hash dynamically inserted
podman run --rm \
  --cpuset-cpus=2-5 \
  --env=RUST_LOG=error \
  --net=host \
  --log-driver=passthrough-tty \
  "ghcr.io/pulsebeamdev/pulsebeam:$HASH" \
  --dev >/dev/null
