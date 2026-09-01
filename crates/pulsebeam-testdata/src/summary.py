import sys
import os
import statistics

# --- CONFIGURATION (Adjust to match your target WebRTC constraints) ---
TARGET_FPS = 30                # Expected frames per second
BUDGET_BITRATE_KBPS = 1200     # Your WebRTC network allocation budget
SPIKE_THRESHOLD_BYTES = int(BUDGET_BITRATE_KBPS * 1000 / TARGET_FPS / 8 * 2)   # Size where a frame risks stalling a network buffer

def analyze_webrtc_baseline(input_stream):
    try:
        frames = []
        for line in input_stream:
            # Strip tags and handle whitespace
            cleaned = line.split(']')[-1].strip()
            if cleaned:
                frames.append(int(cleaned))
    except ValueError:
        print(f"Error: Invalid data format")
        return

    total_frames = len(frames)
    if total_frames == 0:
        print("No frame data found.")
        return

    # 1. Stream & Bitrate Calculations
    stream_duration = total_frames / TARGET_FPS
    total_bytes = sum(frames)
    total_bits = total_bytes * 8
    
    # Global Avg Bitrate
    avg_bitrate_kbps = (total_bits / stream_duration) / 1000 if stream_duration > 0 else 0
    
    # 2. Keyframe (Max) Analysis
    max_frame_size = max(frames)
    max_frame_idx = frames.index(max_frame_size)
    max_frame_bits = max_frame_size * 8
    
    # How long WebRTC network pacing requires to flush this keyframe over the wire
    pacing_clear_time_ms = (max_frame_bits / (BUDGET_BITRATE_KBPS * 1000)) * 1000

    # 3. Burst & Non-Keyframe Statistics
    frames_excl_key = [f for i, f in enumerate(frames) if i != max_frame_idx]
    avg_delta_frame_bytes = statistics.mean(frames_excl_key)
    stdev_delta_frame_bytes = statistics.stdev(frames_excl_key) if len(frames_excl_key) > 1 else 0
    
    spike_count = sum(1 for size in frames if size > SPIKE_THRESHOLD_BYTES)

    # --- OUTPUT SUMMARY ---
    print("=" * 60)
    print("               WEBRTC PROFILE BASELINE REPORT               ")
    print("=" * 60)
    print(f"Stream Duration (est) : {stream_duration:.2f} seconds (@ {TARGET_FPS} FPS)")
    print(f"Total Frames Profiled : {total_frames}")
    print("-" * 60)
    print(f"Overall Avg Bitrate   : {avg_bitrate_kbps:.1f} kbps")
    print(f"Target Budget Cap     : {BUDGET_BITRATE_KBPS} kbps")
    print(f"Budget Utilization    : {(avg_bitrate_kbps / BUDGET_BITRATE_KBPS) * 100:.1f}%")
    print(f"Budget Status         : {'PASS ✅' if avg_bitrate_kbps <= BUDGET_BITRATE_KBPS else 'OVER BUDGET ❌'}")
    print("-" * 60)
    print(f"Largest Frame (Key)   : {max_frame_size} bytes (idx={max_frame_idx})")
    print(f"Keyframe Pacing Time  : {pacing_clear_time_ms:.1f} ms to flush over network")
    print(f"Avg Delta Frame Size  : {avg_delta_frame_bytes:.1f} bytes (StDev: {stdev_delta_frame_bytes:.1f})")
    print(f"Burst Risk Frames     : {spike_count} frame(s) exceeded {SPIKE_THRESHOLD_BYTES} bytes")
    print("=" * 60)

if __name__ == "__main__":
    if len(sys.argv) > 1:
        # File provided: open it and pass the file object
        file_path = sys.argv[1]
        with open(file_path, 'r') as f:
            analyze_webrtc_baseline(f)
    else:
        # No arg: use stdin
        analyze_webrtc_baseline(sys.stdin)
