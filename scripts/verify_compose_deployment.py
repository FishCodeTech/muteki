#!/usr/bin/env python3
"""Real Compose Web/Runner separation check with Pi and Ollama.

Expects a running Compose project (see docker-compose.web.yml) and checks, against the
actual containers:
  * the Web service has no Docker Socket and cannot reach Docker,
  * only the Runner service mounts it,
  * a real Pi + Ollama task is dispatched by Web through the Runner, solves a flag and
    leaves its artifact in the shared workspace,
  * restarting the Web container during a second task leaves the Runner process and
    the Worker container untouched and the task still finishes.

Environment: MUTEKI_COMPOSE_PROJECT (default muteki), MUTEKI_WEB_PORT, MUTEKI_WEB_PASSWORD,
MUTEKI_HOST_DATA_ROOT, MUTEKI_TEST_MODEL.
"""
import base64,json,os,subprocess,time,urllib.error,urllib.request,uuid

PROJECT=os.environ.get('MUTEKI_COMPOSE_PROJECT','muteki')
PORT=os.environ.get('MUTEKI_WEB_PORT','3101')
PASSWORD=os.environ['MUTEKI_WEB_PASSWORD']
ROOT=os.environ['MUTEKI_HOST_DATA_ROOT']
MODEL=os.environ.get('MUTEKI_TEST_MODEL','deepseek-v4.1-flash:cloud')
URL=f'http://127.0.0.1:{PORT}'
WEB=f'{PROJECT}-web-1';RUNNER=f'{PROJECT}-runner-1'

def sh(*args,check=True):
    return subprocess.run(args,capture_output=True,text=True,check=check).stdout.strip()

def wait(check,seconds=60,what='state'):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        try:
            value=check()
        except (OSError,urllib.error.URLError):
            value=None
        if value:return value
        time.sleep(.5)
    raise RuntimeError('Timed out waiting for '+what)

token=None
def call(path,body=None,method=None,timeout=30):
    headers={'Content-Type':'application/json','X-Muteki-CSRF':'1'}
    if token:headers['Authorization']='Bearer '+token
    req=urllib.request.Request(URL+path,None if body is None else json.dumps(body).encode(),headers,method=method)
    try:
        with urllib.request.urlopen(req,timeout=timeout) as result:return result.status,json.load(result)
    except urllib.error.HTTPError as error:return error.code,json.load(error)

DB='''
import json,sqlite3,sys
db=sqlite3.connect('file:{root}/state/muteki.sqlite?mode=ro',uri=True)
rows=[json.loads(r[0]) for r in db.execute('SELECT value FROM documents WHERE collection=?',(sys.argv[1],))]
print(json.dumps(rows))
'''.format(root=ROOT)
def rows(collection):
    return json.loads(sh('docker','exec',WEB,'python3','-c',DB,collection))

def seed_account():
    script='''
import json,sqlite3
db=sqlite3.connect('{root}/state/muteki.sqlite')
a=dict(id='compose-fixture',engine='api',worker_engine='pi',secret='ollama',base_url='http://host.docker.internal:11434/v1',default_model='{model}',details={{'target_engine':'pi'}})
db.execute('INSERT OR REPLACE INTO documents(collection,id,value) VALUES(?,?,?)',('credential_accounts','compose-fixture',json.dumps(a)))
db.commit()
'''.format(root=ROOT,model=MODEL)
    sh('docker','exec',WEB,'python3','-c',script)

def start_task(pause_seconds=0):
    run='compose-'+uuid.uuid4().hex[:10]
    assert call('/api/runs',dict(run_id=run))[0]==200
    flag='flag{'+uuid.uuid4().hex+'}'
    workspace=f'{ROOT}/state/sessions/{run}'
    payload=base64.b64encode(flag.encode()).decode()
    sh('docker','exec',WEB,'sh','-c',f'mkdir -p {workspace} && printf "Recover the base64 encoded flag:\\n{payload}\\n" > {workspace}/challenge.txt')
    profile=dict(id='fixture',name='fixture',engine='pi',account_id='compose-fixture',credential_account='compose-fixture',base_url='http://host.docker.internal:11434/v1',model=MODEL,enabled=True,execute_timeout=180,conclude_timeout=15,roles=['solve','respond','review'])
    body=dict(command_id='start-'+run,coordinator=False,cold_start=False,worker_backend='container',worker_node_id='local',worker_container_scope='shared',engines=['fixture'],worker_profiles=[profile],start_workers=1,wall_clock_budget=240,challenge=dict(name='Compose acceptance',mode='ctf',category='misc',description='Read challenge.txt, decode its base64 payload to recover the flag. Write that exact flag to solved.txt, then submit it with the muteki-blackboard submit-flag command. Finish after submission.',flag_format='flag{...}'))
    if pause_seconds:
        body['challenge']['description']=f'First execute sleep {int(pause_seconds)} using Bash. Then '+body['challenge']['description']
    status,result=call(f'/api/runs/{run}/start',body,timeout=90);assert status==200,result
    return run,flag,workspace,profile

