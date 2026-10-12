"""Reserve future allocations; materialized data is already reflected in free space."""
import threading

GIB = 1024**3


def task_disk_budget(declared, *, memory_mb, template_mode, archive_bytes, cached_bytes):
    if template_mode not in ('reuse', 'prepare', 'cold'):
        raise ValueError('Unknown template disk budget mode')
    if any(type(v) is not int or v < 0 for v in (memory_mb, archive_bytes, cached_bytes)):
        raise ValueError('Disk budget sizes must be nonnegative integers')
    storage = (declared.get('storage_mb') or 10240) * 1024**2
    materialization = memory_mb * 1024**2 if template_mode != 'cold' else 0
    builder = storage if template_mode == 'prepare' else 0
    return {'declared_storage_bytes': storage, 'virtual_bytes': 64 * GIB,
            'storage_limit_enforced': False, 'runtime_write_bytes': storage,
            'builder_bytes': builder, 'materialization_bytes': max(0, materialization - cached_bytes),
            'import_bytes': archive_bytes,
            'future_bytes': storage + builder + max(0, materialization - cached_bytes) + archive_bytes}


class DiskBudget:
    def __init__(self, reserve_bytes, free_bytes):
        if type(reserve_bytes) is not int or reserve_bytes < 0:
            raise ValueError('Disk safety reserve must be nonnegative')
        self.reserve_bytes = reserve_bytes
        self.free_bytes = free_bytes
        self.leases = {}
        self.lock = threading.RLock()

    def admit(self, key, future_bytes):
        if not key or type(future_bytes) is not int or future_bytes < 0:
            raise ValueError('Invalid disk reservation')
        with self.lock:
            if key in self.leases:
                if self.leases[key]['total'] != future_bytes:
                    raise ValueError('Conflicting reservation for the same input')
                return not self.snapshot()['low_watermark']
            if self.snapshot()['headroom_bytes'] < future_bytes:
                return False
            self.leases[key] = {'total': future_bytes, 'materialized': 0}
            return True

    def materialized(self, key, bytes):
        with self.lock:
            row = self.leases.get(key)
            if row is None or type(bytes) is not int or bytes < row['materialized']:
                raise ValueError('Missing reservation or invalid cumulative materialization')
            row['materialized'] = bytes

    def release(self, key):
        with self.lock:
            self.leases.pop(key, None)

    def snapshot(self):
        with self.lock:
            free = self.free_bytes()
            future = sum(max(0, r['total'] - r['materialized']) for r in self.leases.values())
            return {'free_bytes': free, 'reserve_bytes': self.reserve_bytes,
                    'future_bytes': future, 'headroom_bytes': free - self.reserve_bytes - future,
                    'low_watermark': free < self.reserve_bytes,
                    'reservations': {k: dict(v) for k, v in self.leases.items()}}
