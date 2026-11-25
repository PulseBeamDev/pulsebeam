"""Validate immutable observations without a media decoder or sender."""

import hashlib
import json
from pathlib import Path
import unittest

from observe_vp9_pilot import correlate, elementary, inspect_encoder, restart_switch

ROOT = Path(__file__).resolve().parent.parent / "fixtures/vp9-pilot"


class LayeredPilot(unittest.TestCase):
    def test_fixture_identity(self):
        files = sorted(path.relative_to(ROOT).as_posix() for path in ROOT.glob("*/*"))
        checked = []
        for line in (ROOT / "SHA256SUMS").read_text().splitlines():
            digest, name = line.split("  ", 1)
            self.assertEqual(hashlib.sha256((ROOT / name).read_bytes()).hexdigest(), digest, name)
            checked.append(name)
        self.assertEqual(sorted(checked), files)

    def test_observed_modes_and_reference_experiment(self):
        for mode in ("S2T1", "L2T1"):
            with self.subTest(mode=mode):
                directory = ROOT / mode
                context, units, mapping, wire = correlate(directory)
                self.assertEqual(context["requested_mode"], mode)
                edges = inspect_encoder(units, mapping, mode)
                observation = json.loads((directory / "observations.json").read_text())
                self.assertEqual(observation["encoder_dependencies"], edges)
                self.assertEqual(observation["wire_units"], mapping)

                lower = observation["lost_unit"]
                upper = observation["corrupt_unit"]
                self.assertEqual(int(units[lower][0]["spatial"]), 0)
                self.assertEqual(int(units[upper][0]["spatial"]), 1)
                self.assertEqual(mapping[lower]["rtp_timestamp"], mapping[upper]["rtp_timestamp"])
                self.assertEqual(mode == "L2T1", lower in edges[upper])
                self.assertEqual(observation["lost_packets"], mapping[lower]["packets"])
                encoded = [(row, data, item) for (row, data), item in zip(units, mapping, strict=True)]
                received = [(row, data, item) for (row, _), data, item in zip(units, wire, mapping, strict=True)]
                experiments = {
                    "intact": elementary(encoded, edges),
                    "lost_lower": elementary(received, edges, absent_packets=mapping[lower]["packets"]),
                    "corrupt_retained": elementary(received, edges, corrupt=upper),
                }
                for name, data in experiments.items():
                    self.assertEqual(hashlib.sha256(data).hexdigest(), observation[name]["ivf_sha256"], name)
                intact = observation["intact"]
                self.assertIsNone(intact["error"])
                self.assertEqual(intact["decoder_errors"], [])
                upper_frames = [frame for frame in intact["frames"] if frame["width"] == 640]
                self.assertEqual([frame["pts"] for frame in upper_frames],
                                 [int(row["input_rtp"]) for row, _ in units[1::2]])
                for frame in upper_frames:
                    self.assertEqual((frame["height"], frame["time_base"]), (360, "1/90000"))
                    self.assertEqual([len(bytes.fromhex(x)) for x in frame["planes_sha256"]], [32] * 3)
                self.assertGreater(len({frame["planes_sha256"][0] for frame in upper_frames}), 1)
                restart, selected, switched_ivf = restart_switch(received, edges)
                switch = observation["restart_switch"]
                self.assertEqual(switch["input_rtp"], restart)
                self.assertEqual(switch["units"], selected)
                self.assertEqual(switch["ivf_sha256"], hashlib.sha256(switched_ivf).hexdigest())
                self.assertEqual([f["pts"] for f in switch["pictures"]], [f["pts"] for f in upper_frames])
                self.assertEqual([f for f in switch["pictures"] if f["pts"] >= restart],
                                 [f for f in upper_frames if f["pts"] >= restart])
                for f in switch["pictures"]:
                    size = (320, 180) if f["pts"] < restart else (640, 360)
                    self.assertEqual((f["width"], f["height"], f["time_base"]), (*size, "1/90000"))
                self.assertIsNotNone(observation["corrupt_retained"]["error"])
                self.assertNotEqual(experiments["intact"], experiments["corrupt_retained"])
                loss = observation["lost_lower"]
                if mode == "S2T1":
                    self.assertEqual(loss, intact)
                else:
                    self.assertIsNotNone(loss["error"])
                    self.assertTrue(any("references are available" in log for log in loss["decoder_errors"]))
                    self.assertEqual(loss["frames"], [])


if __name__ == "__main__":
    unittest.main()
