"""Content-verified dependency bundles and ordinary OCI data images."""
import hashlib
import fcntl
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile
from urllib.parse import urlsplit

ACR = 'agent-eval-registry-vpc.ap-southeast-5.cr.aliyuncs.com/terminal-bench'


def file_digest(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def json_bytes(value):
    return json.dumps(value, sort_keys=True, separators=(',', ':')).encode()


def safe_name(name):
    path = PurePosixPath(name)
    if (not name or path.is_absolute() or '..' in path.parts or
            str(path) != name or '\\' in name or any(ord(c) < 32 for c in name)):
        raise ValueError('Invalid dependency archive path')
    return name


def task_digest(task_dir):
    root = Path(task_dir)
    files = {}
    for path in sorted(root.rglob('*')):
        if '__pycache__' in path.parts:
            continue
        if path.is_symlink():
            raise ValueError('Task source contains a symlink')
        if path.is_file():
            files[path.relative_to(root).as_posix()] = file_digest(path)
    if not files:
        raise ValueError('Task source is empty')
    return hashlib.sha256(json_bytes(files)).hexdigest()


def bind_tasks(tasks, images, prior, revision):
    """Bind the frozen eligible set without claiming unprepared downloads cached."""
    from scope import filter_microvm_tasks
    if prior.get('schema') != 1 or prior.get('harbor_revision') != revision:
        raise ValueError('Dependency binding schema or Harbor revision changed')
    names, excluded = filter_microvm_tasks(sorted(tasks))
    rows = {}
    for name in names:
        row = {'task_sha256': task_digest(tasks[name]),
               'image_digest': images[name]['source_platform_digest'], 'verifier': None}
        existing = prior.get('tasks', {}).get(name)
        if existing:
            if any(existing.get(key) != row[key] for key in ('task_sha256', 'image_digest')):
                raise ValueError('Existing dependency task source or image changed: ' + name)
            row['verifier'] = existing['verifier']
        row['verifier_cache_status'] = 'prepared' if row['verifier'] else 'not_prepared'
        rows[name] = row
    return {'schema': 1, 'harbor_revision': revision, 'tasks': rows, 'scope_excluded': excluded}


def validate_lab_dependencies(record, tasks, images, revision):
    if record.get('schema') != 1 or record.get('harbor_revision') != revision:
        raise ValueError('Dependency binding schema or Harbor revision changed')
    files = []
    for name, path in tasks.items():
        row = record.get('tasks', {}).get(name)
        if (not row or row.get('task_sha256') != task_digest(path) or
                row.get('image_digest') != images[name]['source_platform_digest']):
            raise ValueError('Dependency binding task source or image changed')
        binding = row.get('verifier')
        if binding:
            spec = binding['spec']
            if (spec.get('task_name') != name or spec.get('phase') != 'verifier' or
                    spec.get('task_sha256') != row['task_sha256'] or
                    spec.get('image_digest') != row['image_digest']):
                raise ValueError('Dependency verifier spec has an invalid task binding')
            bundle = verify_bundle(binding['bundle_root'], spec)
            if any(bundle[key] != binding[key] for key in ('manifest_sha256', 'bundle_sha256')):
                raise ValueError('Dependency bundle differs from the frozen binding')
            files.extend(str(Path(binding['bundle_root']) / name) for name in ('manifest.json', 'bundle.tar'))
    return files


def validate_spec(spec):
    if (not re.fullmatch(r'[a-z0-9][a-z0-9_-]*', spec.get('task_name', '')) or
            not re.fullmatch(r'[0-9a-f]{64}', spec.get('task_sha256', '')) or
            not re.fullmatch(r'sha256:[0-9a-f]{64}', spec.get('image_digest', '')) or
            spec.get('phase') not in ('public', 'verifier')):
        raise ValueError('Invalid dependency task binding')
    for name in ('max_unpacked_bytes', 'max_bundle_bytes'):
        if type(spec.get(name)) is not int or spec[name] <= 0:
            raise ValueError('Dependency size bounds must be positive integers')
    source = urlsplit(spec.get('source', ''))
    if source.scheme != 'https' or not source.hostname or source.username or source.password:
        raise ValueError('Dependency source must be credential-free HTTPS')


def build_bundle(spec, tree, output):
    validate_spec(spec)
    tree, output = Path(tree), Path(output)
    files, directories = {}, []
    for path in sorted(tree.rglob('*')):
        name = safe_name(path.relative_to(tree).as_posix())
        if path.is_symlink():
            raise ValueError('Dependency source links are not supported')
        if path.is_dir():
            directories.append(name)
        elif path.is_file():
            files[name] = {'sha256': file_digest(path), 'size': path.stat().st_size,
                           'mode': path.stat().st_mode & 0o777}
        else:
            raise ValueError('Dependency source special files are not supported')
    unpacked = sum(row['size'] for row in files.values())
    if not files or unpacked > spec['max_unpacked_bytes']:
        raise ValueError('Dependency source exceeds its size bound or is empty')
    output.mkdir()
    archive = output / 'bundle.tar'
    with tarfile.open(archive, 'w', format=tarfile.PAX_FORMAT) as stream:
        for name in directories:
            info = tarfile.TarInfo(name)
            info.type, info.mode = tarfile.DIRTYPE, 0o755
            stream.addfile(info)
        for name, row in files.items():
            info = tarfile.TarInfo(name)
            info.size, info.mode = row['size'], row['mode']
            with (tree / name).open('rb') as source:
                stream.addfile(info, source)
    if archive.stat().st_size > spec['max_bundle_bytes']:
        raise ValueError('Dependency archive exceeds its size bound')
    manifest = {'schema': 1, 'spec': spec, 'files': files, 'directories': directories,
                'unpacked_bytes': unpacked, 'bundle_bytes': archive.stat().st_size,
                'bundle_sha256': file_digest(archive)}
    (output / 'manifest.json').write_bytes(json_bytes(manifest))
    return verify_bundle(output, spec)


def verify_bundle(root, expected):
    root = Path(root)
    if root.is_symlink() or any((root / name).is_symlink() for name in ('manifest.json', 'bundle.tar')):
        raise ValueError('Dependency bundle paths must not be symlinks')
    manifest = json.loads((root / 'manifest.json').read_bytes())
    if manifest.get('schema') != 1:
        raise ValueError('Unsupported dependency manifest schema')
    spec = manifest['spec']
    validate_spec(spec)
    if any(spec.get(name) != value for name, value in expected.items()):
        raise ValueError('Dependency bundle does not match the requested task or phase')
    archive = root / 'bundle.tar'
    if (archive.stat().st_size != manifest['bundle_bytes'] or
            archive.stat().st_size > spec['max_bundle_bytes'] or
            file_digest(archive) != manifest['bundle_sha256']):
        raise ValueError('Dependency archive hash or size mismatch')
    files = manifest['files']
    directories = manifest['directories']
    if len(directories) != len(set(directories)):
        raise ValueError('Duplicate dependency directories')
    for name in [*files, *directories]:
        safe_name(name)
    seen, size = set(), 0
    with tarfile.open(archive, 'r:') as stream:
        for info in stream:
            name = safe_name(info.name)
            if name in seen or not (info.isfile() or info.isdir()):
                raise ValueError('Duplicate or unsupported dependency archive entry')
            seen.add(name)
            if info.isdir():
                if name not in directories:
                    raise ValueError('Unlisted dependency directory')
                continue
            row = files.get(name)
            if not row or info.size != row['size'] or info.mode != row['mode']:
                raise ValueError('Dependency entry metadata mismatch')
            size += info.size
            if size > spec['max_unpacked_bytes']:
                raise ValueError('Dependency unpacked size exceeds its bound')
            digest = hashlib.sha256()
            with stream.extractfile(info) as source:
                for block in iter(lambda: source.read(1024 * 1024), b''):
                    digest.update(block)
            if digest.hexdigest() != row['sha256']:
                raise ValueError('Dependency payload hash mismatch')
    if seen != set(files) | set(directories) or size != manifest['unpacked_bytes']:
        raise ValueError('Dependency archive file set or unpacked size mismatch')
    return {**manifest, 'root': str(root.resolve()),
            'manifest_sha256': file_digest(root / 'manifest.json')}


def build_oci(bundle, output):
    bundle, output = Path(bundle), Path(output)
    record = verify_bundle(bundle, {})
    blobs = output / 'blobs' / 'sha256'
    blobs.mkdir(parents=True)
    layer = output / 'layer.pending'
    with tarfile.open(layer, 'w', format=tarfile.PAX_FORMAT) as stream:
        for name in ('manifest.json', 'bundle.tar'):
            info = tarfile.TarInfo('harbor-deps/' + name)
            info.mode, info.size = 0o444, (bundle / name).stat().st_size
            with (bundle / name).open('rb') as source:
                stream.addfile(info, source)
    layer_hash = file_digest(layer)
    layer_size = layer.stat().st_size
    layer.rename(blobs / layer_hash)

    def blob(value):
        raw = json_bytes(value)
        digest = hashlib.sha256(raw).hexdigest()
        (blobs / digest).write_bytes(raw)
        return 'sha256:' + digest, len(raw)

    config_digest, config_size = blob({'architecture': 'amd64', 'os': 'linux', 'config': {},
        'rootfs': {'type': 'layers', 'diff_ids': ['sha256:' + layer_hash]}})
    media = 'application/vnd.oci.image.manifest.v1+json'
    digest, size = blob({'schemaVersion': 2, 'mediaType': media,
        'config': {'mediaType': 'application/vnd.oci.image.config.v1+json',
                   'digest': config_digest, 'size': config_size},
        'layers': [{'mediaType': 'application/vnd.oci.image.layer.v1.tar',
                    'digest': 'sha256:' + layer_hash, 'size': layer_size}]})
    (output / 'oci-layout').write_bytes(json_bytes({'imageLayoutVersion': '1.0.0'}))
    (output / 'index.json').write_bytes(json_bytes({'schemaVersion': 2, 'manifests': [{
        'mediaType': media, 'digest': digest, 'size': size,
        'annotations': {'org.opencontainers.image.ref.name': 'deps'},
        'platform': {'os': 'linux', 'architecture': 'amd64'}}]}))
    return {'image_digest': digest, 'manifest_sha256': record['manifest_sha256'],
            'layout': str(output.resolve()), 'source_ref': 'ocidir://' + str(output.resolve()) + ':deps'}


def _manifest(regctl, ref, run):
    result = run([str(regctl), 'manifest', 'get', ref, '--format', 'raw-body'],
                 capture_output=True, timeout=60)
    if not result.returncode:
        return result.stdout
    error = result.stderr.decode(errors='replace').lower()
    if not re.search(r'401|403|unauthorized|forbidden|timeout|dial tcp', error) and re.search(
            r'manifest_unknown|manifest unknown|name_unknown|name unknown', error):
        return None
    raise RuntimeError('Dependency registry manifest lookup failed; not an absent tag')


def publish_oci(regctl, image, repository, *, run=subprocess.run):
    if not re.fullmatch(re.escape(ACR) + r'/hb-deps-[a-z0-9_-]+', repository):
        raise ValueError('Dependency publication requires its own ACR repository')
    digest = image['image_digest']
    raw = (Path(image['layout']) / 'blobs/sha256' / digest.split(':')[1]).read_bytes()
    if 'sha256:' + hashlib.sha256(raw).hexdigest() != digest:
        raise ValueError('Local OCI manifest changed')
    tag = repository + ':bundle-' + image['manifest_sha256']
    with (Path(image['layout']).parent / '.publication.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        existing = _manifest(regctl, tag, run)
        if existing is not None:
            if 'sha256:' + hashlib.sha256(existing).hexdigest() != digest:
                raise ValueError('Existing ACR tag conflicts; refusing to overwrite')
            return {'image': repository + '@' + digest, 'tag': tag, 'published': False}
        result = run([str(regctl), 'image', 'copy', image['source_ref'], tag],
                     capture_output=True, timeout=1800)
        if result.returncode:
            raise RuntimeError('Dependency OCI publication failed')
        final = _manifest(regctl, tag, run)
        if final is None or 'sha256:' + hashlib.sha256(final).hexdigest() != digest:
            raise ValueError('Published OCI digest does not match the prepared image')
        return {'image': repository + '@' + digest, 'tag': tag, 'published': True}


def _blob(regctl, ref, descriptor, target, run):
    digest = descriptor['digest']
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', digest):
        raise ValueError('Invalid OCI blob digest')
    with target.open('wb') as stream:
        result = run([str(regctl), 'blob', 'get', ref.split('@')[0], digest],
                     stdout=stream, stderr=subprocess.PIPE, timeout=1800)
    if result.returncode:
        raise RuntimeError('Dependency OCI blob download failed')
    if target.stat().st_size != descriptor['size'] or 'sha256:' + file_digest(target) != digest:
        raise ValueError('Dependency OCI blob hash or size mismatch')


def verify_cached_oci(root, digest):
    """Retain registry proof and bind the current bundle bytes to the OCI layer."""
    root = Path(root)
    try:
        raw = (root / 'oci-manifest.json').read_bytes()
        config = (root / 'oci-config.json').read_bytes()
    except FileNotFoundError as error:
        raise ValueError('Dependency cache has no OCI proof; use a new cache directory') from error
    manifest = json.loads(raw)
    if ('sha256:' + hashlib.sha256(raw).hexdigest() != digest or
            manifest.get('mediaType') != 'application/vnd.oci.image.manifest.v1+json' or
            len(manifest.get('layers', [])) != 1):
        raise ValueError('Dependency cached OCI manifest mismatch')
    descriptor = manifest['config']
    if (len(config) != descriptor['size'] or
            'sha256:' + hashlib.sha256(config).hexdigest() != descriptor['digest']):
        raise ValueError('Dependency cached OCI config mismatch')
    platform = json.loads(config)
    if platform.get('architecture') != 'amd64' or platform.get('os') != 'linux':
        raise ValueError('Dependency cached OCI platform mismatch')

    class LayerHash:
        def __init__(self):
            self.digest, self.size = hashlib.sha256(), 0

        def write(self, block):
            self.digest.update(block)
            self.size += len(block)
            return len(block)

    layer = LayerHash()
    with tarfile.open(fileobj=layer, mode='w|', format=tarfile.PAX_FORMAT) as stream:
        for name in ('manifest.json', 'bundle.tar'):
            info = tarfile.TarInfo('harbor-deps/' + name)
            info.mode, info.size = 0o444, (root / name).stat().st_size
            with (root / name).open('rb') as source:
                stream.addfile(info, source)
    descriptor = manifest['layers'][0]
    if (descriptor['mediaType'] != 'application/vnd.oci.image.layer.v1.tar' or
            layer.size != descriptor['size'] or
            'sha256:' + layer.digest.hexdigest() != descriptor['digest']):
        raise ValueError('Dependency bundle differs from its cached OCI layer')


def load_image(regctl, image_ref, cache_root, expected, *, run=subprocess.run):
    if not re.fullmatch(re.escape(ACR) + r'/hb-deps-[a-z0-9_-]+@sha256:[0-9a-f]{64}', image_ref):
        raise ValueError('Dependency image must be an immutable ACR digest')
    validate_spec(expected)
    cache_root = Path(cache_root)
    cache_root.mkdir(parents=True, exist_ok=True)
    digest = image_ref.rsplit('@', 1)[1]
    root = cache_root / digest.replace(':', '-')
    locks = cache_root / '.locks'
    locks.mkdir(exist_ok=True)
    with (locks / digest.split(':')[1]).open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if root.exists():
            verify_cached_oci(root, digest)
            return {**verify_bundle(root, expected), 'image': image_ref, 'cache_hit': True}
        stage = Path(tempfile.mkdtemp(prefix='.staging-', dir=cache_root))
        try:
            raw = _manifest(regctl, image_ref, run)
            if raw is None or 'sha256:' + hashlib.sha256(raw).hexdigest() != digest:
                raise ValueError('Dependency OCI manifest hash mismatch or missing image')
            manifest = json.loads(raw)
            layers = manifest.get('layers', [])
            if (manifest.get('mediaType') != 'application/vnd.oci.image.manifest.v1+json' or
                    len(layers) != 1 or layers[0]['mediaType'] != 'application/vnd.oci.image.layer.v1.tar' or
                    layers[0]['size'] > expected['max_bundle_bytes'] + 16 * 1024**2 or
                    manifest['config']['size'] > 1024**2):
                raise ValueError('Unsupported or oversized dependency OCI layout')
            config_path, layer = stage / 'config.pending', stage / 'layer.pending'
            _blob(regctl, image_ref, manifest['config'], config_path, run)
            config = json.loads(config_path.read_bytes())
            if config.get('architecture') != 'amd64' or config.get('os') != 'linux':
                raise ValueError('Dependency OCI platform mismatch')
            _blob(regctl, image_ref, layers[0], layer, run)
            seen = set()
            with tarfile.open(layer, 'r:') as stream:
                for info in stream:
                    if (info.name not in ('harbor-deps/manifest.json', 'harbor-deps/bundle.tar') or
                            info.name in seen or not info.isfile() or
                            info.size > (1024**2 if info.name.endswith('.json') else expected['max_bundle_bytes'])):
                        raise ValueError('Unexpected or oversized dependency OCI layer member')
                    seen.add(info.name)
                    with stream.extractfile(info) as source, (stage / info.name.split('/')[-1]).open('wb') as target:
                        shutil.copyfileobj(source, target, 1024 * 1024)
            if seen != {'harbor-deps/manifest.json', 'harbor-deps/bundle.tar'}:
                raise ValueError('Incomplete dependency OCI payload')
            verify_bundle(stage, expected)
            (stage / 'oci-manifest.json').write_bytes(raw)
            config_path.rename(stage / 'oci-config.json')
            verify_cached_oci(stage, digest)
            layer.unlink()
            os.rename(stage, root)
            return {**verify_bundle(root, expected), 'image': image_ref, 'cache_hit': False}
        finally:
            if stage.exists():
                shutil.rmtree(stage)
