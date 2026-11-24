#!/usr/bin/env python3
"""Verify frozen automatic quotas and exact preservation of explicit limits."""
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
from verify_runner_ipc import read


def main():
    with tempfile.TemporaryDirectory(prefix='runner-default-limits-',dir='/tmp') as directory, tempfile.TemporaryFile() as log:
        root=Path(directory).resolve();sock=root/'runner/control.sock'
        env=dict(os.environ);env.pop('MUTEKI_RUNNER_SOCKET',None)
        runner=subprocess.Popen([str(Path('.cache/target/debug/muteki-server').resolve()),'runner','--state-dir',str(root)],env=env,stdout=log,stderr=log)
        handles=[]
        def call(path,body=None):
            code,result=read(sock,path,body);assert code==200,result;return result
        def ensure(run,settings,generation=1):
            handle=call('/v1/containers/ensure',{'run_id':run,'owner_run_id':run,'generation':generation,'settings':settings});handles.append((handle,generation));return handle
        def stop(handle,generation=1):
            call('/v1/containers/'+handle['run_id']+'/teardown',{'generation':generation})
        try:
            deadline=time.monotonic()+20
            while not sock.exists():
                if runner.poll() is not None or time.monotonic()>deadline:
                    log.seek(0);raise RuntimeError(log.read().decode())
                time.sleep(.05)
            a=ensure('explicit-a',{'worker_container_scope':'shared','worker_memory':'0.5g','worker_cpus':'0.5','worker_pids_limit':'128','_runner_browser_enabled':True})
            b=ensure('explicit-b',{'worker_container_scope':'shared','worker_memory':'512m','worker_cpus':0.5,'worker_pids_limit':128,'_runner_browser_enabled':True})
            assert a['container_id']==b['container_id']
            assert a['limits']['memory_bytes']==512*1024*1024 and a['limits']['pids_limit']==128 and a['limits']['cpus']==0.5
            browser_settings={'worker_container_scope':'run','_runner_browser_enabled':True}
            browser=ensure('browser-default',browser_settings)
            assert browser['limits']['memory_bytes']>=2*1024**3 and browser['limits']['pids_limit']>=512,browser['limits']
            prior=call('/v1/runs/browser-default')['run']
            again=ensure('browser-default',browser_settings)
            assert again['container_id']==browser['container_id'] and again['limits']==browser['limits']
            assert call('/v1/runs/browser-default')['run']['resource_limit_sources']==prior['resource_limit_sources']
            code,result=read(sock,'/v1/containers/ensure',{'run_id':'browser-default','owner_run_id':'browser-default','generation':1,'settings':dict(browser_settings,worker_memory='512m')})
            assert code!=200 and 'policy' in str(result),result
            assert call('/v1/runs/browser-default')['run']['resource_limits']==prior['resource_limits']
            stop(browser)
            next_run=ensure('browser-default',{'worker_container_scope':'run','worker_memory':'512m','worker_cpus':0.5,'worker_pids_limit':128},2)
            assert next_run['container_id']!=browser['container_id'] and next_run['limits']['memory_bytes']==512*1024*1024
            assert call('/v1/runs/browser-default')['run']['browser_reserved'] is False
            code,result=read(sock,'/v1/containers/ensure',{'run_id':'bad-array','settings':[]})
            assert code==409 and 'object' in str(result),result
            assert call('/v1/health')['service']=='muteki-runner'
            stop(a);stop(b);stop(next_run,2)
            print(json.dumps({'equivalent_explicit_limits_share_pool':True,'explicit_browser_limits_not_increased':True,
                'independent_browser_defaults':browser['limits'],'same_generation_preserves_container_limits_and_sources':True,
                'live_quota_change_rejected':True,'new_generation_accepts_new_limits':True,
                'browser_budget_cleared_only_after_stop_and_new_generation':True,'invalid_settings_rejected_without_handler_panic':True},indent=2))
        finally:
            if runner.poll() is None:
                runner.send_signal(signal.SIGINT)
                try:runner.wait(timeout=10)
                except subprocess.TimeoutExpired:runner.kill();runner.wait()
            ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={root}','--format','{{.ID}}'],text=True).splitlines()
            if ids:subprocess.run(['docker','rm','-f',*ids],check=True,capture_output=True)


if __name__=='__main__':main()
