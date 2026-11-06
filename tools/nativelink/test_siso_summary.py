import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import siso_summary


A = '00000000-0000-4000-8000-000000000001'
B = '00000000-0000-4000-8000-000000000002'
C = '00000000-0000-4000-8000-000000000003'
D = '00000000-0000-4000-8000-000000000004'
SHA = 'ab' * 32


def digest(uuid, sha=SHA):
    return f'I1105 12:00:00.000 remoteexec.go:96] {uuid} digest: {sha}/123, command\n'


def metadata(uuid, queue):
    return (f'I1105 12:00:00.001 remoteexec.go:164] {uuid} execution metadata: '
            f'queue={queue} worker=1s input=2ms exec=3s output=4ms\n')


def completion(uuid, exit_code=0, cache='false'):
    return f'I1105 12:00:00.002 remoteexec.go:177] {uuid} exit={exit_code} cache={cache}\n'


class SisoSummaryTest(unittest.TestCase):
    def test_interleaved_completed_and_incomplete(self):
        log = (digest(A) + digest(B) + metadata(B, '1.5ms') + metadata(A, '2s')
               + completion(A, cache='true') + completion(B, 2)
               + digest(C) + metadata(D, '3us')
               + 'cmd cache miss: no caches to retrieve\n')
        result = siso_summary.summarize(io.StringIO(log))
        self.assertEqual(result['completed'], {
            'total': 2, 'backend_cache_hits': 1, 'backend_cache_misses': 1,
            'nonzero_exits': 1, 'with_metadata': 2, 'without_metadata': 0,
            'with_digest': 2, 'without_digest': 0,
        })
        self.assertEqual(result['queue_duration'], {
            'count': 2, 'total_ns': 2001500000, 'min_ns': 1500000,
            'max_ns': 2000000000, 'mean_ns': 1000750000,
        })
        self.assertEqual(result['incomplete_without_completion'], {
            'uuids': 2, 'with_metadata': 1, 'with_digest': 1,
        })
        self.assertTrue(all(count == 0 for count in result['diagnostics'].values()))

    def test_malformed_duplicates_and_out_of_order(self):
        log = (digest(A, 'invalid') + metadata(A, 'bad') + metadata(A, '1s')
               + digest(A) + digest(A) + completion(A, cache='maybe')
               + completion(A, -1, 'false') + completion(B, cache='true')
               + metadata(B, '1s') + 'remoteexec.go:164] not-a-uuid execution metadata: queue=1s\n'
               + f'remoteexec.go:164] {C} not metadata\n'
               + f'remoteexec.go:177] {C} exit=0 cache=truegarbage\n')
        result = siso_summary.summarize(io.StringIO(log))
        self.assertEqual(result['completed'], {
            'total': 2, 'backend_cache_hits': 1, 'backend_cache_misses': 1,
            'nonzero_exits': 1, 'with_metadata': 1, 'without_metadata': 1,
            'with_digest': 1, 'without_digest': 1,
        })
        self.assertEqual(result['diagnostics'], {
            'malformed_digest_records': 1, 'malformed_metadata_records': 2,
            'malformed_completion_records': 2, 'invalid_queue_durations': 1,
            'duplicate_digest_records': 1, 'duplicate_metadata_records': 1,
        })
        self.assertEqual(result['queue_duration']['count'], 0)
        self.assertIsNone(result['queue_duration']['mean_ns'])
        self.assertEqual(result['incomplete_without_completion'], {
            'uuids': 1, 'with_metadata': 1, 'with_digest': 0,
        })

    def test_go_durations(self):
        for text, expected in [('0', 0), ('1h2m3.004005006s', 3723004005006),
                               ('1µs', 1000), ('1μs', 1000), ('.5ms', 500000),
                               ('1ns', 1), ('-1s', None), ('NaN', None),
                               ('1.5ns', None), ('1sX', None), ('', None),
                               ('9223372036854775808ns', None)]:
            with self.subTest(text=text):
                self.assertEqual(siso_summary._duration_ns(text), expected)
        self.assertIsNone(siso_summary._duration_ns('9' * 1000001 + 's'))

    def test_streams_many_records_without_reading_or_retaining_completed(self):
        class Lines:
            def __iter__(self):
                for i in range(20000):
                    uuid = f'00000000-0000-4000-8000-{i:012x}'
                    yield digest(uuid)
                    yield metadata(uuid, '1us')
                    yield completion(uuid)
        result = siso_summary.summarize(Lines())
        self.assertEqual(result['completed']['backend_cache_misses'], 20000)
        self.assertEqual(result['queue_duration']['total_ns'], 20000000)
        self.assertEqual(result['incomplete_without_completion']['uuids'], 0)

    def test_cli_file_and_stdin(self):
        log = digest(A) + metadata(A, '0') + completion(A, cache='true')
        expected = siso_summary.summarize(io.StringIO(log))
        script = str(Path(siso_summary.__file__))
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'synthetic.log'
            path.write_text(log, encoding='utf-8')
            output = subprocess.check_output([sys.executable, script, str(path)], text=True)
        self.assertEqual(json.loads(output), expected)
        output = subprocess.check_output([sys.executable, script, '-'], input=log, text=True)
        self.assertEqual(json.loads(output), expected)


if __name__ == '__main__':
    unittest.main()
