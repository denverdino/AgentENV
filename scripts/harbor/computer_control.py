"""Deterministic GUI positive control, separate from the vision-model score.

Targets come from screenshot colors and input focus. Only this control reads
the completed window title for its final code; the model has screenshot tools
only. Both use Harbor's original desktop runtime and unmodified verifier.
"""
import asyncio
from dataclasses import asdict
import json
from pathlib import PurePosixPath
import re
import shlex

from PIL import Image
from harbor.agents.base import BaseAgent
from harbor.agents.computer_1.runtime import Computer1Session, ComputerAction
from harbor.models.trial.paths import EnvironmentPaths


def locate_color(image, color, min_width, max_width):
    pixels = image.convert('RGB').load()
    width, height = image.size
    points = {(x, y) for y in range(height) for x in range(width) if pixels[x, y] == color}
    candidates = []
    while points:
        stack = [points.pop()]
        component = []
        while stack:
            x, y = stack.pop()
            component.append((x, y))
            for point in ((x-1, y), (x+1, y), (x, y-1), (x, y+1)):
                if point in points:
                    points.remove(point)
                    stack.append(point)
        xs, ys = zip(*component)
        left, right, top, bottom = min(xs), max(xs), min(ys), max(ys)
        if min_width <= right-left+1 <= max_width and 10 <= bottom-top+1 <= 100 and 0 < top < bottom < height-1:
            candidates.append((len(component), (left+right)//2, (top+bottom)//2))
    if not candidates:
        return None
    _, x, y = max(candidates)
    return x, y


class ComputerCalibrationControl(BaseAgent):
    @staticmethod
    def name():
        return 'computer-calibration-control'

    def version(self):
        return '1.0'

    async def setup(self, environment):
        self.session = Computer1Session(environment, EnvironmentPaths.agent_dir)
        self.actions = []
        self.environment = environment
        await self.session.start()

    async def action(self, kind, **kwargs):
        action = ComputerAction(type=kind, **kwargs)
        result = await self.session.execute(action)
        if result.get('status') not in ('ok', 'done'):
            raise RuntimeError(result)
        self.actions.append(asdict(action))
        (self.logs_dir / 'control-actions.json').write_text(json.dumps(self.actions, indent=2))
        await asyncio.sleep(.4)

    async def screenshot(self):
        path = self.logs_dir / f'control-{len(self.actions):03d}.png'
        guest = PurePosixPath('/logs/agent') / path.name
        await self.session.fetch_screenshot(guest)
        await self.environment.download_file(str(guest), path)
        with Image.open(path) as image:
            return image.convert('RGB')

    async def target(self, color, min_width=30, max_width=300):
        for _ in range(14):
            target = locate_color(await self.screenshot(), color, min_width, max_width)
            if target:
                return target
            await self.action('scroll', scroll_y=300)
        raise RuntimeError('GUI target not found: ' + str(color))

    async def run(self, instruction, environment, context):
        await self.action('navigate', url='file:///app/click_calibration.html')
        for color in ((230,57,70), (69,123,157), (42,157,143), (244,162,97), (157,78,221)):
            x, y = await self.target(color, 40, 100)
            await self.action('click', x=x, y=y)
        for kind, color in (('double_click', (106,76,147)), ('right_click', (239,71,111))):
            x, y = await self.target(color, 150, 250)
            await self.action(kind, x=x, y=y)
        await self.action('key', keys=['Tab'])
        await self.action('type', text='harbor')
        await self.action('key', keys=['Enter'])
        x, y = await self.target((244,162,97), 40, 100)
        await self.action('drag', x=x, y=y, end_x=x+530, end_y=y)
        x, y = await self.target((67,97,238), 100, 300)
        await self.action('click', x=x, y=y)
        await self.action('key', keys=['ctrl', 'End'])
        x, y = await self.target((255,255,255), 150, 250)
        await self.action('zoom', zoom_region=[x-100, y-20, x+100, y+20])
        await self.screenshot()
        result = await environment.exec('DISPLAY=:1 wmctrl -l', timeout_sec=10)
        code = re.search(r'PASS CODE: ([A-Z2-9]{4})', result.stdout)
        if result.return_code or not code:
            raise RuntimeError('GUI stages did not complete: ' + result.stdout)
        answer = 'PASS — All 7 stages complete. Final answer must include CODE: ' + code[1]
        await self.action('done', result=answer)
        result = await environment.exec('printf %s ' + shlex.quote(answer) + ' > /logs/agent/final_answer.txt', timeout_sec=10)
        if result.return_code:
            raise RuntimeError('Cannot write control answer')
