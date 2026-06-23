#!/usr/bin/env python3
import argparse
import sys
import pandas as pd
import matplotlib.pyplot as plt
import seaborn as sns

# Professional engineering publication configurations
sns.set_theme(style="whitegrid")
plt.rcParams.update(
    {
        "font.family": "sans-serif",
        "font.size": 11,
        "axes.labelsize": 11,
        "axes.titlesize": 12,
        "xtick.labelsize": 10,
        "ytick.labelsize": 10,
        "legend.fontsize": 10,
        "figure.titlesize": 15,
        "grid.alpha": 0.3,
    }
)

# High-contrast, colorblind-safe palette (Okabe-Ito inspired)
COLOR_AGENTS = "#7f7f7f"  # Muted grey for background load
COLOR_TX = "#009E73"  # Green for inbound traffic to SFU
COLOR_RX = "#D55E00"  # Vermillion for client fan-out traffic
COLOR_P50 = "#0072B2"  # Pure Blue for median baseline
COLOR_P99 = "#56B4E9"  # Sky Blue for standard tail events


def load_and_process_data(filepath):
    try:
        df = pd.read_csv(filepath)
    except Exception as e:
        print(f"Error reading CSV file: {e}")
        sys.exit(1)

    # Standardize types and cast any string 'NA' elements safely to NaN floats
    metrics = ["fwd_p50_ms", "fwd_p99_ms", "tx_pps", "rx_pps", "agents"]
    for col in metrics:
        if col in df.columns:
            df[col] = pd.to_numeric(df[col], errors="coerce")
    return df


def generate_separated_p99_plots(csv_path, output_img_path):
    df = load_and_process_data(csv_path)

    # 1 Row, 3 Columns: High-impact widescreen presentation for engineering blog layouts
    fig, (ax1, ax2, ax3) = plt.subplots(1, 3, figsize=(18, 5.2))
    mark_every = max(1, len(df) // 12)

    # ========================================================
    # PANEL 1: THROUGHPUT RATE & ACTIVE AGENTS
    # ========================================================
    ax1.plot(
        df["timestamp_s"],
        df["tx_pps"],
        color=COLOR_TX,
        linewidth=2.2,
        marker="^",
        markevery=mark_every,
        label="TX Rate (Inbound)",
    )
    ax1.plot(
        df["timestamp_s"],
        df["rx_pps"],
        color=COLOR_RX,
        linewidth=2.2,
        marker="v",
        markevery=mark_every,
        label="RX Rate (Fan-out)",
    )
    ax1.set_ylabel("Throughput (Packets / sec)", fontweight="bold")
    ax1.set_xlabel("Elapsed Time (seconds)", fontweight="bold")
    ax1.set_title("Packet Processing Volume", pad=10)
    ax1.yaxis.set_major_formatter(plt.FuncFormatter(lambda x, p: f"{int(x):,}"))

    # Secondary axis to mirror agent density onto the throughput grid
    ax1_twin = ax1.twinx()
    ax1_twin.plot(
        df["timestamp_s"],
        df["agents"],
        color=COLOR_AGENTS,
        linestyle=":",
        alpha=0.6,
        label="Active Agents",
    )
    ax1_twin.fill_between(
        df["timestamp_s"], df["agents"], color=COLOR_AGENTS, alpha=0.04
    )
    ax1_twin.set_ylabel("Active Agents", color=COLOR_AGENTS, fontweight="bold")
    ax1_twin.tick_params(axis="y", labelcolor=COLOR_AGENTS)
    ax1_twin.grid(False)

    lines1, labels1 = ax1.get_legend_handles_labels()
    lines2, labels2 = ax1_twin.get_legend_handles_labels()
    ax1.legend(lines1 + lines2, labels1 + labels2, loc="upper left", frameon=True)

    # ========================================================
    # PANEL 2: SEPARATED P50 LATENCY (CORE ENGINE BASELINE)
    # ========================================================
    ax2.plot(
        df["timestamp_s"],
        df["fwd_p50_ms"],
        color=COLOR_P50,
        linewidth=2.5,
        label="P50 Median",
    )
    ax2.set_ylabel("Forwarding Latency (ms)", fontweight="bold")
    ax2.set_xlabel("Elapsed Time (seconds)", fontweight="bold")
    ax2.set_title("P50 Latency: Median User Experience", pad=10)

    # Auto-scale the Y-axis range to stay clean and focused on microsecond values
    max_p50 = df["fwd_p50_ms"].max() if not df["fwd_p50_ms"].dropna().empty else 10
    ax2.set_ylim(0, max_p50 * 1.3)
    ax2.legend(loc="upper right", frameon=True)

    # ========================================================
    # PANEL 3: SEPARATED P99 LATENCY (STANDARD TAIL BOUNDARY)
    # ========================================================
    ax3.plot(
        df["timestamp_s"],
        df["fwd_p99_ms"],
        color=COLOR_P99,
        linewidth=2.5,
        label="P99 Tail",
    )
    ax3.set_ylabel("Forwarding Latency (ms)", fontweight="bold")
    ax3.set_xlabel("Elapsed Time (seconds)", fontweight="bold")
    ax3.set_title("P99 Latency: Standard Tail Boundaries", pad=10)
    ax3.legend(loc="upper right", frameon=True)

    # Shade structural phases across both latency panels to visually mark the test lifecycle
    peak_agents = df["agents"].max()
    ramp_up_end_series = df[df["agents"] >= peak_agents * 0.98]["timestamp_s"]
    if not ramp_up_end_series.empty:
        ramp_up_end = ramp_up_end_series.iloc[0]
        for ax in [ax2, ax3]:
            ax.axvspan(
                df["timestamp_s"].min(), ramp_up_end, color="#d11141", alpha=0.03
            )
            ax.axvspan(
                ramp_up_end, df["timestamp_s"].max(), color="#00aedb", alpha=0.02
            )

    # Global rendering finish pass
    plt.tight_layout()
    fig.suptitle(
        "PulseBeam SFU High-Load Performance Metrics Profile", y=1.04, fontweight="bold"
    )

    plt.savefig(output_img_path, dpi=300, bbox_inches="tight")
    print(f"📸 Separated P50/P99 chart generated successfully: {output_img_path}")


def main():
    parser = argparse.ArgumentParser(
        description="Generate a clean 3-panel performance chart with separate P50 and P99 axes."
    )
    parser.add_argument("csv_file", help="Path to the standard performance log CSV")
    parser.add_argument(
        "-o", "--output", default="sfu_separated_p50_p99.png", help="Output image path"
    )
    args = parser.parse_args()
    generate_separated_p99_plots(args.csv_file, args.output)


if __name__ == "__main__":
    main()
