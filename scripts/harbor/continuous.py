"""Continuous trial slots; a slot is reusable only after independent cleanup proof."""
from concurrent.futures import ThreadPoolExecutor, wait, FIRST_COMPLETED
import threading
import time


def run_continuous(trials, *, concurrency, prepare, execute, verify_cleanup, capacity, record, cancelled):
    if not 1 <= concurrency <= 8:
        raise ValueError('Continuous concurrency must be between one and eight')
    if len({str(t['id']) for t in trials}) != len(trials):
        raise ValueError('Trial IDs must be unique')
    pending = list(trials); active = {}; stop = threading.Event(); disk_wait_states = {}
    report = {'completed': [], 'interrupted': [], 'cleanup_errors': [], 'pending': [], 'reason': 'completed'}
    def event(kind, trial=None, **extra):
        record({'event': kind, 'time': time.time(), 'active_slots': len(active),
                **({'id': trial['id'], 'task': trial.get('task')} if trial else {}), **extra})
    def worker(trial):
        row = {'id': trial['id'], 'task': trial.get('task')}
        def check_stage():
            state = capacity.snapshot()
            reason = ('low_watermark' if state['low_watermark'] else
                      'cancelled' if cancelled() else None)
            if reason:
                stop.set(); row['stop_reason'] = reason
            return stop.is_set()
        try:
            if check_stage(): row['interrupted'] = True
            else:
                prepared = prepare(trial)
                if check_stage(): row['interrupted'] = True
                else:
                    row['result'] = execute(trial, prepared, stop)
                    row['interrupted'] = stop.is_set() or bool((row.get('result') or {}).get('interruption'))
        except BaseException as error:
            row['error'] = type(error).__name__ + ': ' + str(error)
            row['interrupted'] = stop.is_set()
        finally:
            try:
                if verify_cleanup(trial) is not True:
                    raise RuntimeError('Cleanup was not positively verified')
                row['cleanup'] = 'passed'
            except BaseException as error:
                row['cleanup'] = type(error).__name__ + ': ' + str(error)
        return row
    def admit(trial):
        # Coordinator is the sole admission writer. Shared inputs remain leased
        # for the lifetime of this Lab, even after their first consumer finishes.
        added = []
        for key, size in trial.get('shared', {}).items():
            existed = key in capacity.snapshot()['reservations']
            if not capacity.admit(key, size):
                for item in added: capacity.release(item)
                return False
            if not existed: added.append(key)
        key = 'trial:' + str(trial['id'])
        if not capacity.admit(key, trial['disk_bytes']):
            for item in added: capacity.release(item)
            return False
        return True
    with ThreadPoolExecutor(max_workers=concurrency) as pool:
        while pending or active:
            state = capacity.snapshot()
            if not stop.is_set() and (cancelled() or state['low_watermark']):
                report['reason'] = 'low_watermark' if state['low_watermark'] else 'cancelled'
                stop.set(); event('stopping', reason=report['reason'], capacity=state)
            while pending and len(active) < concurrency and not stop.is_set():
                selected = None
                for index, candidate in enumerate(pending):
                    if admit(candidate):
                        selected = index; break
                    state = capacity.snapshot()
                    signature = (state['free_bytes'] // 1024**3, state['future_bytes'] // 1024**3, len(active))
                    if disk_wait_states.get(str(candidate['id'])) != signature:
                        event('disk_wait', candidate, capacity=state)
                        disk_wait_states[str(candidate['id'])] = signature
                if selected is None: break
                trial = pending.pop(selected)
                future = pool.submit(worker, trial); active[future] = trial
                event('admitted', trial, capacity=capacity.snapshot())
            if not active:
                if pending and not stop.is_set(): report['reason'] = 'blocked_disk'
                break
            done, _ = wait(active, timeout=.05, return_when=FIRST_COMPLETED)
            for future in done:
                trial = active[future]; row = future.result()
                if row.get('stop_reason'):
                    report['reason'] = row['stop_reason']; stop.set()
                    event('stopping', trial, reason=row['stop_reason'], capacity=capacity.snapshot())
                if row['cleanup'] == 'passed':
                    capacity.release('trial:' + str(trial['id']))
                    event('cleaned', trial)
                else:
                    report['cleanup_errors'].append(row)
                    report['reason'] = 'cleanup_error'; stop.set()
                    event('cleanup_error', trial, error=row['cleanup'])
                report['interrupted' if row.get('interrupted') else 'completed'].append(row)
                del active[future]
                event('finished', trial, interrupted=row.get('interrupted', False))
    report['pending'] = pending
    report['capacity'] = capacity.snapshot()
    return report
