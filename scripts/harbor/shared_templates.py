"""Immutable template index records in the Harbor-only OSS repository."""
import json
import os
import re

from dependencies import json_bytes

REPOSITORY_FIELDS = ('endpoint', 'region', 'bucket', 'prefix')


class SharedTemplateStore:
    def __init__(self, client, repository):
        self.client = client
        self.repository = {name: repository[name] for name in REPOSITORY_FIELDS}
        self.prefix = self.repository['prefix'].rstrip('/')
        self.bucket = self.repository['bucket']

    def _validate(self, binding, key):
        from prepare_templates import launch_key
        if (binding.get('scope') != 'shared' or binding.get('repository') != self.repository or
                binding.get('key') != key or not re.fullmatch(r'[0-9a-f]{64}', key) or
                binding.get('key') != launch_key(binding) or
                not re.fullmatch(r'[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}', binding.get('snapshot_id', '')) or
                not re.fullmatch(r'[0-9a-f]{64}', binding.get('workspace_sha256', '')) or
                not binding.get('source_sandbox_id')):
            raise ValueError('Invalid shared template binding')

    def _committed(self, binding):
        # The API returns capture only after repository commit. Recheck the
        # durable record so stale/deleted publications cannot become cache hits.
        self.client.head_object(Bucket=self.bucket, Key=self.prefix +
            '/catalog/records/' + binding['snapshot_id'] + '.json')

    def lookup(self, key):
        if not re.fullmatch(r'[0-9a-f]{64}', key):
            raise ValueError('Invalid shared template key')
        prefix = self.prefix + '/harbor-template-index/' + key + '/'
        objects = [row['Key'] for page in self.client.get_paginator('list_objects_v2').paginate(
            Bucket=self.bucket, Prefix=prefix) for row in page.get('Contents', [])]
        if not objects:
            return None
        # Multiple publishers never overwrite one another. Pick the same
        # completed candidate on every reader, irrespective of listing order.
        path = min(objects)
        body = self.client.get_object(Bucket=self.bucket, Key=path)['Body']
        try:
            raw = body.read(65537)
        finally:
            body.close()
        if len(raw) > 65536:
            raise ValueError('Shared template record exceeds metadata budget')
        record = json.loads(raw)
        if record.get('schema') != 1 or not isinstance(record.get('binding'), dict):
            raise ValueError('Invalid shared template index schema')
        binding = record['binding']
        self._validate(binding, key)
        if path != prefix + binding['snapshot_id'] + '.json':
            raise ValueError('Shared template index identity mismatch')
        self._committed(binding)
        return binding

    def publish(self, binding):
        self._validate(binding, binding['key'])
        self._committed(binding)
        # No aliases, mutable current pointer or unsupported conditional PUT.
        # The snapshot ID names this publication; retries write identical bytes.
        self.client.put_object(Bucket=self.bucket, Key=self.prefix + '/harbor-template-index/' +
            binding['key'] + '/' + binding['snapshot_id'] + '.json',
            Body=json_bytes({'schema': 1, 'binding': binding}), ContentType='application/json')


def open_store(env_path, repository):
    import boto3
    from botocore.config import Config
    from oss_env import read_env, snapshot_config
    values = read_env(env_path)
    expected = snapshot_config(env_path, 'shared-templates-v1', uid=os.getuid())
    if repository != {name: expected[name] for name in REPOSITORY_FIELDS}:
        raise ValueError('Shared template repository differs from private OSS configuration')
    client = boto3.client('s3', endpoint_url=values['ENDPOINT'], region_name=values['REGION'],
        aws_access_key_id=values['ACCESS_KEY_ID'], aws_secret_access_key=values['ACCESS_KEY_SECRET'],
        aws_session_token=values.get('SECURITY_TOKEN') or None,
        config=Config(signature_version='s3v4', s3={'addressing_style': 'virtual'},
                      connect_timeout=10, read_timeout=30, retries={'total_max_attempts': 1},
                      request_checksum_calculation='when_required', response_checksum_validation='when_required'))
    return SharedTemplateStore(client, repository)
