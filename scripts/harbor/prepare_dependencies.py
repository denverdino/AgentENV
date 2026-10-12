#!/usr/bin/env python3
"""Publish the reviewed OCaml verifier mirror as an immutable ACR data image."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time

from dependencies import ACR, build_bundle, build_oci, load_image, publish_oci, task_digest, bind_tasks, validate_lab_dependencies
from prepare_acr import verify_delivery
from prepare_tasks import HARBOR_REVISION

SOURCE = 'https://github.com/sadiqj/ocaml/'
TAG = 'tag_purposefully_broken_sweeping_changes'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--delivery', type=Path, required=True)
    parser.add_argument('--mirror', type=Path,
                        help='Completed full bare mirror of the original OCaml repository')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cache', type=Path, required=True)
    parser.add_argument('--regctl', type=Path, required=True)
    parser.add_argument('--reuse-binding', type=Path, help='Reuse verified immutable dependencies without publishing')
    parser.add_argument('--all-supported', action='store_true', help='Bind all frozen MicroVM-eligible tasks')
    args = parser.parse_args()
    if os.geteuid() == 0:
        parser.error('Dependency preparation must use the non-root test account')
    source, images = verify_delivery(args.delivery)
    started = time.time()
    if args.reuse_binding:
        prior = json.loads(args.reuse_binding.read_text())
        if not args.all_supported:
            parser.error('--reuse-binding requires --all-supported')
        for row in prior['tasks'].values():
            if row['verifier']:
                binding = row['verifier']
                cached = load_image(args.regctl, binding['image'], args.cache, binding['spec'])
                binding.update(bundle_root=cached['root'], cache_root=str(args.cache.resolve()))
        record = bind_tasks({name: args.delivery / 'tasks' / name for name in images},
                            images, prior, HARBOR_REVISION)
        validate_lab_dependencies(record, {name: args.delivery / 'tasks' / name for name in record['tasks']},
                                  images, HARBOR_REVISION)
        record.update(started=started, finished=time.time(), publication={'published': False})
        args.output.mkdir()
        (args.output / 'dependencies.json').write_text(json.dumps(record, indent=2))
        print(json.dumps({'task_count': len(record['tasks']), 'published': False,
                          'verifier_cache_count': sum(bool(r['verifier']) for r in record['tasks'].values())}))
        return
    if not args.mirror:
        parser.error('--mirror or --reuse-binding is required')

    def git(*arguments):
        return subprocess.check_output(['git', '-C', str(args.mirror), *arguments],
                                       timeout=300, stderr=subprocess.PIPE).decode().strip()

    if (args.mirror.is_symlink() or git('rev-parse', '--is-bare-repository') != 'true' or
            git('config', '--get', 'remote.origin.url').rstrip('/') != SOURCE.rstrip('/')):
        parser.error('Mirror does not match the reviewed OCaml source')
    git('fsck', '--full')
    commit = git('rev-parse', TAG + '^{commit}')
    spec = {'task_name': 'fix-ocaml-gc', 'task_sha256': task_digest(args.delivery / 'tasks/fix-ocaml-gc'),
        'image_digest': images['fix-ocaml-gc']['source_platform_digest'], 'phase': 'verifier',
        'source': SOURCE, 'commit': commit, 'tag': TAG, 'harbor_revision': HARBOR_REVISION,
        'benchmark_revision': source['revision'], 'recipe_version': 1,
        'git_version': git('--version'), 'platform': 'linux/amd64',
        'max_unpacked_bytes': 2 * 1024**3, 'max_bundle_bytes': 2 * 1024**3,
        'testsuite_tree': git('rev-parse', commit + ':testsuite')}
    args.output.mkdir()
    bundle = build_bundle(spec, args.mirror, args.output / 'bundle')
    image = build_oci(args.output / 'bundle', args.output / 'image')
    published = publish_oci(args.regctl, image, ACR + '/hb-deps-fix-ocaml-gc-verifier')
    cache = load_image(args.regctl, published['image'], args.cache, spec)
    tasks = {name: {'task_sha256': task_digest(args.delivery / 'tasks' / name),
                    'image_digest': images[name]['source_platform_digest'], 'verifier': None}
             for name in ('openssl-selfsigned-cert', 'fix-ocaml-gc')}
    tasks['fix-ocaml-gc']['verifier'] = {'image': published['image'], 'spec': spec,
        'manifest_sha256': bundle['manifest_sha256'], 'bundle_sha256': bundle['bundle_sha256'],
        'cache_root': str(args.cache.resolve()), 'bundle_root': cache['root']}
    if args.all_supported:
        tasks = bind_tasks({name: args.delivery / 'tasks' / name for name in images}, images,
                           {'schema': 1, 'harbor_revision': HARBOR_REVISION, 'tasks': tasks}, HARBOR_REVISION)['tasks']
    record = {'schema': 1, 'harbor_revision': HARBOR_REVISION, 'tasks': tasks,
        'publication': published, 'cache_hit': cache['cache_hit'],
        'started': started, 'finished': time.time()}
    (args.output / 'dependencies.json').write_text(json.dumps(record, indent=2))
    print(json.dumps({'image': published['image'], 'published': published['published'],
                      'commit': commit, 'bundle_bytes': bundle['bundle_bytes']}))


if __name__ == '__main__':
    main()
