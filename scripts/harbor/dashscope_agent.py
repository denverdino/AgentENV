"""Terminus-2 with recursive credential redaction before model logs are written."""
import asyncio
import json
import os
import time

from harbor.agents.terminus_2.terminus_2 import Terminus2, Terminus2Options
from harbor.agents.computer_1.computer_1 import Computer1


def redact(value, secret):
    if isinstance(value, str):
        return value.replace(secret, '[REDACTED]') if secret else value
    if isinstance(value, dict):
        return {key: redact(item, secret) for key, item in value.items()}
    if isinstance(value, list):
        return [redact(item, secret) for item in value]
    return value


class DashScopeTerminus2Options(Terminus2Options):
    pause_taskvm_during_inference: bool = False


class DashScopeTerminus2(Terminus2):
    options_model = DashScopeTerminus2Options

    def __init__(self, *args, pause_taskvm_during_inference=False, **kwargs):
        self._pause_taskvm_during_inference = pause_taskvm_during_inference
        self._inference_environment = None
        self._inference_number = 0
        self._inference_lock = asyncio.Lock()
        super().__init__(*args, pause_taskvm_during_inference=pause_taskvm_during_inference, **kwargs)

    async def run(self, instruction, environment, context):
        self._inference_environment = environment
        try:
            return await super().run(instruction, environment, context)
        finally:
            self._inference_environment = None

    def _lifecycle_event(self, phase, started, error=None):
        row = {'inference': self._inference_number, 'phase': phase,
               'seconds': time.monotonic() - started,
               'error_type': type(error).__name__ if error else None}
        self.logger.info('TaskVM %s %s %.3fs', phase,
                         row['error_type'] or 'OK', row['seconds'])
        with (self.logs_dir / 'pause-resume.jsonl').open('a') as stream:
            stream.write(json.dumps(row) + '\n')

    async def _call_with_paused_taskvm(self, call, *args, **kwargs):
        environment = self._inference_environment
        if not self._pause_taskvm_during_inference or environment is None:
            return await call(*args, **kwargs)
        # All Chat and summarization paths use this agent's LLM.call. Serialize
        # them so one call cannot resume a VM while another is still inferring.
        async with self._inference_lock:
            self._inference_number += 1
            try:
                for phase, operation in (('pause', environment.pause),
                                         ('infer', lambda: call(*args, **kwargs))):
                    started = time.monotonic()
                    error = None
                    try:
                        result = await operation()
                    except BaseException as caught:
                        error = caught
                        raise
                    finally:
                        self._lifecycle_event(phase, started, error)
                return result
            finally:
                started = time.monotonic()
                error = None
                resume_task = asyncio.create_task(environment.resume())
                try:
                    try:
                        await asyncio.shield(resume_task)
                    except asyncio.CancelledError:
                        await resume_task
                        raise
                except BaseException as caught:
                    error = caught
                    raise
                finally:
                    self._lifecycle_event('resume', started, error)

    def _init_llm(self, *args, **kwargs):
        llm = super()._init_llm(*args, **kwargs)
        original = llm._init_logger_fn
        secret = os.environ['OPENAI_API_KEY']

        def safe_logger(path):
            callback = original(path)
            return lambda data: callback(redact(data, secret))

        llm._init_logger_fn = safe_logger
        call = llm.call

        async def paused_call(*args, **kwargs):
            return await self._call_with_paused_taskvm(call, *args, **kwargs)

        llm.call = paused_call
        return llm


class DashScopeComputer1(Computer1):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        original = self._llm._init_logger_fn
        secret = os.environ['OPENAI_API_KEY']

        def safe_logger(path):
            callback = original(path)
            return lambda data: callback(redact(data, secret))

        self._llm._init_logger_fn = safe_logger
