import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
from types import SimpleNamespace
import unittest


class BundleTests(unittest.TestCase):
    def module(self):
        self.assertTrue(Path(__file__).with_name('dependencies.py').exists(),
                        'Dependency integrity module is missing')
        import dependencies
        return dependencies

    def setup_bundle(self, root):
        tree = root / 'tree'
        tree.mkdir()
        (tree / 'objects').mkdir()
        (tree / 'objects' / 'pack').write_bytes(b'frozen git objects')
        (tree / 'HEAD').write_text('ref: refs/heads/main\n')
        (tree / 'empty').mkdir()
        spec = {'task_name': 'fix-ocaml-gc', 'task_sha256': 'a' * 64,
                'image_digest': 'sha256:' + 'b' * 64, 'phase': 'verifier',
                'source': 'https://github.com/sadiqj/ocaml/', 'commit': 'c' * 40,
                'max_unpacked_bytes': 1024, 'max_bundle_bytes': 1024 * 1024}
        module = self.module()
        bundle = root / 'bundle'
        module.build_bundle(spec, tree, bundle)
        return module, tree, spec, bundle

    def test_valid_bundle_has_exact_binding_files_and_empty_directories(self):
        with tempfile.TemporaryDirectory() as directory:
            module, tree, spec, bundle = self.setup_bundle(Path(directory))
            record = module.verify_bundle(bundle, spec)
            self.assertEqual(record['spec'], spec)
            self.assertEqual(set(record['files']), {'HEAD', 'objects/pack'})
            self.assertIn('empty', record['directories'])
            self.assertEqual(record['bundle_sha256'], module.file_digest(bundle / 'bundle.tar'))

    def test_all_task_binding_keeps_reviewed_cache_and_labels_uncached_tasks(self):
        module = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            tasks = {}
            for name in ('fix-ocaml-gc', 'prove-plus-comm', 'install-windows-3.11'):
                tasks[name] = root / name
                tasks[name].mkdir()
                (tasks[name] / 'task.toml').write_text(name)
            images = {name: {'source_platform_digest': 'sha256:' + 'b' * 64} for name in tasks}
            prior = {'schema': 1, 'harbor_revision': 'frozen', 'tasks': {'fix-ocaml-gc': {
                'task_sha256': module.task_digest(tasks['fix-ocaml-gc']),
                'image_digest': 'sha256:' + 'b' * 64, 'verifier': {'marker': 'existing'}}}}
            result = module.bind_tasks(tasks, images, prior, 'frozen')
            self.assertEqual(set(result['tasks']), {'fix-ocaml-gc', 'prove-plus-comm'})
            self.assertEqual(result['tasks']['fix-ocaml-gc']['verifier'], {'marker': 'existing'})
            self.assertIsNone(result['tasks']['prove-plus-comm']['verifier'])
            self.assertEqual(result['tasks']['prove-plus-comm']['verifier_cache_status'], 'not_prepared')
            (tasks['fix-ocaml-gc'] / 'task.toml').write_text('changed')
            with self.assertRaises(ValueError):
                module.bind_tasks(tasks, images, prior, 'frozen')

    def test_changed_task_or_phase_is_rejected_even_with_same_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            module, tree, spec, bundle = self.setup_bundle(Path(directory))
            for changed in ({'task_sha256': 'd' * 64}, {'phase': 'public'},
                            {'task_name': 'another-task'}, {'image_digest': 'sha256:' + 'e' * 64}):
                with self.subTest(changed=changed), self.assertRaises(ValueError):
                    module.verify_bundle(bundle, {**spec, **changed})

    def test_corrupt_archive_and_payload_manifest_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            module, tree, spec, bundle = self.setup_bundle(Path(directory))
            archive = bundle / 'bundle.tar'
            original = archive.read_bytes()
            archive.write_bytes(original[:-1])
            with self.assertRaises(ValueError):
                module.verify_bundle(bundle, spec)
            archive.write_bytes(original)
            p = bundle / 'manifest.json'
            manifest = json.loads(p.read_text())
            manifest['files']['HEAD']['sha256'] = '0' * 64
            p.write_text(json.dumps(manifest))
            with self.assertRaises(ValueError):
                module.verify_bundle(bundle, spec)

    def test_hash_valid_path_escape_and_links_are_rejected(self):
        for name, kind in [('../escape', 'file'), ('/escape', 'file'),
                           ('HEAD', 'symlink'), ('HEAD', 'hardlink'), ('HEAD', 'duplicate')]:
            with self.subTest(name=name, kind=kind), tempfile.TemporaryDirectory() as directory:
                module, tree, spec, bundle = self.setup_bundle(Path(directory))
                with tarfile.open(bundle / 'bundle.tar', 'w') as stream:
                    for _ in range(2 if kind == 'duplicate' else 1):
                        info = tarfile.TarInfo(name)
                        if kind in ('symlink', 'hardlink'):
                            info.type = tarfile.SYMTYPE if kind == 'symlink' else tarfile.LNKTYPE
                            info.linkname = '/etc/passwd'
                            stream.addfile(info)
                        else:
                            info.size = 1
                            stream.addfile(info, io.BytesIO(b'x'))
                p = bundle / 'manifest.json'
                manifest = json.loads(p.read_text())
                manifest['bundle_sha256'] = module.file_digest(bundle / 'bundle.tar')
                manifest['bundle_bytes'] = (bundle / 'bundle.tar').stat().st_size
                p.write_text(json.dumps(manifest))
                with self.assertRaises(ValueError):
                    module.verify_bundle(bundle, spec)

    def test_preparation_rejects_symlink_and_size_overflow(self):
        with tempfile.TemporaryDirectory() as directory:
            module, tree, spec, bundle = self.setup_bundle(Path(directory))
            (tree / 'alias').symlink_to('HEAD')
            with self.assertRaises(ValueError):
                module.build_bundle(spec, tree, Path(directory) / 'linked')
            (tree / 'alias').unlink()
            with self.assertRaises(ValueError):
                module.build_bundle({**spec, 'max_unpacked_bytes': 1}, tree, Path(directory) / 'oversize')

    def test_oci_layout_is_deterministic_and_bound_to_layer_and_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, tree, spec, bundle = self.setup_bundle(root)
            first = module.build_oci(bundle, root / 'image-a')
            second = module.build_oci(bundle, root / 'image-b')
            self.assertEqual(first['image_digest'], second['image_digest'])
            digest = first['image_digest'].split(':')[1]
            manifest = json.loads((root / 'image-a' / 'blobs' / 'sha256' / digest).read_text())
            self.assertEqual(len(manifest['layers']), 1)
            for descriptor in [manifest['config'], *manifest['layers']]:
                p = root / 'image-a' / 'blobs' / 'sha256' / descriptor['digest'].split(':')[1]
                self.assertEqual(descriptor['size'], p.stat().st_size)
                self.assertEqual(descriptor['digest'], 'sha256:' + module.file_digest(p))
            layer = root / 'image-a' / 'blobs' / 'sha256' / manifest['layers'][0]['digest'].split(':')[1]
            with tarfile.open(layer) as stream:
                self.assertEqual(stream.getnames(), ['harbor-deps/manifest.json', 'harbor-deps/bundle.tar'])

    def test_task_identity_includes_verifier_changes(self):
        module = self.module()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'tests').mkdir()
            (root / 'tests/test.sh').write_text('first')
            before = module.task_digest(root)
            (root / 'tests/test.sh').write_text('second')
            self.assertNotEqual(before, module.task_digest(root))


