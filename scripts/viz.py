import argparse
import numpy as np
import pandas as pd
import plotly.graph_objects as go
from plotly.subplots import make_subplots

# --- Target Aesthetic Colors ---
BG_COLOR    = "#0b0f19"  
GRID_COLOR  = "#1e293b"  
TEXT_MUTED  = "#94a3b8"  
TEXT_MAIN   = "#f8fafc"  

COLOR_P9999 = "#ef4444"  # Vibrant Red for P99.99 Tail Latency
COLOR_P50   = "#64748b"  # Slate Blue for P50
COLOR_TX    = "#3b82f6"  # Vibrant Blue for TX Throughput
COLOR_RX    = "#10b981"  # Emerald Green for RX Throughput


def process_data(lat_csv: str, snap_csv: str, window_secs: float, offset_ms: float, duration_ms: float, metric_col: str):
    end_ms = offset_ms + duration_ms if duration_ms else float('inf')

    # ---------------------------------------------------------
    # 1. PROCESS LATENCY (Extract timeline and delay if selected)
    # ---------------------------------------------------------
    lat_df = pd.read_csv(lat_csv)
    lat_df.columns = lat_df.columns.str.strip()
    
    lat_df = lat_df[(lat_df["elapsed_ms"] >= offset_ms) & (lat_df["elapsed_ms"] <= end_ms)].copy()
    if lat_df.empty:
        raise ValueError("No latency data found in the specified --offset-ms and --duration-ms window.")

    lat_df["plot_time"] = (lat_df["elapsed_ms"] - offset_ms) / 1000.0
    lat_df["window_rel"] = (lat_df["plot_time"] // window_secs) * window_secs

    active_agents = lat_df.groupby("window_rel")["agent_id"].nunique().reset_index()
    active_agents.rename(columns={"agent_id": "agents"}, inplace=True)

    # ---------------------------------------------------------
    # 2. PROCESS SNAPSHOTS (Throughput and RTT tracking)
    # ---------------------------------------------------------
    snap_df = pd.read_csv(snap_csv)
    snap_df.columns = snap_df.columns.str.strip()
    snap_df = snap_df.sort_values(['agent_id', 'elapsed_ms'])
    
    snap_df['tx_diff'] = snap_df.groupby('agent_id')['tx_bytes'].diff().clip(lower=0)
    snap_df['rx_diff'] = snap_df.groupby('agent_id')['rx_bytes'].diff().clip(lower=0)
    snap_df['dt_sec']  = snap_df.groupby('agent_id')['elapsed_ms'].diff() / 1000.0
    
    snap_df = snap_df[(snap_df["elapsed_ms"] >= offset_ms) & (snap_df["elapsed_ms"] <= end_ms)].copy()
    
    valid_snaps = snap_df[snap_df['dt_sec'] > 0].copy()
    valid_snaps['tx_rate_mbps'] = (valid_snaps['tx_diff'] * 8) / (valid_snaps['dt_sec'] * 1_000_000.0)
    valid_snaps['rx_rate_mbps'] = (valid_snaps['rx_diff'] * 8) / (valid_snaps['dt_sec'] * 1_000_000.0)

    valid_snaps['end_rel'] = (valid_snaps['elapsed_ms'] - offset_ms) / 1000.0
    valid_snaps['start_rel'] = valid_snaps['end_rel'] - valid_snaps['dt_sec']
    valid_snaps['window_rel'] = (valid_snaps['end_rel'] // window_secs) * window_secs
    
    max_t = lat_df["window_rel"].max()
    num_bins = int((max_t // window_secs) + 2)
    tx_mbps_totals = np.zeros(num_bins, dtype=float)
    rx_mbps_totals = np.zeros(num_bins, dtype=float)
    
    for row in valid_snaps.itertuples(index=False):
        start_idx = max(0, int(row.start_rel // window_secs))
        end_idx = min(num_bins, int(row.end_rel // window_secs) + 1)
        tx_mbps_totals[start_idx:end_idx] += row.tx_rate_mbps
        rx_mbps_totals[start_idx:end_idx] += row.rx_rate_mbps
        
    times = np.arange(0, num_bins * window_secs, window_secs)
    tput = pd.DataFrame({'window_rel': times, 'tx_mbps_raw': tx_mbps_totals, 'rx_mbps_raw': rx_mbps_totals})
    
    tput = pd.merge(tput, active_agents, on="window_rel", how="left").fillna({'agents': 0})
    tput['tx_mbps'] = tput['tx_mbps_raw'].rolling(window=3, center=True, min_periods=1).median()
    tput['rx_mbps'] = tput['rx_mbps_raw'].rolling(window=3, center=True, min_periods=1).median()
    tput['mbps'] = tput['tx_mbps'] + tput['rx_mbps']
    tput = tput[tput['window_rel'] <= max_t].copy()

    # ---------------------------------------------------------
    # 3. DYNAMIC PERCENTILE COMPUTATION BASED ON SOURCE FILE
    # ---------------------------------------------------------
    if metric_col == "delay_us":
        lat_df["metric_ms"] = lat_df["delay_us"] / 1000.0
        percentiles = lat_df.groupby("window_rel")["metric_ms"].agg(
            p50=lambda x: np.percentile(x, 50),
            p9999=lambda x: np.percentile(x, 99.99),
            pmax=lambda x: np.max(x)
        ).reset_index()
    else:  # rtt_us from snapshots
        valid_snaps["metric_ms"] = valid_snaps["rtt_us"] / 1000.0
        percentiles = valid_snaps.groupby("window_rel")["metric_ms"].agg(
            p50=lambda x: np.percentile(x, 50),
            p9999=lambda x: np.percentile(x, 99.99),
            pmax=lambda x: np.max(x)
        ).reset_index()
        
        # Ensure a continuous timeline for snapshot metrics by alignment with tput timeline
        percentiles = pd.merge(tput[['window_rel']], percentiles, on="window_rel", how="left")
        percentiles['p50'] = percentiles['p50'].ffill().bfill().fillna(0)
        percentiles['p9999'] = percentiles['p9999'].ffill().bfill().fillna(0)
        percentiles['pmax'] = percentiles['pmax'].ffill().bfill().fillna(0)

    return percentiles, tput


def plot_benchmark(lat_csv: str, snap_csv: str, label: str, window_secs: float, offset_ms: float, duration_ms: float, metric_col: str, max_y: float = None):
    percentiles, tput = process_data(lat_csv, snap_csv, window_secs, offset_ms, duration_ms, metric_col)

    # Summary Stats calculation
    max_agents = int(tput["agents"].max())
    p50_med = percentiles["p50"].median()
    p9999_med = percentiles["p9999"].median()
    max_lat = percentiles["pmax"].max()
    tput_avg = tput["mbps"].mean()
    tx_avg = tput["tx_mbps"].mean()
    rx_avg = tput["rx_mbps"].mean()
    tput_max = tput["mbps"].max()
    duration_s = int(tput["window_rel"].max())

    metric_label = "End-to-end Latency (delay_us)" if metric_col == "delay_us" else "Round Trip Time (rtt_us)"

    title_html = f"""
    <span style="font-size: 22px; font-weight: bold; color: {TEXT_MAIN};">{label}</span><br>
    <span style="font-size: 13px; color: {TEXT_MUTED};">
    Peak Concurrency: {max_agents} Agents • Window: {window_secs}s • Sliced Duration: {duration_s}s (Offset: {offset_ms}ms)<br>
    {metric_label} • Med: {p50_med:.2f} ms • P99.99: {p9999_med:.2f} ms • Max: {max_lat:.2f} ms<br>
    Total Throughput • Avg: {tput_avg:.2f} Mbps (TX Avg: {tx_avg:.2f} Mbps • RX Avg: {rx_avg:.2f} Mbps)
    </span>
    """

    fig = make_subplots(
        rows=2, cols=1, 
        shared_xaxes=True, 
        row_heights=[0.75, 0.25],
        vertical_spacing=0.08
    )

    # 1. Target Metric: P99.99 Line
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p9999"],
        name=f"P99.99 {metric_col}",
        line=dict(color=COLOR_P9999, width=1.5),
        opacity=0.9
    ), row=1, col=1)

    # 2. Target Metric: P50 Line
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p50"],
        name=f"P50 {metric_col} (Median)",
        line=dict(color=COLOR_P50, width=1.5, dash='dot')
    ), row=1, col=1)

    # 3. RX Throughput Fill (Stacked Area Part 1)
    fig.add_trace(go.Scatter(
        x=tput["window_rel"], y=tput["rx_mbps"],
        name="RX Throughput",
        mode='lines',
        stackgroup='throughput',
        line=dict(color=COLOR_RX, width=1.5),
        fillcolor="rgba(16, 185, 129, 0.15)"
    ), row=2, col=1)

    # 4. TX Throughput Fill (Stacked Area Part 2)
    fig.add_trace(go.Scatter(
        x=tput["window_rel"], y=tput["tx_mbps"],
        name="TX Throughput",
        mode='lines',
        stackgroup='throughput',
        line=dict(color=COLOR_TX, width=1.5),
        fillcolor="rgba(59, 130, 246, 0.15)"
    ), row=2, col=1)

    # 5. Total Throughput Hidden Trace (Forces total value into unified hover context)
    fig.add_trace(go.Scatter(
        x=tput["window_rel"], y=tput["mbps"],
        name="Total Throughput",
        mode='lines',
        line=dict(color='rgba(0,0,0,0)', width=0),
        showlegend=False
    ), row=2, col=1)

    fig.update_layout(
        title=dict(text=title_html, x=0.5, xanchor='center', y=0.96),
        margin=dict(t=140, b=40, l=60, r=40),
        paper_bgcolor=BG_COLOR,
        plot_bgcolor=BG_COLOR,
        font=dict(family="Inter, -apple-system, sans-serif", color=TEXT_MAIN),
        hovermode="x unified",
        hoverlabel=dict(bgcolor="#1e293b", font_size=13, font_family="monospace"),
        legend=dict(orientation="h", yanchor="bottom", y=1.02, xanchor="right", x=1, bgcolor="rgba(0,0,0,0)"),
    )

    # Scale viewport limit based on explicit argument OR fallback to visible metric
    if max_y is not None:
        lat_max = max_y
    else:
        max_y_val = percentiles["p9999"].max()
        lat_max = min(max_y_val * 1.2, 500.0) if max_y_val > 0 else 100.0

    fig.update_yaxes(
        title_text=f"{metric_col} [ms]", title_font=dict(color=TEXT_MUTED, size=12),
        range=[0, lat_max],
        showgrid=True, gridwidth=1, gridcolor=GRID_COLOR, zerolinecolor=GRID_COLOR,
        showspikes=True, spikemode="across", spikethickness=1, spikedash="dash", spikecolor=TEXT_MUTED,
        row=1, col=1
    )
    
    fig.update_yaxes(
        title_text="Traffic [Mbps]", title_font=dict(color=TEXT_MUTED, size=12),
        range=[0, tput_max * 1.2],
        showgrid=True, gridwidth=1, gridcolor=GRID_COLOR, zerolinecolor=GRID_COLOR,
        showspikes=True, spikemode="across", spikethickness=1, spikedash="dash", spikecolor=TEXT_MUTED,
        row=2, col=1
    )

    fig.update_xaxes(
        showgrid=True, gridwidth=1, gridcolor=GRID_COLOR, zerolinecolor=GRID_COLOR,
        showspikes=True, spikemode="across", spikethickness=1, spikedash="dash", spikecolor=TEXT_MUTED,
    )
    
    fig.update_xaxes(
        title_text="Time since offset [s]", title_font=dict(color=TEXT_MUTED, size=12),
        rangeslider=dict(visible=False),  # Fully removes the range slider window container
        row=2, col=1
    )

    print(f"✅ Loaded window instantly. Plotting {max_agents} peak concurrent agents looking at {metric_col} (P50 & P99.99)...")
    fig.show()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Interactive SFU Analytics Plotter")
    parser.add_argument("--latency-csv", required=True)
    parser.add_argument("--snapshots-csv", required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--window", type=float, default=1.0)
    parser.add_argument("--offset-ms", type=float, required=True)
    parser.add_argument("--duration-ms", type=float, default=None)
    parser.add_argument("--metric", choices=["delay_us", "rtt_us"], default="delay_us", 
                        help="Choose target metric column (delay_us from latency, rtt_us from snapshots)")
    parser.add_argument("--max-y", type=float, default=None,
                        help="Manually cap the maximum Y-axis limit for the latency/RTT plot (in ms)")
    
    args = parser.parse_args()
    plot_benchmark(
        args.latency_csv, 
        args.snapshots_csv, 
        args.label, 
        args.window, 
        args.offset_ms, 
        args.duration_ms, 
        args.metric,
        max_y=args.max_y
    )
