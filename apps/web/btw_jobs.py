"""Application-owned BTW jobs; a browser connection only observes execution."""
import asyncio
import json
from dataclasses import dataclass, field
from typing import Any


@dataclass
class BtwJob:
    task: Any = None
    answer: str = ''
    progress: dict = field(default_factory=dict)
    history_id: str = ''
    error: str = ''
    done: bool = False
    stop_requested: bool = False
    listeners: set = field(default_factory=set)

    def snapshot(self):
        result = {}
        if self.answer:
            result['answer'] = self.answer
        if self.progress:
            result['progress'] = self.progress
        if self.history_id:
            result['history_id'] = self.history_id
        if self.error:
            result['error'] = self.error
        return result

    def publish(self, frame):
        if 'answer' in frame:
            self.answer = frame['answer']
        elif 'delta' in frame:
            self.answer += frame['delta']
        if 'progress' in frame:
            self.progress = frame['progress']
        self.history_id = frame.get('history_id', self.history_id)
        self.error = frame.get('error', self.error)
        for queue in self.listeners:
            if queue.full():
                while not queue.empty():
                    queue.get_nowait()
                queue.put_nowait(self.snapshot())
                if not frame.get("done"):
                    continue
            queue.put_nowait(frame)

    async def events(self):
        queue = asyncio.Queue(maxsize=32)
        self.listeners.add(queue)
        try:
            snapshot = self.snapshot()
            if snapshot:
                yield {'data': json.dumps(snapshot, ensure_ascii=False)}
            if self.done:
                yield {'data': json.dumps({'done': True})}
                return
            while True:
                frame = await queue.get()
                yield {'data': json.dumps(frame, ensure_ascii=False)}
                if frame.get('done'):
                    return
        finally:
            # Navigation detaches the observer, never the producer.
            self.listeners.discard(queue)


class BtwJobs:
    def __init__(self):
        self.jobs = {}
        self.closing = False
        self.cleanup_owners = {}

    def status(self, run_id):
        job = self.jobs.get(run_id)
        pending = run_id in self.cleanup_owners
        return {'active': bool(job and not job.done) or pending,
                'progress': {'phase': 'cleanup'} if pending else job.progress if job else {}}

    def start(self, run_id, factory):
        if self.closing or run_id in self.jobs or run_id in self.cleanup_owners:
            raise ValueError('BTW already running or service shutting down')
        job = BtwJob()
        self.jobs[run_id] = job
        async def produce():
            try:
                async for event in factory():
                    frame = json.loads(event['data'])
                    if not frame.get('done'):
                        job.publish(frame)
            except asyncio.CancelledError:
                pass
            except Exception as exc:
                job.publish({'error': f'BTW Worker failed ({type(exc).__name__})'})
            finally:
                job.done = True
                job.publish({'done': True})
                if self.jobs.get(run_id) is job:
                    self.jobs.pop(run_id, None)
        job.task = asyncio.create_task(produce(), name=f'btw-{run_id}')
        def completed(task):
            # A task cancelled before its first instruction never enters finally.
            if not job.done:
                job.done = True
                job.publish({'done': True})
            if self.jobs.get(run_id) is job:
                self.jobs.pop(run_id, None)
        job.task.add_done_callback(completed)
        return job

    def retain_cleanup(self, run_id, attempt):
        """Retain an independent owner until process/container exit is proved."""
        if run_id in self.cleanup_owners:
            raise RuntimeError('duplicate BTW cleanup owner')
        async def reap():
            while True:
                try:
                    if await attempt():
                        return
                except asyncio.CancelledError:
                    raise
                except Exception:
                    pass  # Keep ownership and retry; never log materialised secrets.
                await asyncio.sleep(1)
        task = asyncio.create_task(reap(), name=f'btw-cleanup-{run_id}')
        self.cleanup_owners[run_id] = task
        def finished(done):
            # Cancellation is not evidence of process exit.
            if not done.cancelled() and done.exception() is None:
                if self.cleanup_owners.get(run_id) is done:
                    self.cleanup_owners.pop(run_id, None)
        task.add_done_callback(finished)

    async def stop(self, run_id, timeout=15):
        deadline = asyncio.get_running_loop().time() + timeout
        job = self.jobs.get(run_id)
        if job:
            if not job.stop_requested:
                job.stop_requested = True
                job.task.cancel()
            done, _ = await asyncio.wait([job.task], timeout=max(0, deadline - asyncio.get_running_loop().time()))
            if not done:
                return False
        owner = self.cleanup_owners.get(run_id)
        if owner:
            done, _ = await asyncio.wait([owner], timeout=max(0, deadline - asyncio.get_running_loop().time()))
            return bool(done) and not owner.cancelled() and owner.exception() is None
        return True

    async def shutdown(self):
        self.closing = True
        results = await asyncio.gather(*(self.stop(run_id) for run_id in set(self.jobs) | set(self.cleanup_owners)))
        if not all(results):
            raise RuntimeError('BTW runtime exit is not yet confirmed')
