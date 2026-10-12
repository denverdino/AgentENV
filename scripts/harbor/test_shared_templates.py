import io
import json
from pathlib import Path
import unittest


class ObjectStore:
    """Only replaces external S3; cache serialization and selection remain real."""
    def __init__(self):
        self.objects = {}
    def put_object(self, *, Bucket, Key, Body, ContentType):
        self.objects[Key] = bytes(Body)
    def get_object(self, *, Bucket, Key):
        return {'Body': io.BytesIO(self.objects[Key])}
    def head_object(self, *, Bucket, Key):
        return {'ContentLength': len(self.objects[Key])}
    def get_paginator(self, name):
        assert name == 'list_objects_v2'
        return self
    def paginate(self, *, Bucket, Prefix):
        # Exercise multiple pages and reverse listing order.
        for key in sorted(self.objects, reverse=True):
            if key.startswith(Prefix):yield {'Contents': [{'Key': key}]}


class SharedTemplateTests(unittest.TestCase):
    def store(self):
        self.assertTrue(Path(__file__).with_name('shared_templates.py').exists(),
                        'Persistent shared template lookup/publication is missing')
        from shared_templates import SharedTemplateStore
        self.client = ObjectStore()
        self.repository = {'endpoint': 'https://oss.example.test', 'region': 'test-1',
                           'bucket': 'test-bucket', 'prefix': 'harbor/shared-templates-v1'}
        return SharedTemplateStore(self.client, self.repository)

    def binding(self, snapshot='01a123ba-af7d-7042-8623-4a7bd7181fbb'):
        from prepare_templates import launch_key
        value = {'scope': 'shared', 'repository': self.repository,
            'task_name': 'openssl-selfsigned-cert', 'task_sha256': 'c' * 64,
            'image_digest': 'sha256:' + 'a' * 64,
            'resources': {'cpus': 4, 'memory_mb': 8192}, 'assets': {'runner': 'b' * 64},
            'snapshot_id': snapshot, 'source_sandbox_id': 'builder-source',
            'workspace_sha256': 'f' * 64, 'run_id': 'producer-run'}
        value['key'] = launch_key(value)
        return value

    def committed(self, binding):
        prefix = self.repository['prefix']
        self.client.objects[prefix + '/catalog/records/' + binding['snapshot_id'] + '.json'] = b'{}'

    def test_sdk_uses_validated_oss_upload_mode_and_rejects_another_repository(self):
        import os
        import tempfile
        from shared_templates import open_store
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / '.env'
            path.write_text('HARBOR_OSS_ENDPOINT=https://oss.example.test\n'
                'HARBOR_OSS_REGION=test-1\nHARBOR_OSS_BUCKET=test-bucket\n'
                'HARBOR_OSS_PREFIX=harbor\nHARBOR_OSS_ACCESS_KEY_ID=fixture-id\n'
                'HARBOR_OSS_ACCESS_KEY_SECRET=fixture-secret\n')
            path.chmod(0o600)
            repository = {'endpoint': 'https://oss.example.test', 'region': 'test-1',
                          'bucket': 'test-bucket', 'prefix': 'harbor/shared-templates-v1'}
            store = open_store(path, repository)
            try:
                self.assertEqual(store.client.meta.config.request_checksum_calculation, 'when_required')
                self.assertEqual(store.client.meta.config.response_checksum_validation, 'when_required')
                with self.assertRaises(ValueError):open_store(path, {**repository, 'prefix': 'other'})
            finally:
                store.client.close()

    def test_miss_and_cross_host_lookup_return_the_same_binding(self):
        store = self.store();binding = self.binding()
        self.assertIsNone(store.lookup(binding['key']))
        self.committed(binding);store.publish(binding)
        from shared_templates import SharedTemplateStore
        peer = SharedTemplateStore(self.client, dict(self.repository))
        self.assertEqual(peer.lookup(binding['key']), binding)

    def test_publish_requires_committed_snapshot_and_does_not_publish_failure(self):
        store = self.store();binding = self.binding()
        with self.assertRaises((ValueError, KeyError)):store.publish(binding)
        self.assertIsNone(store.lookup(binding['key']))

    def test_concurrent_publications_have_independent_keys(self):
        store = self.store();first = self.binding();second = self.binding('01a123bb-53e1-73e3-8052-4c439da4f4ba')
        for value in (first, second):self.committed(value);store.publish(value)
        records = [key for key in self.client.objects if '/harbor-template-index/' in key]
        self.assertEqual(len(records), 2)
        self.assertEqual(store.lookup(first['key']), first)
        self.assertEqual(store.lookup(second['key']), first)

    def test_corrupt_binding_or_repository_is_rejected_before_launch(self):
        store = self.store();binding = self.binding();self.committed(binding);store.publish(binding)
        key = next(key for key in self.client.objects if '/harbor-template-index/' in key)
        for change in ({'key': 'a' * 64}, {'repository': {**self.repository, 'prefix': 'wrong'}},
                       {'snapshot_id': '../escape'}, {'workspace_sha256': ''}):
            with self.subTest(change=change):
                self.client.objects[key] = json.dumps({'schema': 1, 'binding': {**binding, **change}}).encode()
                with self.assertRaises(ValueError):store.lookup(binding['key'])

    def test_deleted_snapshot_is_not_silently_rebuilt(self):
        store = self.store();binding = self.binding();self.committed(binding);store.publish(binding)
        del self.client.objects[self.repository['prefix'] + '/catalog/records/' + binding['snapshot_id'] + '.json']
        with self.assertRaises((ValueError, KeyError)):store.lookup(binding['key'])


if __name__ == '__main__':
    unittest.main()