class RegistryTests(unittest.TestCase):
    module = BundleTests.module
    setup_bundle = BundleTests.setup_bundle
    def image(self, root):
        module, tree, spec, bundle = self.setup_bundle(root)
        image = module.build_oci(bundle, root / 'layout')
        return module, spec, image

    def test_existing_tag_is_read_only_and_collision_or_auth_error_never_pushes(self):
        with tempfile.TemporaryDirectory() as directory:
            module, spec, image = self.image(Path(directory))
            raw = (Path(image['layout']) / 'blobs/sha256' / image['image_digest'].split(':')[1]).read_bytes()
            for data, error, fails in [(raw, b'', False), (b'other', b'', True),
                                       (b'', b'HTTP 401 unauthorized', True),
                                       (b'', b'failed to get token: Not Found [http 404]', True),
                                       (b'', b'proxy request failed: HTTP 404 Not Found', True),
                                       (b'', b'dial tcp: host not found', True)]:
                calls = []

                def runner(args, **kwargs):
                    calls.append(args)
                    return SimpleNamespace(returncode=1 if error else 0, stdout=data, stderr=error)

                with self.subTest(error=error, data=data[:10]):
                    if fails:
                        with self.assertRaises((ValueError, RuntimeError)):
                            module.publish_oci('regctl', image, module.ACR + '/hb-deps-fix-ocaml-gc-verifier', run=runner)
                    else:
                        result = module.publish_oci('regctl', image, module.ACR + '/hb-deps-fix-ocaml-gc-verifier', run=runner)
                        self.assertFalse(result['published'])
                        self.assertTrue(result['image'].endswith('@' + image['image_digest']))
                    self.assertTrue(all(args[1:3] == ['manifest', 'get'] for args in calls))

    def test_new_tag_publishes_only_in_dependency_repository(self):
        with tempfile.TemporaryDirectory() as directory:
            module, spec, image = self.image(Path(directory))
            raw = (Path(image['layout']) / 'blobs/sha256' / image['image_digest'].split(':')[1]).read_bytes()
            calls = []

            def runner(args, **kwargs):
                calls.append(args)
                if args[1:3] == ['manifest', 'get']:
                    written = any(c[1:3] == ['image', 'copy'] for c in calls[:-1])
                    return SimpleNamespace(returncode=0 if written else 1, stdout=raw if written else b'',
                                           stderr=b'' if written else b'HTTP 404 manifest unknown')
                return SimpleNamespace(returncode=0, stdout=b'', stderr=b'')

            repository = module.ACR + '/hb-deps-fix-ocaml-gc-verifier'
            result = module.publish_oci('regctl', image, repository, run=runner)
            self.assertTrue(result['published'])
            writes = [args for args in calls if args[1:3] == ['image', 'copy']]
            self.assertEqual(len(writes), 1)
            self.assertTrue(writes[0][-1].startswith(repository + ':bundle-'))
            with self.assertRaises(ValueError):
                module.publish_oci('regctl', image, module.ACR + '/fix-ocaml-gc', run=runner)

    def test_acr_cache_verifies_blobs_and_reuses_without_registry_calls(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, spec, image = self.image(root)
            calls = []

            def runner(args, **kwargs):
                calls.append(args)
                digest = image['image_digest'] if args[1:3] == ['manifest', 'get'] else args[-1]
                raw = (Path(image['layout']) / 'blobs/sha256' / digest.split(':')[1]).read_bytes()
                if 'stdout' in kwargs:
                    kwargs['stdout'].write(raw)
                    raw = b''
                return SimpleNamespace(returncode=0, stdout=raw, stderr=b'')

            ref = module.ACR + '/hb-deps-fix-ocaml-gc-verifier@' + image['image_digest']
            first = module.load_image('regctl', ref, root / 'cache', spec, run=runner)
            self.assertFalse(first['cache_hit'])
            self.assertEqual(first['spec'], spec)
            before = len(calls)
            second = module.load_image('regctl', ref, root / 'cache', spec, run=runner)
            self.assertTrue(second['cache_hit'])
            self.assertEqual(len(calls), before)
            (Path(second['root']) / 'bundle.tar').write_bytes(b'corrupt')
            with self.assertRaises(ValueError):
                module.load_image('regctl', ref, root / 'cache', spec, run=runner)
            self.assertEqual(len(calls), before, 'Corruption must not silently trigger a replacement')

    def test_valid_replacement_bundle_cannot_impersonate_cached_oci_digest(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module, spec, image = self.image(root)

            def runner(args, **kwargs):
                digest = image['image_digest'] if args[1:3] == ['manifest', 'get'] else args[-1]
                raw = (Path(image['layout']) / 'blobs/sha256' / digest.split(':')[1]).read_bytes()
                if 'stdout' in kwargs:
                    kwargs['stdout'].write(raw)
                    raw = b''
                return SimpleNamespace(returncode=0, stdout=raw, stderr=b'')

            ref = module.ACR + '/hb-deps-fix-ocaml-gc-verifier@' + image['image_digest']
            cached = module.load_image('regctl', ref, root / 'cache', spec, run=runner)
            (root / 'tree' / 'HEAD').write_text('another valid mirror')
            module.build_bundle(spec, root / 'tree', root / 'replacement')
            import shutil
            for name in ('manifest.json', 'bundle.tar'):
                shutil.copyfile(root / 'replacement' / name, Path(cached['root']) / name)
            with self.assertRaises(ValueError):
                module.load_image('regctl', ref, root / 'cache', spec, run=runner)

    def test_bad_oci_digest_or_unpinned_ref_is_rejected_before_publication(self):
        with tempfile.TemporaryDirectory() as directory:
            module, spec, image = self.image(Path(directory))

            def runner(args, **kwargs):
                return SimpleNamespace(returncode=0, stdout=b'{}', stderr=b'')

            for ref in (module.ACR + '/hb-deps-fix-ocaml-gc-verifier:latest',
                        module.ACR + '/hb-deps-fix-ocaml-gc-verifier@' + image['image_digest']):
                with self.subTest(ref=ref), self.assertRaises(ValueError):
                    module.load_image('regctl', ref, Path(directory) / 'cache', spec, run=runner)


class LabBindingTests(unittest.TestCase):
    def test_source_image_revision_and_bundle_bound_before_lab(self):
        import dependencies as module
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            task = root / 'task'
            task.mkdir()
            (task / 'test.sh').write_text('original verifier')
            spec = {'task_name': 'fix-ocaml-gc', 'task_sha256': 'a' * 64,
                'image_digest': 'sha256:' + 'b' * 64, 'phase': 'verifier',
                'source': 'https://github.com/sadiqj/ocaml/', 'commit': 'c' * 40,
                'max_unpacked_bytes': 1024, 'max_bundle_bytes': 1024 * 1024}
            spec.update(task_name='fix-ocaml-gc', task_sha256=module.task_digest(task))
            tree = root / 'tree'
            tree.mkdir()
            (tree / 'config').write_text('mirror')
            bundle = module.build_bundle(spec, tree, root / 'bundle')
            row = {'task_sha256': spec['task_sha256'], 'image_digest': spec['image_digest'],
                'verifier': {'spec': spec, 'bundle_root': str(root / 'bundle'),
                    'manifest_sha256': bundle['manifest_sha256'], 'bundle_sha256': bundle['bundle_sha256']}}
            record = {'schema': 1, 'harbor_revision': 'pinned', 'tasks': {'fix-ocaml-gc': row}}
            images = {'fix-ocaml-gc': {'source_platform_digest': spec['image_digest']}}
            inputs = module.validate_lab_dependencies(record, {'fix-ocaml-gc': task}, images, 'pinned')
            self.assertEqual(len(inputs), 2)
            for field, value in (('harbor_revision', 'stale'), ('schema', 2)):
                changed = {**record, field: value}
                with self.assertRaises(ValueError):
                    module.validate_lab_dependencies(changed, {'fix-ocaml-gc': task}, images, 'pinned')
            (task / 'test.sh').write_text('modified verifier')
            with self.assertRaises(ValueError):
                module.validate_lab_dependencies(record, {'fix-ocaml-gc': task}, images, 'pinned')


if __name__ == '__main__':
    unittest.main()
