"""Declared profiles must agree with actual quotas and native agent budgets."""
import argparse
import copy
import unittest


class EffectiveProfileTests(unittest.TestCase):
    def fixture(self):
        import resources
        parser = argparse.ArgumentParser(); resources.add_resource_arguments(parser)
        plan = resources.plan_resources(parser.parse_args([]),
            {'task': {'cpus': 1, 'memory_mb': 2048}}, controls=False)
        limits = {'cpu.max': '6400000 100000', 'memory.max': str(128 * 1024**3), 'pids.max': '8192'}
        agent = {'import_path': 'dashscope_agent:DashScopeTerminus2', 'model_name': 'openai/qwen3.8-flash',
                 'kwargs': {'enable_summarize': False, 'llm_call_kwargs': {'max_tokens': 4096}}}
        return resources, plan, limits, agent

    def validate(self, plan, limits, agents):
        import resources
        self.assertTrue(hasattr(resources, 'effective_profile'),
                        'There is no gate comparing actual node quotas and original agent budgets')
        return resources.effective_profile(plan, limits, agents=agents)

    def test_actual_limits_and_unrestricted_model_budget_produce_stable_profile(self):
        _, plan, limits, agent = self.fixture()
        result = self.validate(plan, limits, [agent])
        other = copy.deepcopy(plan); other['max_active_trials'] = 8
        self.assertEqual(result, self.validate(other, limits, [agent]))
        self.assertEqual(result['node'], plan['node'])
        self.assertEqual(result['guest_overrides'], plan['overrides'])
        self.assertEqual(result['agents'][0]['max_turns'], None)

    def test_smaller_actual_quotas_are_rejected(self):
        _, plan, limits, agent = self.fixture()
        for key, value in [('cpu.max', '400000 100000'), ('memory.max', str(48 * 1024**3)), ('pids.max', '4096')]:
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, 'effective'):
                self.validate(plan, {**limits, key: value}, [agent])

    def test_smoke_turn_limit_and_timeout_override_are_rejected(self):
        _, plan, limits, agent = self.fixture()
        for changed in ({**agent, 'kwargs': {**agent['kwargs'], 'max_turns': 25}},
                        {**agent, 'override_timeout_sec': 180},
                        {**agent, 'override_timeout_multiplier': 2}):
            with self.subTest(agent=changed), self.assertRaisesRegex(ValueError, 'budget'):
                self.validate(plan, limits, [changed])

    def test_model_credentials_are_not_serialized_into_profile(self):
        _, plan, limits, agent = self.fixture()
        agent['kwargs']['api_key'] = 'private-test-sentinel'
        self.assertNotIn('private-test-sentinel', str(self.validate(plan, limits, [agent])))
