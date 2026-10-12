"""Reviewed exclusions for the frozen 89-task Terminal-Bench 2.0 revision.

Eligibility is a workload scope decision, not an Oracle qualification result.
Linux graphics and user-space instruction interpreters remain eligible.
"""

EXCLUDED = {
    'install-windows-3.11': 'Requires a Windows 3.11 guest running in QEMU',
    'qemu-alpine-ssh': 'Requires booting a complete Alpine VM inside QEMU',
    'qemu-startup': 'Requires booting a complete Alpine VM inside QEMU',
}


def filter_microvm_tasks(names):
    return (tuple(name for name in names if name not in EXCLUDED),
            [{'task': name, 'reason': EXCLUDED[name], 'evidence': name + '/instruction.md'}
             for name in names if name in EXCLUDED])
