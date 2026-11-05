import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import analyze


def record(timestamp, operation, message, color=False):
    header = f'  {timestamp}  INFO nativelink_worker::running_actions_manager: {message}'
    span = (f'    in nativelink_worker::local_worker::worker_start_action_ctx '
            f'with operation_id: "{operation}", digest_function: SHA256')
    if color:
        header = f'  \x1b[2m{timestamp}\x1b[0m \x1b[32m INFO\x1b[0m ' \
                 f'\x1b[1;32mnativelink_worker::running_actions_manager\x1b[0m: {message}'
        span = span.replace('operation_id', '\x1b[1moperation_id\x1b[0m')
    return header + '\n    at nativelink-worker/src/running_actions_manager.rs:3518\n' + span + '\n\n'


def start(time, op, args='["clang", "-c", "a.c"]', color=False):
    return record(time, op, f'Executing command, args: {args}, command_directory: /work/{op}/work/out', color)


def complete(time, op, code=0, color=False):
    return record(time, op, f'Command complete, exit_code: {code}, outcome: "COMPLETED"', color)


class AnalyzeTest(unittest.TestCase):
    def test_interleaved_colored_records_sorted_by_event_time(self):
        log = (start('2026-11-05T20:45:41.1Z', 'a', color=True)
               + complete('2026-11-05T20:45:43Z', 'a')
               + start('2026-11-05T20:45:42Z', 'b', color=True)
               + complete('2026-11-05T20:45:44Z', 'b', 3, color=True)
               + start('2026-11-05T20:45:45Z', 'c', '["other"]'))
        result = analyze.analyze(log)
        self.assertEqual((result['starts'], result['completions'], result['peak_simultaneous_executing']), (3, 2, 2))
        self.assertEqual(result['nonzero_exits'], 1)
        self.assertEqual(result['incomplete_operation_ids'], ['c'])
        self.assertEqual(result['duplicate_command_fingerprints'][0]['operation_ids'], ['a', 'b'])
        self.assertEqual(result['duplicate_command_fingerprints'][0]['count'], 2)
        self.assertEqual(result, analyze.analyze(log.replace('\x1b[2m', '').replace('\x1b[32m', '')
                                                .replace('\x1b[1;32m', '').replace('\x1b[1m', '')
                                                .replace('\x1b[0m', '')))

    def test_json_arguments_and_distinct_fingerprints(self):
        a = '["echo", "a, ] \\\"quoted\\\"", "\\\\path"]'
        b = '[ "echo", "a, ] \\\"quoted\\\"", "\\\\path" ]'
        log = start('2026-01-01T00:00:00Z', 'a', a) + start('2026-01-01T00:00:01Z', 'b', b)
        log += start('2026-01-01T00:00:02Z', 'c', '["echo", "different"]')
        result = analyze.analyze(log)
        self.assertEqual(len(result['duplicate_command_fingerprints']), 1)
        self.assertEqual(result['duplicate_command_fingerprints'][0]['operation_ids'], ['a', 'b'])
        self.assertEqual(result['diagnostics']['invalid_command_arguments'], 0)

    def test_ties_orphans_and_invalid_records_are_explicit(self):
        log = (complete('2026-01-01T00:00:00Z', 'missing', -1)
               + complete('2026-01-01T00:00:01Z', 'a')
               + start('2026-01-01T00:00:01Z', 'a')
               + start('2026-01-01T00:00:02Z', 'b', '[broken]')
               + start('bad-time', 'c')
               + record('2026-01-01T00:00:03Z', '', 'Command complete, outcome: "FAILED"')
               + start('2026-01-01T00:00:04Z', 'b'))
        result = analyze.analyze(log)
        self.assertEqual(result['peak_simultaneous_executing'], 1)
        self.assertEqual(result['incomplete_operation_ids'], ['b'])
        self.assertEqual(result['unmatched_completion_operation_ids'], ['missing'])
        self.assertEqual(result['nonzero_exits'], 1)
        self.assertEqual(result['diagnostics'], {
            'invalid_timestamps': 1, 'invalid_command_arguments': 1,
            'missing_or_ambiguous_operation_ids': 1, 'unknown_exit_codes': 1,
            'repeated_start_operation_ids': ['b'], 'repeated_completion_operation_ids': [],
        })

    def test_cli_matches_api(self):
        log = start('2026-11-05T20:45:41.883101Z', 'a') + complete('2026-11-05T20:45:42Z', 'a')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'worker.log'
            path.write_text(log, encoding='utf-8')
            output = subprocess.check_output([sys.executable, str(Path(analyze.__file__)), str(path)], text=True)
        self.assertEqual(json.loads(output), analyze.analyze(log))
        self.assertEqual(analyze.analyze('')['peak_simultaneous_executing'], 0)


if __name__ == '__main__':
    unittest.main()
