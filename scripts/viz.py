import argparse
import numpy as np
import pandas as pd
import plotly.graph_objects as go
from plotly.subplots import make_subplots
import re

# --- Target Aesthetic Colors (Colorblind-Friendly Light Theme) ---
BG_COLOR    = "#ffffff"  # Crisp White Background
GRID_COLOR  = "#e2e8f0"  # Soft Light Gray Gridlines
TEXT_MUTED  = "#475569"  # Cool Gray Muted Text
TEXT_MAIN   = "#0f172a"  # Dark Slate Main Text

# High-contrast, non-overlapping color choices for color blindness
COLOR_P9999 = "#dc2626"  # Crimson Red for P99.99 Tail Latency
COLOR_P50   = "#1e293b"  # Dark Charcoal for P50 Median
COLOR_TX    = "#2563eb"  # Vivid Blue for TX Throughput
COLOR_RX    = "#d97706"  # Amber/Orange for RX Throughput


def process_data(lat_csv: str, snap_csv: str, window_secs: float, offset_ms: float, duration_ms: float, metric_col: str):
    end_ms = offset_ms + duration_ms if duration_ms else float('inf')

    # 1. PROCESS LATENCY
    lat_df = pd.read_csv(lat_csv)
    lat_df.columns = lat_df.columns.str.strip()
    
    lat_df = lat_df[(lat_df["elapsed_ms"] >= offset_ms) & (lat_df["elapsed_ms"] <= end_ms)].copy()
    if lat_df.empty:
        raise ValueError("No latency data found in the specified --offset-ms and --duration-ms window.")

    lat_df["plot_time"] = (lat_df["elapsed_ms"] - offset_ms) / 1000.0
    lat_df["window_rel"] = (lat_df["plot_time"] // window_secs) * window_secs

    active_agents = lat_df.groupby("window_rel")["agent_id"].nunique().reset_index()
    active_agents.rename(columns={"agent_id": "agents"}, inplace=True)

    # 2. PROCESS SNAPSHOTS
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
    
    if 'loss_pct' in valid_snaps.columns:
        loss_grouped = valid_snaps.groupby('window_rel')['loss_pct'].mean().reset_index()
        tput = pd.merge(tput, loss_grouped, on="window_rel", how="left").fillna({'loss_pct': 0.0})
    else:
        tput['loss_pct'] = 0.0

    tput = pd.merge(tput, active_agents, on="window_rel", how="left").fillna({'agents': 0})
    tput['tx_mbps'] = tput['tx_mbps_raw'].rolling(window=3, center=True, min_periods=1).median()
    tput['rx_mbps'] = tput['rx_mbps_raw'].rolling(window=3, center=True, min_periods=1).median()
    tput['mbps'] = tput['tx_mbps'] + tput['rx_mbps']
    tput = tput[tput['window_rel'] <= max_t].copy()

    # 3. DYNAMIC PERCENTILE COMPUTATION
    if metric_col == "delay_us":
        lat_df["metric_ms"] = lat_df["delay_us"] / 1000.0
        percentiles = lat_df.groupby("window_rel")["metric_ms"].agg(
            p50=lambda x: np.percentile(x, 50),
            p9999=lambda x: np.percentile(x, 99.99),
            pmax=lambda x: np.max(x)
        ).reset_index()
    else:
        valid_snaps["metric_ms"] = valid_snaps["rtt_us"] / 1000.0
        percentiles = valid_snaps.groupby("window_rel")["metric_ms"].agg(
            p50=lambda x: np.percentile(x, 50),
            p9999=lambda x: np.percentile(x, 99.99),
            pmax=lambda x: np.max(x)
        ).reset_index()
        
        percentiles = pd.merge(tput[['window_rel']], percentiles, on="window_rel", how="left")
        percentiles['p50'] = percentiles['p50'].ffill().bfill().fillna(0)
        percentiles['p9999'] = percentiles['p9999'].ffill().bfill().fillna(0)
        percentiles['pmax'] = percentiles['pmax'].ffill().bfill().fillna(0)

    return percentiles, tput


def plot_benchmark(lat_csv: str, snap_csv: str, label: str, window_secs: float, offset_ms: float, duration_ms: float, metric_col: str, output_html: str = None):
    percentiles, tput = process_data(lat_csv, snap_csv, window_secs, offset_ms, duration_ms, metric_col)

    max_agents = int(tput["agents"].max())
    p50_med = percentiles["p50"].median()
    p9999_med = percentiles["p9999"].median()
    max_lat = percentiles["pmax"].max()
    tput_avg = tput["mbps"].mean()
    tput_max = tput["mbps"].max()
    loss_avg = tput["loss_pct"].mean()

    metric_label = "Latency" if metric_col == "delay_us" else "RTT"

    fig = make_subplots(
        rows=2, cols=1, 
        shared_xaxes=True, 
        row_heights=[0.70, 0.30],
        vertical_spacing=0.10     
    )

    # 1. Target Metric: P99.99 Line
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p9999"],
        name=f"P99.99 Tail",
        line=dict(color=COLOR_P9999, width=3.0),
        opacity=0.95
    ), row=1, col=1)

    # 2. Target Metric: P50 Line
    fig.add_trace(go.Scatter(
        x=percentiles["window_rel"], y=percentiles["p50"],
        name=f"P50 Median",
        line=dict(color=COLOR_P50, width=2.5, dash='dash')
    ), row=1, col=1)

    # 3. RX Throughput Fill
    fig.add_trace(go.Scatter(
        x=tput["window_rel"], y=tput["rx_mbps"],
        name="RX Throughput",
        mode='lines',
        stackgroup='throughput',
        line=dict(color=COLOR_RX, width=2.0),
        fillcolor="rgba(217, 119, 6, 0.15)"
    ), row=2, col=1)

    # 4. TX Throughput Fill
    fig.add_trace(go.Scatter(
        x=tput["window_rel"], y=tput["tx_mbps"],
        name="TX Throughput",
        mode='lines',
        stackgroup='throughput',
        line=dict(color=COLOR_TX, width=2.0),
        fillcolor="rgba(37, 99, 235, 0.15)"
    ), row=2, col=1)

    fig.add_trace(go.Scatter(
        x=tput["window_rel"], y=tput["mbps"],
        name="Total Throughput",
        mode='lines',
        line=dict(color='rgba(0,0,0,0)', width=0),
        showlegend=False
    ), row=2, col=1)

    # --- REALLOCATED LAYOUT HEIGHTS ---
    inline_title_html = f"""
    <span style="font-family: Inter, sans-serif; font-size: 12px; font-weight: 700; color: {TEXT_MAIN};">{label}</span>
    <br>
    <span style="font-family: Inter, sans-serif; font-size: 12px; color: {TEXT_MUTED}; margin-left: 4px;">
        ({max_agents}Agents • Loss: {loss_avg:.2f}%) | <b>{metric_label}:</b> Med {p50_med:.1f}ms / P99.99 <span style="color: {COLOR_P9999}; font-weight: 600;">{p9999_med:.1f}ms</span> / Max {max_lat:.1f}ms | <b>Throughput:</b> {tput_avg:.1f}Mbps
    </span>
    """

    fig.update_layout(
        title=dict(
            text=inline_title_html,
            x=0.01,             # Flush to the left edge
            xanchor='left',
            y=0.98,             # Tucked right into the upper ceiling
            yanchor='top'
        ),
        margin=dict(t=55, b=50, l=70, r=30), # Reclaimed maximum space: top margin dropped down to 55px
        paper_bgcolor=BG_COLOR,
        plot_bgcolor=BG_COLOR,
        font=dict(family="Inter, -apple-system, sans-serif", color=TEXT_MAIN, size=13),
        hovermode="x unified",
        hoverlabel=dict(bgcolor="#f8fafc", font_size=13, font_family="monospace", font_color=TEXT_MAIN, bordercolor="#e2e8f0"),
        legend=dict(
            orientation="h", 
            yanchor="bottom", 
            y=1.02, 
            xanchor="right", 
            x=1.0,         
            bgcolor="rgba(0,0,0,0)",
            font=dict(size=12)
        ),
    )

    # --- AXIS LABELS AND TICK UPGRADES ---
    fig.update_yaxes(
        title_text=f"{metric_col} [ms]", 
        title_font=dict(color=TEXT_MUTED, size=14, family="Inter, sans-serif"),
        tickfont=dict(size=13), 
        showgrid=True, gridwidth=1, gridcolor=GRID_COLOR, zerolinecolor=GRID_COLOR,
        showspikes=True, spikemode="across", spikethickness=1, spikedash="dash", spikecolor=TEXT_MUTED,
        row=1, col=1
    )
    
    fig.update_yaxes(
        title_text="Traffic [Mbps]", 
        title_font=dict(color=TEXT_MUTED, size=14, family="Inter, sans-serif"),
        tickfont=dict(size=13), 
        range=[0, tput_max * 1.2],
        showgrid=True, gridwidth=1, gridcolor=GRID_COLOR, zerolinecolor=GRID_COLOR,
        showspikes=True, spikemode="across", spikethickness=1, spikedash="dash", spikecolor=TEXT_MUTED,
        row=2, col=1
    )

    fig.update_xaxes(
        tickfont=dict(size=13),
        showgrid=True, gridwidth=1, gridcolor=GRID_COLOR, zerolinecolor=GRID_COLOR,
        showspikes=True, spikemode="across", spikethickness=1, spikedash="dash", spikecolor=TEXT_MUTED,
    )
    
    fig.update_xaxes(
        title_text="Time since offset [s]", 
        title_font=dict(color=TEXT_MUTED, size=14, family="Inter, sans-serif"),
        rangeslider=dict(visible=False),
        row=2, col=1
    )

    if not output_html:
        clean_label = re.sub(r'[^a-zA-Z0-9_\-]', '_', label.lower().strip())
        output_html = f"benchmark_{clean_label}.html"

    print(f"✅ Data processed. Saving interactive HTML report to: {output_html}")
    fig.write_html(output_html, include_plotlyjs="cdn")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Interactive SFU Analytics Plotter")
    parser.add_argument("--latency-csv", required=True)
    parser.add_argument("--snapshots-csv", required=True)
    parser.add_argument("--label", required=True)
    parser.add_argument("--window", type=float, default=1.0)
    parser.add_argument("--offset-ms", type=float, required=True)
    parser.add_argument("--duration-ms", type=float, default=None)
    parser.add_argument("--metric", choices=["delay_us", "rtt_us"], default="delay_us")
    parser.add_argument("--output", type=str, default=None)
    
    args = parser.parse_args()
    plot_benchmark(
        args.latency_csv, 
        args.snapshots_csv, 
        args.label, 
        args.window, 
        args.offset_ms, 
        args.duration_ms, 
        args.metric,
        output_html=args.output
    )
