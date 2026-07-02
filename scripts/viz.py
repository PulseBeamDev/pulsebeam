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
COLOR_P999  = "#0ea5e9"  
COLOR_P99   = "#d946ef"  
COLOR_P50   = "#64748b"  
COLOR_TPUT  = "#3b82f6"  
COLOR_RTT_P99   = "#f59e0b"  # Amber for P99 RTT
COLOR_RTT_P999 = "#10b981"  # Emerald for P99.9 RTT


def process_data(lat_csv: str, snap_csv: str, window_secs: float, offset_ms: float, duration_ms: float):
    end_ms = offset_ms + duration_ms if duration_ms else float('inf')

    # ---------------------------------------------------------
    # 1. PROCESS LATENCY (Lightning Fast Filtering)
    # ---------------------------------------------------------
    lat_df = pd.read_csv(lat_csv)
    lat_df.columns = lat_df.columns.str.strip()
    
    lat_df = lat_df[(lat_df["elapsed_ms"] >= offset_ms) & (lat_df["elapsed_ms"] <= end_ms)].copy()
    if lat_df.empty:
        raise ValueError("No latency data found in the specified --offset-ms and --duration-ms window.")

    lat_df["delay_ms"] = lat_df["delay_us"] / 1000.0
    lat_df["plot_time"] = (lat_df["elapsed_ms"] - offset_ms) / 1000.0
    lat_df["window_rel"] = (lat_df["plot_time"] // window_secs) * window_secs

    active_agents = lat_df.groupby("window_rel")["agent_id"].nunique().reset_index()
    active_agents.rename(columns={"agent_id": "agents"}, inplace=True)

    # ---------------------------------------------------------
    # 2. PROCESS SNAPSHOTS (O(N) Fast Smearing + RTT Multi-Percentile Tracking)
    # ---------------------------------------------------------
    snap_df = pd.read_csv(snap_csv)
    snap_df.columns = snap_df.columns.str.strip()
    snap_df = snap_df.sort_values(['agent_id', 'elapsed_ms'])
    
    snap_df['tx_diff'] = snap_df.groupby('agent_id')['tx_bytes'].diff().clip(lower=0)
    snap_df['rx_diff'] = snap_df.groupby('agent_id')['rx_bytes'].diff().clip(lower=0)
    snap_df['dt_sec']  = snap_df.groupby('agent_id')['elapsed_ms'].diff() / 1000.0
    
    snap_df = snap_df[(snap_df["elapsed_ms"] >= offset_ms) & (snap_df["elapsed_ms"] <= end_ms)].copy()
    
    valid_snaps = snap_df[snap_df['dt_sec'] > 0].copy()
    valid_snaps['bytes_total'] = valid_snaps['tx_diff'] + valid_snaps['rx_diff']
    valid_snaps['rate_mbps'] = (valid_snaps['bytes_total'] * 8) / (valid_snaps['dt_sec'] * 1_000_000.0)
    valid_snaps['rtt_ms'] = valid_snaps['rtt_us'] / 1000.0

    valid_snaps['end_rel'] = (valid_snaps['elapsed_ms'] - offset_ms) / 1000.0
    valid_snaps['start_rel'] = valid_snaps['end_rel'] - valid_snaps['dt_sec']
    valid_snaps['window_rel'] = (valid_snaps['end_rel'] // window_secs) * window_secs

    # Extract both P99 and P99.9 RTT from snapshots per time-slice window
    rtt_stats = valid_snaps.groupby('window_rel')['rtt_ms'].agg(
        rtt_p99=lambda x: np.percentile(x, 99),
        rtt_p999=lambda x: np.percentile(x, 99.9)
    ).reset_index()

    max_t = lat_df["window_rel"].max()
    num_bins = int((max_t // window_secs) + 2)
    mbps_totals = np.zeros(num_bins, dtype=float)
    
    for row in valid_snaps.itertuples(index=False):
        start_idx = max(0, int(row.start_rel // window_secs))
        end_idx = min(num_bins, int(row.end_rel // window_secs) + 1)
        mbps_totals[start_idx:end_idx] += row.rate_mbps
        
    times = np.arange(0, num_bins * window_secs, window_secs)
    tput = pd.DataFrame({'window_rel': times, 'mbps_raw': mbps_totals})
    
    tput = pd.merge(tput, active_agents, on="window_rel", how="left").fillna({'agents': 0})
    
    # CONTINUOUS FIX: Merge RTT, then forward fill and backward fill instead of using fillna(0)
    tput = pd.merge(tput, rtt_stats, on="window_rel", how="left")
    tput['rtt_p99'] = tput['rtt_p99'].ffill().bfill().fillna(0)
    tput['rtt_p999'] = tput['rtt_p999'].ffill().bfill().fillna(0)
    
    tput['mbps'] = tput['mbps_raw'].rolling(window=3, center=True, min_periods=1).median()
    tput = tput[tput['window_rel'] <= max_t].copy()

    return lat_df, tput


def plot_benchmark(lat_csv: str, snap_csv: str, label: str, window_secs: float, offset_ms: float, duration_ms: float):
    lat_df, tput = process_data(lat_csv, snap_csv, window_secs, offset_ms, duration_ms)

    # Calculate Latency Percentiles (Including P99.99)
    percentiles = lat_df.groupby("window_rel")["delay_ms"].agg(
        p50=lambda x: np.percentile(x, 50),
        p99=lambda x: np.percentile(x, 99),
        p999=lambda x: np.percentile(x, 99.9),
        p9999=lambda x: np.percentile(x, 99.99),
        pmax=lambda x: np.max(x)
    ).reset_index()

    # CONTINUOUS FIX: Keep timeline continuous during percentiles merge using ffill/bfill
    percentiles = pd.merge(percentiles, tput[['window_rel', 'rtt_p99', 'rtt_p999']], on="window_rel", how="left")
    percentiles['rtt_p99'] = percentiles['rtt_p99'].ffill().bfill().fillna(0)
    percentiles['rtt_p999'] = percentiles['rtt_p999'].ffill().bfill().fillna(0)

    # Summary Stats calculation (Including P99.99)
    max_agents = int(tput["agents"].max())
    p50_med = percentiles["p50"].median()
    p99_med = percentiles["p99"].median()
    p999_med = percentiles["p999"].median()
    p9999_med = percentiles["p9999"].median()
    rtt_p99_med = percentiles["rtt_p99"].median()
    rtt_p999_med = percentiles["rtt_p999"].median()
    max_lat = percentiles["pmax"].max()
    tput_avg = tput["mbps"].mean()
    tput_max = tput["mbps"].max()
    duration_s = int(tput["window_rel"].max())

    title_html = f"""
    <span style="font-size: 22px; font-weight: bold; color: {TEXT_MAIN};">SFU Runtime Jitter Benchmark — {label}</span><br>
    <span style="font-size: 13px; color: {TEXT_MUTED};">
    Peak Concurrency: {max_agents} Agents • Window: {window_secs}s • Sliced Duration: {duration_s}s (Offset: {offset_ms}ms)<br>
    Transit Latency • Med: {p50_med:.2f} ms • P99: {p99_med:.2f} ms • P99.9: {p999_med:.2f} ms • P99.99: {p9999_med:.2f} ms • Max: {max_lat:.2f} ms<br>
    Network RTT      • P99 RTT: {rtt_p99_med:.2f} ms • P99.9 RTT: {rtt_p999_med:.2f} ms • Edge Throughput • Avg: {tput_avg:.2f} Mbps
    </span>
    """

    fig = make_subplots(
        rows=2, cols=1, 
        shared_xaxes=True, 
        row_heights=[0.75, 0.25],
        vertical_spacing=0.08
    )

    # 1. Transit Latency: P99.99 Line
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p9999"],
        name="P99.99 Transit Latency",
        line=dict(color=COLOR_P9999, width=1.5),
        opacity=0.9
    ), row=1, col=1)

    # 2. Transit Latency: P99.9 Line
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p999"],
        name="P99.9 Transit Latency",
        line=dict(color=COLOR_P999, width=1.5),
        opacity=0.8
    ), row=1, col=1)

    # 3. Transit Latency: P99 Line (Glowing effect)
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p99"],
        showlegend=False, hoverinfo='skip',
        line=dict(color=COLOR_P99, width=8),
        opacity=0.15
    ), row=1, col=1)
    
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p99"],
        name="P99 Transit Latency",
        line=dict(color=COLOR_P99, width=2.5)
    ), row=1, col=1)

    # 4. Network RTT: P99.9 Trace (Emerald)
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["rtt_p999"],
        name="P99.9 Round Trip Time (RTT)",
        line=dict(color=COLOR_RTT_P999, width=1.5, dash='dash')
    ), row=1, col=1)

    # 5. Network RTT: P99 Trace (Amber)
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["rtt_p99"],
        name="P99 Round Trip Time (RTT)",
        line=dict(color=COLOR_RTT_P99, width=2.0, dash='dashdot')
    ), row=1, col=1)

    # 6. Transit Latency: P50 Line
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p50"],
        name="P50 Transit (Median)",
        line=dict(color=COLOR_P50, width=1.5, dash='dot')
    ), row=1, col=1)

    # 7. Throughput Fill
    fig.add_trace(go.Scatter(
        x=tput["window_rel"], y=tput["mbps"],
        name="Throughput (Mbps)",
        fill='tozeroy',
        line=dict(color=COLOR_TPUT, width=2),
        fillcolor=f"rgba(59, 130, 246, 0.15)"
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

    # Auto-adjust scaling metrics for the primary viewport limits (Accounting for P99.99)
    max_y_val = max(percentiles["p9999"].max(), percentiles["rtt_p999"].max())
    lat_max = min(max_y_val * 1.2, 500.0) # Elevated max cutoff since P99.99 catches high peaks

    fig.update_yaxes(
        title_text="Latency / RTT [ms]", title_font=dict(color=TEXT_MUTED, size=12),
        range=[0, lat_max],
        showgrid=True, gridwidth=1, gridcolor=GRID_COLOR, zerolinecolor=GRID_COLOR,
        showspikes=True, spikemode="across", spikethickness=1, spikedash="dash", spikecolor=TEXT_MUTED,
        row=1, col=1
    )
    
    fig.update_yaxes(
        title_text="Traffic [Mbps]", title_font=dict(color=TEXT_MUTED, size=12),
        range=[max(0, tput_avg * 0.5), tput_max * 1.2],
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
        rangeslider=dict(visible=True, thickness=0.06, bgcolor="#1e293b"), 
        row=2, col=1
    )

    print(f"✅ Loaded window instantly. Plotting {max_agents} peak concurrent agents with multi-percentile RTT analytics...")
    fig.show()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Interactive SFU Analytics Plotter")
    parser.add_argument("--latency-csv", required=True)
    parser.add_argument("--snapshots-csv", required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--window", type=float, default=1.0)
    parser.add_argument("--offset-ms", type=float, required=True)
    parser.add_argument("--duration-ms", type=float, default=None)
    
    args = parser.parse_args()
    plot_benchmark(args.latency_csv, args.snapshots_csv, args.label, args.window, args.offset_ms, args.duration_ms)
