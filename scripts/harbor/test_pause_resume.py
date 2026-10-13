"""Pause around host-side inference; preserve guest identity and error semantics."""
import asyncio
import json
import logging
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import AsyncMock, Mock, patch

from agentenv_environment import AgentENVEnvironment
from dashscope_agent import DashScopeTerminus2
from harbor.agents.terminus_2.terminus_2 import Terminus2
from harbor.llms.chat import Chat
from harbor.llms.base import LLMResponse


class EnvironmentLifecycleTests(unittest.IsolatedAsyncioTestCase):
    def environment(self):
        env = object.__new__(AgentENVEnvironment)
        env.sandbox = {'sandboxID': 'original', 'envdAccessToken': 'old'}
        env.sandbox_timeout_sec = 1800
        env._paused = False
        env._resume_pending = False
        env._lifecycle_lock = asyncio.Lock()
        env._receipt = Mock()
        env._request = AsyncMock(return_value=SimpleNamespace(json=lambda: {
            'sandboxID': 'original', 'envdAccessToken': 'new'}))
        return env

    async def test_repeated_calls_and_parallel_pause_preserve_id_and_refresh_token(self):
        env = self.environment()
        await asyncio.gather(env.pause(), env.pause())
        await env.resume()
        await env.resume()
        self.assertEqual(env._request.await_count, 2)
        self.assertEqual(env._request.await_args_list[1].kwargs, {'json': {'timeout': 1800}})
        self.assertEqual(env.sandbox['envdAccessToken'], 'new')
        self.assertFalse(env._paused)

    async def test_lost_pause_response_still_requires_resume(self):
        env = self.environment()
        env._request.side_effect = [TimeoutError('lost response'), env._request.return_value]
        with self.assertRaises(TimeoutError):
            await env.pause()
        self.assertFalse(env._paused)
        self.assertTrue(env._resume_pending)
        await env.resume()
        self.assertEqual(env._request.await_args.args, ('POST', '/sandboxes/original/resume'))
        self.assertFalse(env._paused)
        self.assertFalse(env._resume_pending)

    async def test_resume_retry_after_lost_response_still_sends_request(self):
        env = self.environment()
        await env.pause()
        env._request.side_effect = [TimeoutError('lost response'), env._request.return_value]
        with self.assertRaises(TimeoutError):
            await env.resume()
        await env.resume()
        self.assertEqual(env._request.await_count, 3)
        self.assertFalse(env._resume_pending)

    async def test_failed_or_wrong_identity_resume_retains_pending_state(self):
        for failure in (RuntimeError('resume failed'),
                        SimpleNamespace(json=lambda: {'sandboxID': 'different'})):
            env = self.environment()
            await env.pause()
            env._request.side_effect = failure if isinstance(failure, Exception) else None
            env._request.return_value = failure
            with self.assertRaises(RuntimeError):
                await env.resume()
            self.assertFalse(env._paused)
            self.assertTrue(env._resume_pending)
            self.assertEqual(env.sandbox['sandboxID'], 'original')


