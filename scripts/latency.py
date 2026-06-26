import argparse
import pandas as pd
import numpy as np
import matplotlib.pyplot as plt
import matplotlib.gridspec as gridspec
from matplotlib.ticker import MaxNLocator
from scipy.ndimage import uniform_filter1d


# ---------------------------------------------------------------------------
# Colorblind-safe palette (Okabe-Ito inspired, Tidepool categorical slots)
# Blue for latency, Amber for throughput — distinguishable across all common
# color vision deficiencies, including deuteranopia and protanopia.
# ---------------------------------------------------------------------------
BLUE  = "#2a78d6"   # latency
AMBER = "#eda100"   # throughput
GRID  = "#e1e0d9"   # hairline gridlines
MUTED = "#898781"   # axis ticks and labels
INK   = "#0b0b0b"   # primary text


def _compute_cluster_timeline(snap_df: pd.DataFrame, window_secs: int) -> pd.DataFrame:
    """Aggregate per-agent byte counters into a cluster-level timeline.

    Throughput smoothing rationale
    --------------------------------
    Agents use best-effort statistics polling, so some windows may have
    delayed or skipped samples. Raw per-window byte deltas produce spiky
    throughput curves that misrepresent steady-state capacity. We apply a
    rolling median (window = 3 time bins) to remove single-window outliers
    while preserving genuine load-ramp trends. The raw values are retained
    for reference.
    """
    window_ms = window_secs * 1000
    snap_df = snap_df.copy()
    snap_df["time_bin"] = (snap_df["elapsed_ms"] // window_ms) * window_secs

    # Take the highest counter state each agent reached within each window.
    # Using max() prevents boundary jitter from collapsing deltas to zero.
    agent_max = snap_df.groupby(["time_bin", "agent_id"]).agg(
        tx_max=("tx_bytes", "max"),
        rx_max=("rx_bytes", "max"),
    ).reset_index()

    cluster = agent_max.groupby("time_bin").agg(
        active_agents=("agent_id", "nunique"),
        cluster_tx=("tx_max", "sum"),
        cluster_rx=("rx_max", "sum"),
    ).reset_index().sort_values("time_bin")

    # Byte deltas between consecutive windows
    cluster["tx_delta"] = cluster["cluster_tx"].diff().fillna(0)
    cluster["rx_delta"] = cluster["cluster_rx"].diff().fillna(0)

    # Drop resets (agents reconnecting can temporarily lower the counter sum)
    cluster.loc[cluster["tx_delta"] < 0, "tx_delta"] = 0
    cluster.loc[cluster["rx_delta"] < 0, "rx_delta"] = 0

    total_delta = cluster["tx_delta"] + cluster["rx_delta"]

    # Bytes → Megabits per second:  (bytes × 8 bits) ÷ (1 000 000 × window_s)
    cluster["network_speed_mbps_raw"] = (total_delta * 8) / (1_000_000 * window_secs)

    # Rolling median over 3 consecutive time bins to suppress polling jitter.
    # uniform_filter1d computes a box (mean) filter; for a true median we use
    # a pandas rolling median on the sorted series instead.
    cluster["network_speed_mbps"] = (
        cluster["network_speed_mbps_raw"]
        .rolling(window=3, center=True, min_periods=1)
        .median()
    )

    return cluster


def _compute_latency(lat_df: pd.DataFrame, window_secs: int) -> pd.DataFrame:
    """Compute worst-case per-agent P99.9 latency per time window.

    Why per-agent P99.9, not global P99.9
    ----------------------------------------
    Aggregating all samples into a single pool before computing P99.9 is
    misleading at scale: with N agents each contributing M samples, the
    99.9th percentile of the N×M pool becomes progressively easier to
    satisfy as N grows — even if individual agents are struggling. A rising
    agent count can *mask* degradation.

    Instead we:
      1. Compute P99.9 independently for each agent within each time window.
      2. Take the MAX across agents — the worst-off agent defines the SLO.

    This surface the true tail: if any single agent's stream is degraded,
    the metric reflects it regardless of how well every other agent is doing.
    """
    lat_df = lat_df.copy()
    lat_df["time_bin"] = (lat_df["elapsed_ms"] // (window_secs * 1000)) * window_secs
    lat_df["delay_ms"] = lat_df["delay_us"] / 1000.0

    # Step 1: P99.9 per (window, agent)
    per_agent = (
        lat_df.groupby(["time_bin", "agent_id"])["delay_ms"]
        .agg(p999=lambda x: np.percentile(x, 99.9) if len(x) > 0 else np.nan)
        .reset_index()
    )

    # Step 2: worst-case agent per window
    worst = (
        per_agent.groupby("time_bin")["p999"]
        .max()
        .reset_index()
        .rename(columns={"p999": "worst_agent_p999"})
    )

    # Also carry the median across agents — useful for annotating the spread
    median_across = (
        per_agent.groupby("time_bin")["p999"]
        .median()
        .reset_index()
        .rename(columns={"p999": "median_agent_p999"})
    )

    return pd.merge(worst, median_across, on="time_bin")


def _apply_blog_style() -> None:
    """Apply clean, publication-ready rcParams."""
    plt.rcParams.update({
        "font.family":        "sans-serif",
        "font.sans-serif":    ["Inter", "Helvetica Neue", "Arial", "DejaVu Sans"],
        "axes.spines.top":    False,
        "axes.spines.right":  False,
        "axes.spines.left":   False,
        "axes.spines.bottom": True,
        "axes.edgecolor":     GRID,
        "axes.linewidth":     0.8,
        "axes.grid":          True,
        "axes.axisbelow":     True,
        "grid.color":         GRID,
        "grid.linewidth":     0.6,
        "grid.linestyle":     ":",
        "xtick.color":        MUTED,
        "ytick.color":        MUTED,
        "xtick.labelsize":    9,
        "ytick.labelsize":    9,
        "xtick.direction":    "out",
        "ytick.direction":    "out",
        "xtick.major.size":   3,
        "ytick.major.size":   0,
        "figure.facecolor":   "white",
        "axes.facecolor":     "white",
        "text.color":         INK,
    })


def show_accurate_pulsebeam_chart(
    latency_csv: str,
    snapshots_csv: str,
    window_secs: int = 5,
) -> None:
    # -----------------------------------------------------------------------
    # 1. Load and process
    # -----------------------------------------------------------------------
    snap_df = pd.read_csv(snapshots_csv)
    lat_df  = pd.read_csv(latency_csv)

    cluster = _compute_cluster_timeline(snap_df, window_secs)
    lat_agg = _compute_latency(lat_df, window_secs)

    merged = (
        pd.merge(lat_agg, cluster, on="time_bin")
        .dropna()
        .sort_values("active_agents")
    )

    # Drop cold-start noise — system hasn't stabilised yet below 15 agents
    merged = merged[merged["active_agents"] >= 15]

    x            = merged["active_agents"].values
    worst_p999   = merged["worst_agent_p999"].values
    median_p999  = merged["median_agent_p999"].values
    mbps         = merged["network_speed_mbps"].values
    mbps_raw     = merged["network_speed_mbps_raw"].values

    # -----------------------------------------------------------------------
    # 2. Summary stats
    # -----------------------------------------------------------------------
    peak_agents = int(x.max())
    max_lat     = worst_p999.max()
    peak_mbps   = mbps.max()

    # -----------------------------------------------------------------------
    # 3. Layout: two stacked panels sharing the x-axis
    # -----------------------------------------------------------------------
    _apply_blog_style()

    fig = plt.figure(figsize=(11, 8), dpi=300)
    gs  = gridspec.GridSpec(
        2, 1,
        figure=fig,
        height_ratios=[1, 1],
        hspace=0.08,
    )

    ax_lat  = fig.add_subplot(gs[0])
    ax_tput = fig.add_subplot(gs[1], sharex=ax_lat)

    # -----------------------------------------------------------------------
    # 4. Top panel — worst-case per-agent P99.9 playout delay
    # -----------------------------------------------------------------------

    # Shaded band: worst-case vs median shows the spread across agents
    ax_lat.fill_between(
        x, median_p999, worst_p999,
        alpha=0.12, color=BLUE, zorder=2,
        label="_nolegend_",
    )

    # Median agent line (quieter, dashed)
    ax_lat.plot(
        x, median_p999,
        color=BLUE, linewidth=1.2, linestyle="--",
        alpha=0.55, zorder=3,
    )

    # Worst-case agent line (primary signal)
    ax_lat.plot(
        x, worst_p999,
        color=BLUE, linewidth=2, zorder=4,
        solid_capstyle="round", solid_joinstyle="round",
    )

    # 100 ms ceiling reference line
    ax_lat.axhline(
        100, color=MUTED, linewidth=0.8,
        linestyle=(0, (6, 4)),
        zorder=1,
    )
    ax_lat.text(
        x[-1], 102, "100 ms ceiling",
        fontsize=8, color=MUTED, ha="right", va="bottom",
    )

    # Inline labels
    mid = len(x) // 3
    ax_lat.text(
        x[mid], worst_p999[mid] + 4,
        "Worst-agent P99.9",
        fontsize=9, color=BLUE, fontweight="medium",
    )
    ax_lat.text(
        x[mid], median_p999[mid] - 6,
        "Median-agent P99.9",
        fontsize=8, color=BLUE, alpha=0.65,
    )

    ax_lat.set_ylim(0, 115)
    ax_lat.yaxis.set_major_locator(MaxNLocator(nbins=4, integer=True))
    ax_lat.set_ylabel("Playout delay (ms)", fontsize=10, color=MUTED, labelpad=8)
    ax_lat.grid(axis="x", visible=False)
    plt.setp(ax_lat.get_xticklabels(), visible=False)
    ax_lat.tick_params(axis="x", length=0)

    # -----------------------------------------------------------------------
    # 5. Bottom panel — aggregate throughput (smoothed + raw ghost)
    # -----------------------------------------------------------------------

    # Raw polling data as a faint ghost so readers can see the jitter
    ax_tput.plot(
        x, mbps_raw,
        color=AMBER, linewidth=0.8, alpha=0.25, zorder=2,
    )

    # Smoothed line (rolling median, 3-bin window)
    ax_tput.plot(
        x, mbps,
        color=AMBER, linewidth=2, zorder=3,
        solid_capstyle="round", solid_joinstyle="round",
    )
    ax_tput.fill_between(x, mbps, alpha=0.10, color=AMBER, zorder=1)

    mid_t = len(x) // 3
    ax_tput.text(
        x[mid_t], mbps[mid_t] + mbps.max() * 0.05,
        "Total throughput (3-bin rolling median)",
        fontsize=9, color=AMBER, fontweight="medium",
    )

    ax_tput.set_ylim(0, mbps.max() * 1.25)
    ax_tput.yaxis.set_major_locator(MaxNLocator(nbins=4, integer=True))
    ax_tput.set_ylabel("Aggregate traffic (Mbps)", fontsize=10, color=MUTED, labelpad=8)
    ax_tput.set_xlabel("Concurrent agents", fontsize=10, color=MUTED, labelpad=8)

    # -----------------------------------------------------------------------
    # 6. Summary annotation box
    # -----------------------------------------------------------------------
    summary = (
        f"Peak agents:  {peak_agents}\n"
        f"Max P99.9:    {max_lat:.1f} ms  (worst agent)\n"
        f"Peak traffic: {peak_mbps:.1f} Mbps"
    )
    ax_lat.text(
        0.02, 0.96, summary,
        transform=ax_lat.transAxes,
        fontsize=8.5, color=INK,
        va="top", ha="left",
        linespacing=1.6,
        family="monospace",
        bbox=dict(
            boxstyle="round,pad=0.4",
            facecolor="white",
            edgecolor=GRID,
            linewidth=0.8,
            alpha=0.92,
        ),
    )

    # -----------------------------------------------------------------------
    # 7. Title block
    # -----------------------------------------------------------------------
    fig.text(
        0.015, 0.975,
        "PulseBeam infrastructure scalability",
        fontsize=15, fontweight="bold", color=INK,
        va="top", ha="left",
    )
    fig.text(
        0.015, 0.952,
        "Per-agent worst-case P99.9 latency vs smoothed aggregate throughput · 5-second windows",
        fontsize=10, color=MUTED,
        va="top", ha="left",
    )

    # -----------------------------------------------------------------------
    # 8. Legend
    # -----------------------------------------------------------------------
    legend_handles = [
        plt.Line2D([0], [0], color=BLUE, linewidth=2,
                   label="Worst-agent P99.9 playout delay (ms)"),
        plt.Line2D([0], [0], color=BLUE, linewidth=1.2, linestyle="--", alpha=0.55,
                   label="Median-agent P99.9 playout delay (ms)"),
        plt.Line2D([0], [0], color=AMBER, linewidth=2,
                   label="Aggregate throughput, smoothed (Mbps)"),
        plt.Line2D([0], [0], color=AMBER, linewidth=0.8, alpha=0.25,
                   label="Aggregate throughput, raw (Mbps)"),
    ]
    fig.legend(
        handles=legend_handles,
        loc="lower center",
        ncol=2,
        frameon=True,
        framealpha=0.95,
        edgecolor=GRID,
        fontsize=9,
        bbox_to_anchor=(0.5, 0.01),
    )

    fig.subplots_adjust(left=0.09, right=0.97, top=0.93, bottom=0.12)

    plt.savefig("pulsebeam_scalability.png", dpi=300, bbox_inches="tight")
    plt.show()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Blog-ready PulseBeam WebRTC scalability chart. "
                    "Per-agent worst-case P99.9 latency, smoothed throughput."
    )
    parser.add_argument("--latency-csv",   default="latency.csv")
    parser.add_argument("--snapshots-csv", default="snapshots.csv")
    parser.add_argument("--window-secs",   type=int, default=5)

    args = parser.parse_args()
    show_accurate_pulsebeam_chart(
        args.latency_csv,
        args.snapshots_csv,
        args.window_secs,
    )
