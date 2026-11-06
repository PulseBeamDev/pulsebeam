#!/usr/bin/env python3
"""Offline, conservative Cargo.lock source notice inventory (Python 3.12+)."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import sys
import tarfile
import tomllib


class InventoryError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise InventoryError(message)


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_file(path):
    path = Path(path)
    mode = path.stat().st_mode
    require(stat.S_ISREG(mode) and mode & 0o444, f"not a readable regular file: {path}")
    return path.read_bytes()


def safe_name(name):
    require(isinstance(name, str) and name and '\\' not in name, f"invalid path: {name!r}")
    p = PurePosixPath(name)
    require(not p.is_absolute() and all(x not in ('', '.', '..') for x in name.split('/')),
            f"unsafe path: {name!r}")
    return name


def directory_files(path):
    path = Path(path)
    require(path.is_dir() and not path.is_symlink(), f"not a source directory: {path}")
    result = {}
    def visit(directory, prefix=''):
        mode = directory.stat().st_mode
        require(mode & 0o444 and mode & 0o111, f"unreadable directory: {directory}")
        for item in sorted(directory.iterdir(), key=lambda p: p.name):
            name = safe_name(prefix + item.name)
            mode = item.lstat().st_mode
            if stat.S_ISDIR(mode):
                visit(item, name + '/')
            else:
                require(stat.S_ISREG(mode), f"non-regular source entry: {item}")
                result[name] = read_file(item)
    visit(path)
    return result


def archive_files(data, expected_root):
    files = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:*') as tar:
        seen = set()
        for member in tar:
            name = member.name.rstrip('/')
            safe_name(name)
            require(name not in seen, f"duplicate archive member: {name}")
            seen.add(name)
            parts = name.split('/')
            require(parts[0] == expected_root, f"unexpected archive root: {name}")
            if member.isdir():
                continue
            require(len(parts) > 1 and member.isfile() and member.mode & 0o444,
                    f"unsupported or unreadable archive member: {name}")
            stream = tar.extractfile(member)
            require(stream is not None, f"cannot read archive member: {name}")
            files['/'.join(parts[1:])] = stream.read()
    return files


def workspace_archive_files(data, prefix):
    safe_name(prefix)
    files = {}
    seen = set()
    with tarfile.open(fileobj=io.BytesIO(data), mode='r:*') as tar:
        for member in tar:
            name = member.name.rstrip('/')
            safe_name(name)
            require(name not in seen, f"duplicate archive member: {name}")
            seen.add(name)
            if not name.startswith(prefix + '/'):
                continue
            relative = name[len(prefix) + 1:]
            if member.isdir():
                continue
            require(member.isfile() and member.mode & 0o444,
                    f"unsupported or unreadable workspace member: {name}")
            stream = tar.extractfile(member)
            require(stream is not None, f"cannot read workspace member: {name}")
            files[relative] = stream.read()
    require(files, f"empty workspace archive prefix: {prefix}")
    return files


def key(package):
    return (package['name'], package['version'], package.get('source'))


def dependencies(packages, root):
    by_name = {}
    keys = set()
    for p in packages:
        require(isinstance(p.get('name'), str) and isinstance(p.get('version'), str),
                'invalid lock package identity')
        require(key(p) not in keys, f"duplicate lock package: {key(p)}")
        keys.add(key(p))
        by_name.setdefault(p['name'], []).append(p)
    roots = by_name.get(root, [])
    require(len(roots) == 1, f"expected exactly one {root} root, found {len(roots)}")
    pending = [roots[0]]
    reached = {}
    while pending:
        p = pending.pop()
        if key(p) in reached:
            continue
        reached[key(p)] = p
        refs = p.get('dependencies', [])
        require(isinstance(refs, list), f"invalid dependencies: {key(p)}")
        for ref in refs:
            require(isinstance(ref, str), f"invalid dependency reference: {ref!r}")
            match = re.fullmatch(r'([^ ()]+)(?: ([^ ()]+))?(?: \(([^()]+)\))?', ref)
            require(match is not None, f"invalid dependency reference: {ref!r}")
            name, version, source = match.groups()
            candidates = [q for q in by_name.get(name, [])
                          if (version is None or q['version'] == version)
                          and (source is None or q.get('source') == source)]
            require(len(candidates) == 1, f"ambiguous or missing dependency {ref!r} from {key(p)}")
            pending.append(candidates[0])
    return sorted(reached.values(), key=lambda p: (p['name'], p['version'], p.get('source') or ''))


NOTICE = re.compile(r'^(?:licen[cs]e|copying|copyright|notice|unlicense)(?:$|[._ -].*)', re.I)


def is_notice(path):
    parts = path.split('/')
    return bool(NOTICE.match(parts[-1])) or any(p.lower() == 'licenses' for p in parts[:-1])


def text_bytes(data, label):
    require(bool(data.strip()), f"empty notice: {label}")
    data.decode('utf-8')
    require(b'\x00' not in data, f"NUL in notice: {label}")
    return data.decode('utf-8')


def inventory(lock_bytes, expected_hash, config, base):
    require(re.fullmatch(r'[0-9a-f]{64}', expected_hash or '') is not None,
            'lock hash must be lowercase SHA-256')
    require(digest(lock_bytes) == expected_hash, 'Cargo.lock SHA-256 mismatch')
    lock = tomllib.loads(lock_bytes.decode('utf-8'))
    require(lock.get('version') in (3, 4), 'only Cargo.lock version 3 or 4 supported')
    require(isinstance(lock.get('package'), list), 'missing lock packages')
    packages = dependencies(lock['package'], 'uniffi-runtime-napi')
    specs = {}
    for spec in config.get('sources', []):
        k = (spec['name'], spec['version'], spec.get('source'))
        require(k not in specs, f"duplicate source declaration: {k}")
        specs[k] = spec
    supplements = {}
    for spec in config.get('supplements', []):
        k = (spec['name'], spec['version'])
        require(k not in supplements, f"duplicate supplemental declaration: {k}")
        require(isinstance(spec.get('files'), list) and spec['files'], f"empty supplements: {k}")
        supplements[k] = spec['files']
    # A supplement must not silently apply to multiple source identities.
    for k in supplements:
        require(sum((p['name'], p['version']) == k for p in packages) == 1,
                f"supplement has no unique reachable crate: {k}")
    require(set(specs) == {key(p) for p in packages}, 'sources must exactly match reachable lock packages')
    entries, bundle = [], []
    for p in packages:
        k = key(p)
        spec = specs[k]
        source = p.get('source')
        archive = spec.get('archive')
        directory = spec.get('directory')
        workspace_archive = spec.get('workspace_archive')
        require(archive or directory or workspace_archive, f"no source input: {k}")
        require(not workspace_archive or (source is None and not archive and not directory),
                f"workspace archive requires a source-less package and no other source: {k}")
        files = None
        archive_hash = None
        if workspace_archive:
            data = read_file(base / workspace_archive)
            archive_hash = digest(data)
            require(re.fullmatch(r'[0-9a-f]{64}', spec.get('archive_sha256', '')) is not None
                    and archive_hash == spec['archive_sha256'], f"workspace archive checksum mismatch: {k}")
            files = workspace_archive_files(data, spec['prefix'])
        if archive:
            data = read_file(base / archive)
            archive_hash = digest(data)
            require(re.fullmatch(r'[0-9a-f]{64}', p.get('checksum', '')) is not None,
                    f"archive requires lock checksum: {k}")
            require(archive_hash == p['checksum'], f"crate checksum mismatch: {k}")
            files = archive_files(data, f"{p['name']}-{p['version']}")
        elif source and (source.startswith('registry+') or source.startswith('sparse+')):
            raise InventoryError(f"registry source requires checksum-verified archive: {k}")
        if directory:
            local = directory_files(base / directory)
            if files is not None:
                require(local == files, f"directory does not match archive: {k}")
            files = local
        require(files is not None and 'Cargo.toml' in files, f"missing Cargo.toml: {k}")
        manifest = tomllib.loads(files['Cargo.toml'].decode('utf-8'))
        meta = manifest.get('package', {})
        require(meta.get('name') == p['name'], f"manifest name mismatch: {k}")
        version = meta.get('version')
        # Workspace-inherited versions require independently declared source identity.
        require(version == p['version'] or (source is None and version == {'workspace': True}),
                f"manifest version mismatch: {k}")
        names = {n for n in files if is_notice(n)}
        license_file = meta.get('license-file')
        local_license_file = False
        if license_file is not None:
            if license_file == {'workspace': True} or (
                    isinstance(license_file, str) and license_file.startswith('../')):
                require((p['name'], p['version']) in supplements,
                        f"external/inherited license-file requires reviewed supplement: {k}")
            else:
                require(isinstance(license_file, str), f"invalid license-file: {k}")
                safe_name(license_file)
                require(license_file in files, f"missing license-file: {k}: {license_file}")
                names.add(license_file)
                local_license_file = True
        notices = []
        for name in sorted(names):
            content = text_bytes(files[name], f"{k}: {name}")
            notices.append({'kind': 'source', 'path': name, 'sha256': digest(files[name])})
            bundle.append((k, 'source', name, content))
        for index, path in enumerate(supplements.get((p['name'], p['version']), [])):
            data = read_file(base / path)
            content = text_bytes(data, f"{k}: supplement {index}")
            label = f'supplement-{index + 1}'
            notices.append({'kind': 'supplement', 'path': label, 'sha256': digest(data)})
            bundle.append((k, 'supplement', label, content))
        has_license = local_license_file or any(
            ('/' not in n and re.match(r'^(?:licen[cs]e|copying|unlicense)(?:$|[._ -].*)', n, re.I))
            or n.lower().startswith('licenses/') for n in names)
        require(has_license or (p['name'], p['version']) in supplements,
                f"no license text found for {k}; supply reviewed supplement")
        tree = hashlib.sha256()
        for name, data in sorted(files.items()):
            tree.update(name.encode('utf-8') + b'\0' + digest(data).encode('ascii') + b'\n')
        entries.append({'name': p['name'], 'version': p['version'], 'source': source,
                        'lock_checksum': p.get('checksum'), 'archive_sha256': archive_hash,
                        'tree_sha256': tree.hexdigest(), 'license': meta.get('license'),
                        'notices': notices})
    manifest = {'schema': 1, 'lock_sha256': expected_hash, 'root': 'uniffi-runtime-napi',
                'provenance': 'Source inventory only; not an attestation of binary provenance.',
                'packages': entries}
    # JSON preserves each text exactly, including missing terminal newlines.
    texts = [{'name': k[0], 'version': k[1], 'source': k[2], 'kind': kind,
              'path': name, 'text': content} for k, kind, name, content in bundle]
    return manifest, texts


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--lock', required=True)
    parser.add_argument('--lock-sha256', required=True)
    parser.add_argument('--inputs', required=True, help='JSON source and supplemental declarations')
    parser.add_argument('--base', help='resolve input paths relative to this directory (default: inputs JSON parent)')
    parser.add_argument('--manifest', required=True)
    parser.add_argument('--notices', required=True)
    args = parser.parse_args()
    try:
        require(os.path.abspath(args.manifest) != os.path.abspath(args.notices), 'output paths must differ')
        require(not Path(args.manifest).exists() and not Path(args.notices).exists(),
                'output paths must not already exist')
        config_path = Path(args.inputs)
        config = json.loads(read_file(config_path).decode('utf-8'))
        manifest, texts = inventory(read_file(args.lock), args.lock_sha256, config,
                                    Path(args.base) if args.base else config_path.parent)
        outputs = [json.dumps(x, sort_keys=True, indent=2, ensure_ascii=False) + '\n'
                   for x in (manifest, texts)]
        created = []
        try:
            for path, output in zip((args.manifest, args.notices), outputs):
                with open(path, 'x', encoding='utf-8', newline='\n') as stream:
                    created.append(path)
                    stream.write(output)
        except OSError:
            for path in created:
                os.unlink(path)
            raise
    except (InventoryError, OSError, ValueError, KeyError, TypeError, tarfile.TarError) as e:
        print(f'inventory: {e}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
