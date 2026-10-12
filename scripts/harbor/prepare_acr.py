#!/usr/bin/env python3
"""Prepare canonical Terminal-Bench 2.0 tasks from mirrored ACR digests."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

from prepare_tasks import HARBOR_REVISION, verify_source, verify_tree, task_environment_name
from resources import add_resource_arguments, plan_resources, select_tasks, archive_budget_bytes
from scope import filter_microvm_tasks

REVISION = '69671fbaac6d67a7ef0dfec016cc38a64ef7a77c'


def verify_delivery(root):
    from harbor.environments.definition import environment_content_hash
    source = json.loads((root / 'task-source-manifest.json').read_text())
    images = json.loads((root / 'images.json').read_text())
    names = {path.split('/')[0] for path in source['files']}
    if source['revision'] != REVISION or len(names) != 89 or names != set(images):
        raise ValueError('Expected the exact canonical 89-task source and image set')
    verify_tree(root / 'tasks', source['files'])
    for name, image in images.items():
        task = root / 'tasks' / name
        if (image['environment_id'] != environment_content_hash(task / 'environment') or
            image['environment_name'] != task_environment_name(task) or
            image['platform'] != 'linux/amd64' or
            image['source_platform_digest'] != image['source_digest'] or
            not image['source_digest'].startswith('sha256:') or
            image['image'] != image['destination_tag'].rsplit(':', 1)[0] + '@' + image['source_digest']):
            raise ValueError('Invalid frozen image binding: ' + name)
    return source, images


def verify_cached_archive(record, image):
    archive = Path(record['archive'])
    if (not archive.is_absolute() or archive.is_symlink() or not archive.is_file() or
            archive.name.endswith('.part') or archive.stat().st_size != record['bytes']):
        raise ValueError('Cached archive must be a complete regular file with the frozen size')
    with archive.open('rb') as stream:
        digest = hashlib.file_digest(stream, 'sha256').hexdigest()
    if digest != record['archive_sha256']:
        raise ValueError('Cached archive SHA256 mismatch')
    with tarfile.open(archive) as contents:
        members = contents.getmembers()
        names = [entry.name for entry in members]
        if len(set(names)) != len(names) or any(not (entry.isfile() or entry.isdir()) for entry in members):
            raise ValueError('Cached OCI archive has duplicate or non-regular entries')
        def read(name):
            with contents.extractfile(name) as stream:return stream.read()
        index = json.loads(read('index.json'))
        if image['source_platform_digest'] not in [entry['digest'] for entry in index['manifests']]:
            raise ValueError('Cached archive belongs to another image')
        for entry in members:
            if entry.isfile() and entry.name.startswith('blobs/sha256/'):
                with contents.extractfile(entry) as stream:
                    if hashlib.file_digest(stream, 'sha256').hexdigest() != entry.name.removeprefix('blobs/sha256/'):
                        raise ValueError('Cached OCI blob digest mismatch')
        manifest = json.loads(read('blobs/sha256/' + image['source_platform_digest'].split(':')[1]))
        if manifest['config']['digest'] != image['config_digest']:
            raise ValueError('Cached OCI config differs from the original task image')
        for descriptor in [manifest['config'], *manifest['layers']]:
            if (not descriptor['digest'].startswith('sha256:') or
                    not contents.getmember('blobs/sha256/' + descriptor['digest'].split(':')[1]).isfile() or
                    contents.getmember('blobs/sha256/' + descriptor['digest'].split(':')[1]).size != descriptor['size']):
                raise ValueError('Cached OCI descriptor mismatch')
    return digest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--delivery', type=Path, required=True)
    selection = parser.add_mutually_exclusive_group(required=True)
    selection.add_argument('--task', action='append')
    selection.add_argument('--all-supported', action='store_true',
                           help='Select all reviewed MicroVM-eligible tasks in the frozen suite')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--harbor-source', type=Path, required=True)
    parser.add_argument('--source-manifest', type=Path, required=True)
    archives = parser.add_mutually_exclusive_group(required=True)
    archives.add_argument('--regctl', type=Path)
    archives.add_argument('--cached-archives', type=Path, help='Explicit per-host archive, SHA256 and byte-count map')
    add_resource_arguments(parser)
    args = parser.parse_args()
    if os.geteuid() == 0:
        parser.error('Image preparation must run as the non-root test account')
    verify_source(args.harbor_source, args.source_manifest)
    _, images = verify_delivery(args.delivery)
    from harbor.models.task.task import Task
    try:
        requested = select_tasks(sorted(images) if args.all_supported else args.task, images)
        names, excluded = filter_microvm_tasks(requested)
        plan = plan_resources(args, {name: Task(args.delivery / 'tasks' / name).config.environment.model_dump(mode='json')
                                    for name in names})
    except ValueError as error:
        parser.error(str(error))
    cached = json.loads(args.cached_archives.read_text()) if args.cached_archives else {}
    if args.cached_archives:
        for name in names:
            verify_cached_archive(cached[name], images[name])
    archive_budgets = {}
    for name in names if not args.cached_archives else ():
        raw = subprocess.check_output([str(args.regctl), 'manifest', 'get', images[name]['image'],
                                       '--format', 'raw-body'], timeout=60)
        archive_budgets[name] = archive_budget_bytes(raw, images[name]['source_platform_digest'])
    remaining_archive_bytes = sum(archive_budgets.values())
    required_preparation_bytes = plan['required_disk_bytes'] + remaining_archive_bytes
    if shutil.disk_usage(args.output.parent).free < required_preparation_bytes:
        parser.error(f"Disk admission refused before image export: need {required_preparation_bytes} bytes including archives")
    args.output.mkdir()
    (args.output / 'tasks').mkdir()
    exports = {}
    for name in names:
        if shutil.disk_usage(args.output).free < plan['required_disk_bytes'] + remaining_archive_bytes:
            parser.error('Disk admission refused before next image export')
        task = args.output / 'tasks' / name
        shutil.copytree(args.delivery / 'tasks' / name, task)
        image = images[name]
        if args.cached_archives:
            archive = Path(cached[name]['archive'])
            digest = verify_cached_archive(cached[name], image)
        else:
            archive = args.output / (name + '.tar')
            subprocess.run([str(args.regctl), 'image', 'export', '--platform', 'linux/amd64',
                            image['image'], str(archive)], check=True, timeout=1800)
            with archive.open('rb') as stream:
                digest = hashlib.file_digest(stream, 'sha256').hexdigest()
            remaining_archive_bytes -= archive_budgets[name]
        exports[name] = {**image, 'archive': str(archive.resolve()), 'archive_sha256': digest}
    if shutil.disk_usage(args.output).free < plan['required_disk_bytes']:
        parser.error('Image export consumed the reserved runtime disk headroom')
    prepared = {'harbor_revision': HARBOR_REVISION, 'task_names': list(names),
        'source_root': str(args.harbor_source.resolve()),
        'source_manifest': str(args.source_manifest.resolve()),
        'benchmark': {'revision': REVISION}, 'acr_delivery': str(args.delivery.resolve()),
        'uid': os.geteuid(), 'resource_plan': plan, 'scope_excluded': excluded, 'images': exports,
        'archive_budget_bytes': sum(archive_budgets.values()),
        'required_preparation_disk_bytes': required_preparation_bytes}
    (args.output / 'prepared.json').write_text(json.dumps(prepared, indent=2))


if __name__ == '__main__':
    main()