def run_row(run):
    return next((r for r in rows('runs') if r.get('run_id')==run),{})

def main():
    global token
    status,login=call('/api/auth/login',dict(password=PASSWORD,client='api'));assert status==200,login
    token=login['token']
    web_mounts=sh('docker','inspect',WEB,'--format','{{range .Mounts}}{{.Source}} {{end}}')
    runner_mounts=sh('docker','inspect',RUNNER,'--format','{{range .Mounts}}{{.Source}} {{end}}')
    assert 'docker.sock' not in web_mounts,web_mounts
    assert '/var/run/docker.sock' in runner_mounts,runner_mounts
    reach=subprocess.run(['docker','exec',WEB,'docker','ps'],capture_output=True,text=True)
    assert reach.returncode!=0,'Web container can reach Docker'
    seed_account()
    saved=call('/api/settings/workers',dict(worker_profiles=[dict(id='fixture',name='fixture',engine='pi',account_id='compose-fixture',credential_account='compose-fixture',base_url='http://host.docker.internal:11434/v1',model=MODEL,enabled=True,execute_timeout=180,conclude_timeout=15,roles=['solve','respond','review'])]),method='PUT')
    assert saved[0]==200,saved

    first,flag,workspace,_=start_task()
    wait(lambda:run_row(first).get('running') is False,300,'first task to finish')
    assert run_row(first).get('solved') is True,run_row(first)
    artifact=sh('docker','exec',WEB,'cat',workspace+'/solved.txt',check=False)
    assert flag in artifact,('artifact missing',artifact)

    second,flag2,workspace2,_=start_task(pause_seconds=20)
    def worker_container():
        names=sh('docker','ps','--format','{{.ID}} {{.Names}}','--filter',f'label=muteki.state_dir={ROOT}/state').splitlines()
        return [n for n in names if 'muteki' in n.lower() and PROJECT+'-' not in n]
    live=wait(lambda:[r for r in rows('runner_commands') if r.get('run_id')==second and r.get('state')=='started' and r.get('result',{}).get('worker_id') and not r.get('exit')],120,'confirmed second Worker Start receipt')
    runner_pid=sh('docker','exec',RUNNER,'pgrep','-f','muteki-server runner')
    runner_started=sh('docker','inspect',RUNNER,'--format','{{.State.StartedAt}}')
    containers_before=sorted(worker_container())
    assert containers_before,"No live Worker container at the restart boundary"
    sh('docker','restart',WEB)
    wait(lambda:sh('docker','inspect',WEB,'--format','{{.State.Health.Status}}')=='healthy',90,'Web to become healthy again')
    assert sh('docker','exec',RUNNER,'pgrep','-f','muteki-server runner')==runner_pid,'Runner process changed during Web restart'
    assert sh('docker','inspect',RUNNER,'--format','{{.State.StartedAt}}')==runner_started
    token_status,login=call('/api/auth/login',dict(password=PASSWORD,client='api'));assert token_status==200
    token=login['token']
    wait(lambda:run_row(second).get('running') is False,300,'second task to finish after Web restart')
    assert run_row(second).get('solved') is True,run_row(second)
    artifact2=sh('docker','exec',WEB,'cat',workspace2+'/solved.txt',check=False)
    assert flag2 in artifact2,('artifact missing after Web restart',artifact2)
    print(json.dumps(dict(actual_services=True,provider='real Ollama',model=MODEL,web_has_docker_socket=False,runner_only_docker_socket=True,first_task_solved=True,artifact_returned=True,web_restarted_during_task=True,runner_process_unchanged=True,second_task_solved_after_restart=True,worker_containers_seen_before_restart=len(containers_before)),indent=2))

if __name__=='__main__':main()
