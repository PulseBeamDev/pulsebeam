import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

from tools.native_addon_licenses import inventory


def sha(data):
    return hashlib.sha256(data).hexdigest()


def archive(name, version, files):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode='w:gz') as tar:
        for path, data in sorted(files.items()):
            info = tarfile.TarInfo(f'{name}-{version}/{path}')
            info.size = len(data)
            info.mode = 0o644
            tar.addfile(info, io.BytesIO(data))
    return out.getvalue()


def cargo(name, version='1.0.0', extra=''):
    return f'[package]\nname = "{name}"\nversion = "{version}"\n{extra}'.encode()


class InventoryTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.root = self.base / 'root'
        self.root.mkdir()
        (self.root / 'Cargo.toml').write_bytes(cargo('uniffi-runtime-napi'))
        (self.root / 'LICENSE').write_bytes(b'root license\n')
        self.crate_files = {'Cargo.toml': cargo('libffi-sys'), 'LICENSE': b'crate license\n',
                            'libffi/LICENSE': b'vendored full license\n',
                            'libffi/NOTICE': b'vendored notice without newline'}
        self.crate = archive('libffi-sys', '1.0.0', self.crate_files)
        (self.base / 'libffi.crate').write_bytes(self.crate)
        self.config = {'sources': [
            {'name': 'uniffi-runtime-napi', 'version': '1.0.0', 'directory': 'root'},
            {'name': 'libffi-sys', 'version': '1.0.0', 'source': 'registry+https://example',
             'archive': 'libffi.crate'}]}
        self.lock = self.make_lock()

    def make_lock(self, dependency='libffi-sys', extra=''):
        return (f'version = 3\n[[package]]\nname = "uniffi-runtime-napi"\nversion = "1.0.0"\n'
                f'dependencies = ["{dependency}"]\n[[package]]\nname = "libffi-sys"\n'
                f'version = "1.0.0"\nsource = "registry+https://example"\n'
                f'checksum = "{sha(self.crate)}"\n{extra}').encode()

    def run_inventory(self):
        return inventory.inventory(self.lock, sha(self.lock), self.config, self.base)

    def test_closure_and_nested_texts_are_deterministic(self):
        manifest, texts = self.run_inventory()
        self.assertEqual((manifest, texts), self.run_inventory())
        self.assertEqual([p['name'] for p in manifest['packages']], ['libffi-sys', 'uniffi-runtime-napi'])
        nested = {t['path']: t['text'] for t in texts}
        self.assertEqual(nested['libffi/LICENSE'], 'vendored full license\n')
        self.assertEqual(nested['libffi/NOTICE'], 'vendored notice without newline')
        self.assertIn('not an attestation', manifest['provenance'])

    def test_conservative_transitive_lock_edges_and_unreachable(self):
        packages = [dict(name='uniffi-runtime-napi', version='1', dependencies=['build 1', 'windows 1']),
                    dict(name='build', version='1', dependencies=['proc 1']),
                    dict(name='windows', version='1'), dict(name='proc', version='1'),
                    dict(name='unreachable', version='1')]
        self.assertEqual([p['name'] for p in inventory.dependencies(packages, 'uniffi-runtime-napi')],
                         ['build', 'proc', 'uniffi-runtime-napi', 'windows'])

    def test_ambiguous_and_missing_references_fail(self):
        extra = '[[package]]\nname = "libffi-sys"\nversion = "2.0.0"\n'
        self.lock = self.make_lock(extra=extra)
        with self.assertRaisesRegex(inventory.InventoryError, 'ambiguous or missing'):
            self.run_inventory()
        self.lock = self.make_lock(dependency='missing 1.0.0')
        with self.assertRaisesRegex(inventory.InventoryError, 'ambiguous or missing'):
            self.run_inventory()
        self.lock = self.make_lock(dependency='libffi-sys 1.0.0 (registry+https://example)', extra=extra)
        self.run_inventory()

    def test_hash_checks_and_registry_directory_requirement(self):
        with self.assertRaisesRegex(inventory.InventoryError, 'Cargo.lock SHA-256 mismatch'):
            inventory.inventory(self.lock, '0' * 64, self.config, self.base)
        (self.base / 'libffi.crate').write_bytes(self.crate + b'x')
        with self.assertRaisesRegex(inventory.InventoryError, 'crate checksum mismatch'):
            self.run_inventory()
        self.config['sources'][1] = dict(name='libffi-sys', version='1.0.0',
                                         source='registry+https://example', directory='root')
        with self.assertRaisesRegex(inventory.InventoryError, 'requires checksum-verified archive'):
            self.run_inventory()

    def test_workspace_archive_requires_independent_hash_and_safe_prefix(self):
        data = archive('source', '1.0', {
            'runtimes/napi/Cargo.toml': cargo('uniffi-runtime-napi'),
            'runtimes/napi/LICENSE': b'root license\n',
            'other/ignored': b'unrelated',
        })
        (self.base / 'source.tar.gz').write_bytes(data)
        self.config['sources'][0] = {
            'name': 'uniffi-runtime-napi', 'version': '1.0.0',
            'workspace_archive': 'source.tar.gz', 'archive_sha256': sha(data),
            'prefix': 'source-1.0/runtimes/napi',
        }
        manifest, _ = self.run_inventory()
        root = next(p for p in manifest['packages'] if p['name'] == 'uniffi-runtime-napi')
        self.assertEqual(root['archive_sha256'], sha(data))
        self.config['sources'][0]['archive_sha256'] = '0' * 64
        with self.assertRaisesRegex(inventory.InventoryError, 'workspace archive checksum mismatch'):
            self.run_inventory()
        self.config['sources'][0]['archive_sha256'] = sha(data)
        self.config['sources'][0]['prefix'] = '../outside'
        with self.assertRaisesRegex(inventory.InventoryError, 'unsafe path'):
            self.run_inventory()

    def test_directory_must_match_archive(self):
        crate_dir = self.base / 'unpacked'
        for name, data in self.crate_files.items():
            path = crate_dir / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        self.config['sources'][1]['directory'] = 'unpacked'
        self.run_inventory()
        (crate_dir / 'LICENSE').write_text('changed')
        with self.assertRaisesRegex(inventory.InventoryError, 'does not match archive'):
            self.run_inventory()

    def test_missing_unreadable_and_supplemental_texts(self):
        (self.root / 'LICENSE').unlink()
        (self.root / 'NOTICE').write_text('not a license')
        with self.assertRaisesRegex(inventory.InventoryError, 'no license text'):
            self.run_inventory()
        (self.base / 'reviewed.txt').write_text('Reviewed full text')
        self.config['supplements'] = [{'name': 'uniffi-runtime-napi', 'version': '1.0.0',
                                       'files': ['reviewed.txt']}]
        manifest, texts = self.run_inventory()
        self.assertIn('Reviewed full text', [t['text'] for t in texts])
        self.config['supplements'][0]['version'] = '2.0.0'
        with self.assertRaisesRegex(inventory.InventoryError, 'no unique reachable'):
            self.run_inventory()
        self.config['supplements'][0]['version'] = '1.0.0'
        (self.base / 'reviewed.txt').chmod(0)
        with self.assertRaisesRegex(inventory.InventoryError, 'not a readable'):
            self.run_inventory()

    def test_declared_license_file_and_unsafe_inputs(self):
        (self.root / 'LICENSE').unlink()
        (self.root / 'custom.txt').write_text('complete text')
        (self.root / 'Cargo.toml').write_bytes(cargo('uniffi-runtime-napi', extra='license-file = "custom.txt"\n'))
        self.run_inventory()
        (self.root / 'custom.txt').unlink()
        with self.assertRaisesRegex(inventory.InventoryError, 'missing license-file'):
            self.run_inventory()
        with self.assertRaisesRegex(inventory.InventoryError, 'unsafe path'):
            inventory.archive_files(archive('x', '1', {'../outside': b'x'}), 'x-1')

    def test_nested_license_cannot_satisfy_crate_license(self):
        del self.crate_files['LICENSE']
        self.crate = archive('libffi-sys', '1.0.0', self.crate_files)
        (self.base / 'libffi.crate').write_bytes(self.crate)
        self.lock = self.make_lock()
        with self.assertRaisesRegex(inventory.InventoryError, 'no license text found'):
            self.run_inventory()
        (self.base / 'reviewed.txt').write_text('crate license')
        self.config['supplements'] = [{'name': 'libffi-sys', 'version': '1.0.0',
                                       'files': ['reviewed.txt']}]
        self.run_inventory()

    def test_inherited_or_external_license_requires_supplement(self):
        (self.root / 'LICENSE').unlink()
        for declaration in ('license-file = { workspace = true }', 'license-file = "../LICENSE"'):
            (self.root / 'Cargo.toml').write_bytes(cargo('uniffi-runtime-napi', extra=declaration + '\n'))
            with self.assertRaisesRegex(inventory.InventoryError, 'requires reviewed supplement'):
                self.run_inventory()
            (self.base / 'reviewed.txt').write_text('reviewed upstream text')
            self.config['supplements'] = [{'name': 'uniffi-runtime-napi', 'version': '1.0.0',
                                           'files': ['reviewed.txt']}]
            self.run_inventory()
            del self.config['supplements']

    def test_cli_outputs_and_failure_do_not_leave_files(self):
        lock = self.base / 'Cargo.lock'
        config = self.base / 'inputs.json'
        lock.write_bytes(self.lock)
        config.write_text(json.dumps(self.config))
        script = Path(inventory.__file__).resolve()
        manifest, notices = self.base / 'manifest.json', self.base / 'notices.json'
        cmd = [sys.executable, str(script), '--lock', str(lock), '--lock-sha256', sha(self.lock),
               '--inputs', str(config), '--manifest', str(manifest), '--notices', str(notices)]
        result = subprocess.run(cmd, capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(manifest.read_text()), self.run_inventory()[0])
        self.assertEqual(json.loads(notices.read_text()), self.run_inventory()[1])
        manifest.unlink()
        notices.unlink()
        (self.base / 'libffi.crate').write_bytes(b'wrong')
        self.assertNotEqual(subprocess.run(cmd, capture_output=True, check=False).returncode, 0)
        self.assertFalse(manifest.exists())
        self.assertFalse(notices.exists())


if __name__ == '__main__':
    unittest.main()
