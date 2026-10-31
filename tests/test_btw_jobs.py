import asyncio
import json
import unittest
from apps.web.btw_jobs import BtwJobs


def frame(value):
    return {'data': json.dumps(value)}


class BtwJobsTest(unittest.IsolatedAsyncioTestCase):
    async def test_navigation_does_not_cancel_or_restart_worker(self):
        jobs = BtwJobs()
        release = asyncio.Event()
        starts = []
        async def producer():
            starts.append(True)
            yield frame({'history_id': 'one'})
            yield frame({'delta': 'partial'})
            await release.wait()
            yield frame({'answer': 'complete'})
        job = jobs.start('run-a', producer)
        observer = job.events()
        await anext(observer)
        await observer.aclose()
        self.assertFalse(job.task.done())
        self.assertTrue(jobs.status('run-a')['active'])
        with self.assertRaises(ValueError):
            jobs.start('run-a', producer)
        observer = job.events()
        initial = json.loads((await anext(observer))['data'])
        self.assertEqual(initial['answer'], 'partial')
        release.set()
        frames = [json.loads(event['data']) async for event in observer]
        self.assertIn({'answer': 'complete'}, frames)
        self.assertEqual(starts, [True])
        self.assertFalse(jobs.status('run-a')['active'])

    async def test_explicit_stop_only_cancels_target_and_waits_for_cleanup(self):
        jobs = BtwJobs()
        cleanup = []
        async def producer():
            try:
                yield frame({'progress': {'phase': 'running'}})
                await asyncio.Event().wait()
            finally:
                await asyncio.sleep(0.01)
                cleanup.append(True)
        a, b = jobs.start('a', producer), jobs.start('b', producer)
        await asyncio.sleep(0)
        self.assertTrue(await jobs.stop('a'))
        self.assertEqual(cleanup, [True])
        self.assertFalse(b.task.done())
        await jobs.shutdown()
        self.assertEqual(cleanup, [True, True])

    async def test_slow_observer_has_bounded_queue_and_correct_final_answer(self):
        jobs = BtwJobs()
        release = asyncio.Event()
        async def producer():
            yield frame({'delta': 'start'})
            await release.wait()
            for _ in range(100):
                yield frame({'delta': 'x'})
        job = jobs.start('a', producer)
        observer = job.events()
        await anext(observer)
        release.set()
        await job.task
        answer = 'start'
        async for event in observer:
            item = json.loads(event['data'])
            if 'answer' in item:
                answer = item['answer']
            elif 'delta' in item:
                answer += item['delta']
        self.assertEqual(answer, 'start' + 'x' * 100)

    async def test_cancel_before_start_releases_slot(self):
        jobs = BtwJobs()
        async def producer():
            yield frame({'delta': 'answer'})
        job = jobs.start('a', producer)
        self.assertTrue(await jobs.stop('a'))
        self.assertTrue(job.done)
        self.assertFalse(jobs.status('a')['active'])

    async def test_cleanup_owner_blocks_delete_and_new_worker_until_exit(self):
        jobs = BtwJobs()
        release = asyncio.Event()
        async def attempt():
            await release.wait()
            return True
        jobs.retain_cleanup('a', attempt)
        self.assertTrue(jobs.status('a')['active'])
        self.assertEqual(jobs.status('a')['progress']['phase'], 'cleanup')
        self.assertFalse(await jobs.stop('a', timeout=0.01))
        with self.assertRaises(ValueError):
            jobs.start('a', lambda: None)
        release.set()
        self.assertTrue(await jobs.stop('a', timeout=1))
        await asyncio.sleep(0)
        self.assertFalse(jobs.status('a')['active'])

    async def test_repeated_stop_does_not_interrupt_cleanup_again(self):
        jobs = BtwJobs()
        cleaning = asyncio.Event()
        release = asyncio.Event()
        async def producer():
            try:
                yield frame({'delta': 'partial'})
                await asyncio.Event().wait()
            finally:
                cleaning.set()
                await release.wait()
        job = jobs.start('a', producer)
        await asyncio.sleep(0)
        first = asyncio.create_task(jobs.stop('a', timeout=1))
        await cleaning.wait()
        second = asyncio.create_task(jobs.stop('a', timeout=1))
        await asyncio.sleep(0.01)
        self.assertFalse(job.task.done())
        release.set()
        self.assertTrue(await first)
        self.assertTrue(await second)
