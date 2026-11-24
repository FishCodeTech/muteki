#!/usr/bin/env python3
"""Real migration check: legacy Web with an embedded Runner -> separate Runner service.

Expects the legacy layout to be running (one `web` service holding the Docker Socket, which spawned
its own Runner child) and drives the real `muteki upgrade --compose` against a local manifest and
a Compose file in the new layout. It must:
  * refuse to migrate while the embedded Runner still manages a live Pi Worker,
  * leave the old stack and its single Runner untouched after that refusal,
  * migrate once the Run has finished, leaving exactly one Runner (in its own service), a Web
    without the Docker Socket, and a state directory that a second Runner cannot take,
  * run a new Pi + Ollama task through the new Runner.

Environment: BINARY (host muteki-server), NEW_COMPOSE (new-layout Compose file), plus the variables
read by verify_compose_deployment.
"""
import http.server,json,os,subprocess,tempfile,threading,time
import verify_compose_deployment as v

BINARY=os.environ['BINARY'];NEW_COMPOSE=os.environ['NEW_COMPOSE']
STATE=f'{v.ROOT}/state'

def runner_processes():
    names=v.sh('docker','ps','--filter',f'label=com.docker.compose.project={v.PROJECT}','--format','{{.Names}}').split()
    found={}
    for name in names:
        out=v.sh('docker','exec',name,'sh','-c','ps -eo args | grep "muteki-server runner" | grep -v grep || true')
        if out:found[name]=out.splitlines()
    return found

def upgrade(manifest_url,root):
    env=dict(os.environ,MUTEKI_COMPOSE_FILE=NEW_COMPOSE,MUTEKI_INSTALL_ROOT=root)
    return subprocess.run([BINARY,'upgrade','--compose','--root',root,'--manifest-url',manifest_url,'--json'],capture_output=True,text=True,env=env,timeout=900)

def main():
    manifest=json.dumps(dict(schema_version=1,version='0.4.99',commit='local',images=dict(web=os.environ['MUTEKI_WEB_IMAGE'],worker=os.environ['MUTEKI_WORKER_IMAGE']))).encode()
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(manifest)));self.end_headers();self.wfile.write(manifest)
        def log_message(self,*args):pass
    server=http.server.HTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
    manifest_url=f'http://127.0.0.1:{server.server_address[1]}/release.json'

    before=runner_processes()
    assert list(before)==[v.WEB],('expected exactly one embedded Runner inside the legacy Web container',before)
    assert '/var/run/docker.sock' in v.sh('docker','inspect',v.WEB,'--format','{{range .Mounts}}{{.Destination}} {{end}}')
    status,login=v.call('/api/auth/login',dict(password=v.PASSWORD,client='api'));assert status==200,login
    v.token=login['token']
    v.seed_account()
    v.call('/api/settings/workers',dict(worker_profiles=[dict(id='fixture',name='fixture',engine='pi',account_id='compose-fixture',credential_account='compose-fixture',base_url='http://host.docker.internal:11434/v1',model=v.MODEL,enabled=True,execute_timeout=180,conclude_timeout=15,roles=['solve','respond','review'])]),method='PUT')

    run,flag,workspace,_=v.start_task()
    v.wait(lambda:v.sh('docker','ps','-q','--filter',f'label=muteki.state_dir={STATE}'),120,'legacy Worker container')
    with tempfile.TemporaryDirectory(prefix='muteki-install-') as root:
        refused=upgrade(manifest_url,root)
        refusal=refused.stdout+refused.stderr
        assert refused.returncode!=0 and '运行中的 Worker' in refusal,('migration was not refused',refused.returncode,refusal[-600:])
        assert runner_processes()==before,'refused migration changed the Runner layout'
        assert '/var/run/docker.sock' in v.sh('docker','inspect',v.WEB,'--format','{{range .Mounts}}{{.Destination}} {{end}}')
        v.wait(lambda:v.run_row(run).get('running') is False,300,'legacy Run to finish')
        assert v.run_row(run).get('solved') is True
        assert flag in v.sh('docker','exec',v.WEB,'cat',workspace+'/solved.txt',check=False)
        v.wait(lambda:not v.sh('docker','ps','-q','--filter',f'label=muteki.state_dir={STATE}'),120,'legacy Worker containers to be released')

        done=upgrade(manifest_url,root)
        assert done.returncode==0,(done.stdout[-800:],done.stderr[-800:])
    v.wait(lambda:v.sh('docker','inspect',v.WEB,'--format','{{.State.Health.Status}}',check=False)=='healthy',120,'migrated Web')
    after=runner_processes()
    assert list(after)==[v.RUNNER],('exactly one Runner must remain, in the Runner service',after)
    assert 'docker.sock' not in v.sh('docker','inspect',v.WEB,'--format','{{range .Mounts}}{{.Destination}} {{end}}')
    web_try=subprocess.run(['docker','exec',v.WEB,'/opt/muteki/bin/muteki-server','runner','--state-dir',STATE],capture_output=True,text=True,timeout=60)
    assert web_try.returncode!=0,'Web started a second controller'
    second=subprocess.run(['docker','exec',v.RUNNER,'/opt/muteki/bin/muteki-server','runner','--state-dir',STATE],capture_output=True,text=True,timeout=60)
    assert second.returncode!=0 and 'already owns' in second.stderr+second.stdout,('a second controller was not refused',second.stdout,second.stderr)

    status,login=v.call('/api/auth/login',dict(password=v.PASSWORD,client='api'));assert status==200,login
    v.token=login['token']
    run2,flag2,workspace2,_=v.start_task()
    v.wait(lambda:v.run_row(run2).get('running') is False,300,'migrated Run to finish')
    assert v.run_row(run2).get('solved') is True and flag2 in v.sh('docker','exec',v.WEB,'cat',workspace2+'/solved.txt',check=False)
    assert v.run_row(run).get('solved') is True,'pre-migration Run history lost'
    print(json.dumps(dict(actual_services=True,provider='real Ollama',model=v.MODEL,legacy_embedded_runner_count=1,migration_refused_with_live_worker=True,old_stack_unchanged_after_refusal=True,migrated_after_drain=True,runners_after_migration=1,web_has_docker_socket_after=False,second_controller_refused=True,new_task_solved_after_migration=True,pre_migration_run_preserved=True),indent=2))

if __name__=='__main__':main()