class InferenceLifecycleTests(unittest.IsolatedAsyncioTestCase):
    def agent(self, directory, events, enabled=True):
        agent = object.__new__(DashScopeTerminus2)
        agent._pause_taskvm_during_inference = enabled
        agent._inference_number = 0
        agent._inference_lock = asyncio.Lock()
        agent.logs_dir = Path(directory)
        agent.logger = logging.getLogger('pause-resume-tests')

        async def pause(): events.append('pause')
        async def resume(): events.append('resume')
        agent._inference_environment = SimpleNamespace(pause=pause, resume=resume)
        return agent

    async def test_success_disabled_and_model_error(self):
        for enabled, failed in ((True, False), (False, False), (True, True)):
            with self.subTest(enabled=enabled, failed=failed), tempfile.TemporaryDirectory() as directory:
                events = []
                agent = self.agent(directory, events, enabled)

                async def call():
                    events.append('infer')
                    if failed: raise ValueError('original model failure')
                    return 'response'

                if failed:
                    with self.assertRaisesRegex(ValueError, 'original model failure'):
                        await agent._call_with_paused_taskvm(call)
                else:
                    self.assertEqual(await agent._call_with_paused_taskvm(call), 'response')
                self.assertEqual(events, ['pause', 'infer', 'resume'] if enabled else ['infer'])
                if enabled:
                    rows = [json.loads(line) for line in (Path(directory) / 'pause-resume.jsonl').read_text().splitlines()]
                    self.assertEqual([r['phase'] for r in rows], events)
                    self.assertEqual(rows[1]['error_type'], 'ValueError' if failed else None)

    async def test_pause_failure_recovers_without_calling_model(self):
        with tempfile.TemporaryDirectory() as directory:
            events = []
            agent = self.agent(directory, events)
            agent._inference_environment.pause = AsyncMock(side_effect=TimeoutError('pause unknown'))
            call = AsyncMock()
            with self.assertRaisesRegex(TimeoutError, 'pause unknown'):
                await agent._call_with_paused_taskvm(call)
            call.assert_not_awaited()
            self.assertEqual(events, ['resume'])

    async def test_resume_failure_does_not_return_model_response(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = self.agent(directory, [])
            agent._inference_environment.resume = AsyncMock(side_effect=RuntimeError('restore failed'))
            with self.assertRaisesRegex(RuntimeError, 'restore failed'):
                await agent._call_with_paused_taskvm(AsyncMock(return_value='response'))

    async def test_lost_resume_response_repauses_before_inference_retry(self):
        with tempfile.TemporaryDirectory() as directory:
            agent = self.agent(directory, [])
            env = EnvironmentLifecycleTests.environment(self)
            agent._inference_environment = env
            state = 'running'
            pauses = 0
            resumes = 0

            async def request(method, path, **kwargs):
                nonlocal state, pauses, resumes
                if path.endswith('/pause'):
                    pauses += 1
                    state = 'paused'
                else:
                    resumes += 1
                    state = 'running'
                    if resumes == 1:
                        raise TimeoutError('resume response lost after server success')
                return SimpleNamespace(json=lambda: {
                    'sandboxID': 'original', 'envdAccessToken': 'new'})

            async def infer():
                self.assertEqual(state, 'paused', 'inference ran while the VM was running')
                return 'response'

            env._request.side_effect = request
            with self.assertRaisesRegex(TimeoutError, 'resume response lost'):
                await agent._call_with_paused_taskvm(infer)
            self.assertEqual(await agent._call_with_paused_taskvm(infer), 'response')
            self.assertEqual((pauses, resumes, state), (2, 2, 'running'))

    async def test_cancellation_during_pause_infer_or_resume_waits_for_recovery(self):
        for phase in ('pause', 'infer', 'resume'):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as directory:
                events = []
                agent = self.agent(directory, events)
                entered = asyncio.Event()

                async def blocked():
                    entered.set()
                    await asyncio.Event().wait()

                async def resume():
                    if phase == 'resume': entered.set()
                    await asyncio.sleep(0.01)
                    events.append('recovered')

                agent._inference_environment.resume = resume
                if phase == 'pause': agent._inference_environment.pause = blocked
                call = blocked if phase == 'infer' else AsyncMock(return_value='response')
                task = asyncio.create_task(agent._call_with_paused_taskvm(call))
                await entered.wait()
                task.cancel()
                with self.assertRaises(asyncio.CancelledError): await task
                self.assertIn('recovered', events)

    async def test_real_chat_and_direct_summary_calls_share_the_hook(self):
        with tempfile.TemporaryDirectory() as directory:
            events = []

            async def call(**kwargs):
                events.append('infer')
                return LLMResponse(content='response')

            llm = SimpleNamespace(call=call, _init_logger_fn=lambda path: lambda data: None)
            with patch.object(Terminus2, '_init_llm', return_value=llm), \
                 patch.dict('os.environ', {'OPENAI_API_KEY': 'unit-secret'}):
                agent = DashScopeTerminus2(logs_dir=Path(directory), model_name='openai/qwen3.8-flash',
                                          pause_taskvm_during_inference=True)

            async def pause(): events.append('pause')
            async def resume(): events.append('resume')
            agent._inference_environment = SimpleNamespace(pause=pause, resume=resume)
            await Chat(agent._llm).chat('main inference')
            await agent._llm.call(prompt='short summary')
            self.assertEqual(events, ['pause', 'infer', 'resume'] * 2)

    def test_model_flag_is_opt_in(self):
        from run_lab import model_agent_config
        from harbor.agents.factory import AgentFactory
        from harbor.models.trial.config import AgentConfig
        self.assertNotIn('pause_taskvm_during_inference', model_agent_config('qwen3.8-flash')['kwargs'])
        config = AgentConfig.model_validate(model_agent_config('qwen3.8-flash', True))
        agent_class = AgentFactory.get_agent_class_from_config(config)
        # The CLI validates options before constructing an agent or a VM.
        options = agent_class.options_model.model_validate(config.kwargs)
        self.assertTrue(options.pause_taskvm_during_inference)


if __name__ == '__main__':
    unittest.main()
