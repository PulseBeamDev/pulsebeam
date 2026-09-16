from pathlib import Path


def replace_once(path: str, old: str, new: str, label: str) -> None:
    p = Path(path)
    s = p.read_text()
    if old not in s:
        raise SystemExit(f"{label} not found")
    p.write_text(s.replace(old, new, 1))


# Remove the dead queue-delay-policy seam. PulseBeam latency policy remains outside
# SCReAM through admission/allocation/pacer horizons; it cannot mutate SCReAM qdelay.
congestion = Path("crates/pulsebeam-rtc/src/congestion.rs")
s = congestion.read_text()
s = s.replace(
    '''    pub(crate) window_or_pacer_blocked: bool,\n    /// Legacy PulseBeam policy seam. It is intentionally NOT passed to SCReAMv2.\n    pub(crate) queue_delay_ceiling: Duration,\n    pub(crate) ecn: EcnValidation,\n''',
    '''    pub(crate) window_or_pacer_blocked: bool,\n    pub(crate) ecn: EcnValidation,\n''',
    1,
)
s = s.replace(
    '''/// PulseBeam latency policy. These values govern media usefulness, allocation, pacer\n/// horizon, repair, and probing outside SCReAMv2. In particular, queue_delay_ceiling is\n/// NOT a congestion-control input.\n''',
    '''/// PulseBeam latency policy. These values govern media usefulness, allocation, pacer\n/// horizon, repair, and probing outside SCReAMv2.\n''',
    1,
)
s = s.replace('    pub(crate) queue_delay_ceiling: Duration,\n', '', 1)
s = s.replace(
    '''            queue_delay_ceiling: lerp_duration_even(\n                Duration::from_millis(15),\n                Duration::from_millis(60),\n                numerator,\n                denominator,\n            ),\n''',
    '',
    1,
)
helper = '''\n    pub(crate) fn strictest_queue_ceiling<'a>(\n        active: impl IntoIterator<Item = &'a SenderOperatingPoint>,\n    ) -> Option<Duration> {\n        active\n            .into_iter()\n            .map(|point| point.queue_delay_ceiling)\n            .min()\n    }\n'''
if helper not in s:
    raise SystemExit("strictest queue helper not found")
s = s.replace(helper, '', 1)
s = s.replace(
    '''        // SCReAMv2 sees only draft-defined algorithm inputs. PulseBeam's playout-derived\n        // queue_delay_ceiling, offered/admitted rates, and pacer state remain outside it.\n''',
    '''        // SCReAMv2 sees only draft-defined algorithm inputs. PulseBeam's playout,\n        // offered/admitted-rate, and pacer policy remain outside it.\n''',
    1,
)
s = s.replace('            queue_delay_ceiling: Duration::ZERO,\n', '', 1)
# Remove the obsolete test whose only varying input was the deleted seam.
start = s.find('    #[test]\n    fn pulsebeam_queue_policy_cannot_change_scream_state() {')
end_marker = '    #[test]\n    fn synthetic_loss_does_not_refresh_feedback_freshness() {'
end = s.find(end_marker)
if start < 0 or end < 0 or end <= start:
    raise SystemExit("obsolete queue-policy test not found")
s = s[:start] + s[end:]
s = s.replace('            queue_delay_ceiling: Duration::from_millis(15),\n', '', 1)
s = s.replace(
    '        assert!(quality.queue_delay_ceiling >= urgent.queue_delay_ceiling);\n',
    '        assert!(quality.allocation_utilization >= urgent.allocation_utilization);\n',
    1,
)
congestion.write_text(s)

# Stop calculating/passing a path queue ceiling that no longer exists.
egress = Path("crates/pulsebeam-rtc/src/egress.rs")
s = egress.read_text()
block = '''        let points = self\n            .senders\n            .iter()\n            .filter(|sender| sender.policy.desired_bitrate.as_bps() > 0)\n            .map(|sender| {\n                LatencyGovernor::operating_point(\n                    playout_max_ticks(sender.policy),\n                    sender.policy.desired_bitrate.as_bps(),\n                )\n            })\n            .collect::<Vec<_>>();\n        let queue_ceiling = LatencyGovernor::strictest_queue_ceiling(points.iter())\n            .unwrap_or(Duration::from_millis(15));\n'''
if block not in s:
    raise SystemExit("egress queue-ceiling block not found")
