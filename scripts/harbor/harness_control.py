"""No-model scheduling fixture; runs an actual guest command then original verifier."""
import shlex
from harbor.agents.nop import NopAgent


class HarnessControl(NopAgent):
    @staticmethod
    def name(): return 'harness-control'

    def __init__(self, *args, delay_seconds=1, **kwargs):
        if not isinstance(delay_seconds, (int, float)) or not 0 <= delay_seconds <= 120:
            raise ValueError('Invalid diagnostic guest delay')
        self.delay_seconds = delay_seconds
        super().__init__(*args, **kwargs)

    async def run(self, instruction, environment, context):
        result = await environment.exec('sleep ' + shlex.quote(str(self.delay_seconds)),
                                        timeout_sec=self.delay_seconds + 10, user='root')
        if result.return_code: raise RuntimeError('Diagnostic guest command failed')
