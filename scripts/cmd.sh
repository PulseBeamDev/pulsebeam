# RUST_LOG=error ./target/release/pulsebeam-cli --api-url http://192.168.4.1:7070 bench --rooms 5 --max-rooms 15 --users-per-room 4 --session-duration 3600 --join-spread-secs 30 --drain-duration 3600 --arrival-rate 5 --fixed-session | tee c-2.csv
# RUST_LOG=error ./target/release/pulsebeam-cli --api-url http://192.168.4.1:7070 bench --rooms 0 --max-rooms 75 --users-per-room 4 --session-duration 3600 --join-spread-secs 30 --drain-duration 3600 --arrival-rate 1 --fixed-session | tee c-2.csv
RUST_LOG=error ./target/release/pulsebeam-cli --api-url http://192.168.4.1:7070 bench \
  --users-per-room 4 \
  --arrival-rate 0.1 \
  --max-rooms 50 \
  --session-duration 3600