s = s.replace(block, '', 1)
s = s.replace('                queue_delay_ceiling: queue_ceiling,\n', '', 1)
egress.write_text(s)

scenario = Path("crates/pulsebeam-rtc/src/congestion/scenario.rs")
s = scenario.read_text()
if '                queue_delay_ceiling: Duration::from_millis(60),\n' not in s:
    raise SystemExit("scenario queue ceiling not found")
scenario.write_text(s.replace('                queue_delay_ceiling: Duration::from_millis(60),\n', '', 1))

# Reconcile the normative design text with the implemented ownership boundary.
doc = Path("crates/pulsebeam-rtc/docs/congestion-control.md")
s = doc.read_text()
s = s.replace(
    '''                per-sender operating points\n                              |\n                              +------ strict queue-target ceiling -----+\n                              |                                         |\n                              v                                         v\npacket feedback ------> self-contained SCReAM v2 core ----------> safe RTP envelope\n                              |                                         |\n                              +----------------+------------------------+\n                                               v\n''',
    '''                per-sender operating points\n                              |\n                              v\npacket feedback ------> self-contained SCReAM v2 core ----------> safe RTP envelope\n                              |                                         |\n                              +----------------+------------------------+\n                                               v\n''',
    1,
)
s = s.replace('- the private PulseBeam queue-delay ceiling.\n', '', 1)
s = s.replace('| Most urgent queue-delay ceiling | `15 ms` |\n', '', 1)
s = s.replace('| Quality-saturated queue-delay ceiling | `60 ms` |\n', '', 1)
s = s.replace('    queue_delay_ceiling: Duration,\n', '', 1)
s = s.replace('queue_delay_ceiling  = lerp(15 ms, 60 ms, x)\n', '', 1)
s = s.replace(
    'The strictest `queue_delay_ceiling` and `probe_queue_impact` among active senders\nbecome the path values. Utilization, demand, frame/RTX usefulness, and service\nbalance remain per sender.\n',
    'The strictest `probe_queue_impact` among active senders becomes the path probe\nlimit. Utilization, demand, frame/RTX usefulness, and service balance remain per\nsender.\n',
    1,
)
s = s.replace('- use the `15 ms` queue-delay ceiling and `0.80` allocation utilization;\n', '- use `0.80` allocation utilization and the urgent pacer horizon;\n', 1)
s = s.replace(
    '- governed demand, pacer horizon, and allocation may fall immediately;\n- the path queue-delay ceiling tightens immediately when this sender becomes the\n  strictest active sender.\n',
    '- governed demand, pacer horizon, probe impact, and allocation may fall immediately.\n',
    1,
)
s = s.replace('- increase its or the path queue-delay ceiling;\n', '', 1)
s = s.replace(
    'Increasing a maximum may relax only that sender\'s private limits and only through\ndamping. It cannot relax a path ceiling still required by another active sender.\n',
    'Increasing a maximum may relax only that sender\'s private limits and only through\ndamping. It cannot alter SCReAM\'s native congestion state.\n',
    1,
)
s = s.replace(
    '- Tightening one active sender cannot relax a shared path ceiling; making it\n  inactive permits the next strictest active sender to control it.\n- `effective_queue_delay_target <= native_queue_delay_target` always.\n',
    '- Tightening one active sender cannot increase SCReAM\'s native queue-delay target,\n  reference window, or target bitrate.\n- `effective_queue_delay_target == native_queue_delay_target` always.\n',
    1,
)
s = s.replace(
    '- the internal effective queue target never exceeds the strictest active ceiling;\n',
    '- PulseBeam policy never mutates the SCReAM-native queue-delay target;\n',
    1,
)
s = s.replace(
    '- after convergence, controlled-bottleneck p99 queue delay is no more than the\n  effective target plus `10 ms`, except during a declared path step or probe;\n',
    '- after convergence, controlled-bottleneck p99 queue delay is no more than the\n  native SCReAM target plus `10 ms`, except during a declared path step or probe;\n',
    1,
)
doc.write_text(s)
