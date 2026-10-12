"""Terminus-2 with recursive credential redaction before model logs are written."""
import os

from harbor.agents.terminus_2.terminus_2 import Terminus2
from harbor.agents.computer_1.computer_1 import Computer1


def redact(value, secret):
    if isinstance(value, str):
        return value.replace(secret, '[REDACTED]') if secret else value
    if isinstance(value, dict):
        return {key: redact(item, secret) for key, item in value.items()}
    if isinstance(value, list):
        return [redact(item, secret) for item in value]
    return value


class DashScopeTerminus2(Terminus2):
    def _init_llm(self, *args, **kwargs):
        llm = super()._init_llm(*args, **kwargs)
        original = llm._init_logger_fn
        secret = os.environ['OPENAI_API_KEY']

        def safe_logger(path):
            callback = original(path)
            return lambda data: callback(redact(data, secret))

        llm._init_logger_fn = safe_logger
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
