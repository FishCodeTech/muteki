#!/usr/bin/env python3
"""Real Compose upgrade and rollback of a separate-Runner deployment, with a live Worker.

Drives the real `muteki upgrade --compose` / `muteki rollback --compose` on the host against a
local manifest server and two distinguishable Worker images, and checks against actual containers:
  * installing v1 brings up Web + Runner and a Pi + Ollama task runs on the v1 Worker image,
  * upgrading to v2 while that Worker is live does not rewrite the running Worker's image,
  * a task started after the upgrade uses the v2 Worker image and solves,
  * rolling back restores the v1 images and a task after the rollback uses v1 again.

Environment: BINARY (host muteki-server), MUTEKI_COMPOSE_FILE, MUTEKI_INSTALL_ROOT,
MUTEKI_WEB_IMAGE, WORKER_V1, WORKER_V2, plus the variables read by verify_compose_deployment.
"""
import http.server,json,os,subprocess,threading,time
import verify_compose_deployment as v

BINARY=os.environ['BINARY']
COMPOSE=os.environ['MUTEKI_COMPOSE_FILE']
INSTALL_ROOT=os.environ['MUTEKI_INSTALL_ROOT']
WEB_IMAGE=os.environ['MUTEKI_WEB_IMAGE']
WORKER_V1=os.environ['WORKER_V1'];WORKER_V2=os.environ['WORKER_V2']
STATE=f'{v.ROOT}/state'

def manage(action,manifest_url=None):
    env=dict(os.environ,MUTEKI_COMPOSE_FILE=COMPOSE,MUTEKI_INSTALL_ROOT=INSTALL_ROOT)
    env.pop('MUTEKI_WORKER_IMAGE',None);env.pop('MUTEKI_RELEASE_VERSION',None)
    argv=[BINARY,action,'--compose','--root',INSTALL_ROOT,'--json']
    if manifest_url:argv+=['--manifest-url',manifest_url]
    return subprocess.run(argv,capture_output=True,text=True,env=env,timeout=1800)

def worker_containers():
    ids=v.sh('docker','ps','--filter',f'label=muteki.state_dir={STATE}','--format','{{.ID}}',check=False).splitlines()
    return [[cid,v.sh('docker','inspect',cid,'--format','{{.Image}}')] for cid in ids if cid.strip()]

def service_worker_image(service):
    return v.sh('docker','inspect',f'{v.PROJECT}-{service}-1','--format',
        '{{range .Config.Env}}{{if eq (index (split . "=") 0) "MUTEKI_WORKER_IMAGE"}}{{index (split . "=") 1}}{{end}}{{end}}',check=False)

def login():
    status,reply=v.call('/api/auth/login',dict(password=v.PASSWORD,client='api'));assert status==200,reply
    v.token=reply['token']

def healthy():
    return v.sh('docker','inspect',f'{v.PROJECT}-web-1','--format','{{.State.Health.Status}}',check=False)=='healthy'

def main():
    manifests={'v1':dict(schema_version=1,version='0.4.97',commit='local-v1',images=dict(web=WEB_IMAGE,worker=WORKER_V1)),
               'v2':dict(schema_version=1,version='0.4.98',commit='local-v2',images=dict(web=WEB_IMAGE,worker=WORKER_V2))}
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            key=self.path.strip('/').removesuffix('.json')
            body=json.dumps(manifests.get(key,{})).encode()
            self.send_response(200 if key in manifests else 404)
            self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(body)))
            self.end_headers();self.wfile.write(body)
        def log_message(self,*args):pass
    server=http.server.HTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
    base=f'http://127.0.0.1:{server.server_address[1]}'
    image_v1=v.sh('docker','image','inspect',WORKER_V1,'--format','{{.Id}}')
    image_v2=v.sh('docker','image','inspect',WORKER_V2,'--format','{{.Id}}')
    assert image_v1!=image_v2
    summary={'worker_image_ids':{'v1':image_v1,'v2':image_v2}}

    installed=manage('upgrade',base+'/v1.json')
    assert installed.returncode==0,(installed.stdout[-2000:],installed.stderr[-2000:])
    v.wait(healthy,180,'Web after the v1 install')
    summary['installed_version']=json.loads(open(os.path.join(INSTALL_ROOT,'install.json')).read())['current_version']
    summary['runner_worker_image_v1']=service_worker_image('runner')
    assert summary['runner_worker_image_v1']==WORKER_V1,summary

    login();v.seed_account()
    profile=dict(id='fixture',name='fixture',engine='pi',account_id='compose-fixture',credential_account='compose-fixture',base_url='http://host.docker.internal:11434/v1',model=v.MODEL,enabled=True,execute_timeout=180,conclude_timeout=15,roles=['solve','respond','review'])
    assert v.call('/api/settings/workers',dict(worker_profiles=[profile]),method='PUT')[0]==200

    first,flag,workspace,_=v.start_task(pause_seconds=20)
    live=v.wait(worker_containers,180,'a Worker container on the v1 image')
    summary['first_worker_images']=sorted({image for _,image in live})
    assert summary['first_worker_images']==[image_v1],summary
    live_ids=[cid for cid,_ in live]

    runner_before=v.sh('docker','inspect',v.RUNNER,'--format','{{.Id}}')
    upgraded=manage('upgrade',base+'/v2.json')
    summary['upgrade_with_live_worker']=dict(exit=upgraded.returncode,output=(upgraded.stdout+upgraded.stderr)[-1200:])
    assert upgraded.returncode!=0 and '排空' in upgraded.stdout+upgraded.stderr,summary
    assert v.sh('docker','inspect',v.RUNNER,'--format','{{.Id}}')==runner_before
    summary['live_worker_images_after_upgrade']={cid:v.sh('docker','inspect',cid,'--format','{{.Image}}') for cid in live_ids}
    assert all(image==image_v1 for image in summary['live_worker_images_after_upgrade'].values()),summary
    login()
    deadline=time.monotonic()+420
    while time.monotonic()<deadline and v.run_row(first).get('running') is not False:time.sleep(5)
    summary['first_run']=dict(running=v.run_row(first).get('running'),solved=v.run_row(first).get('solved'),
        finish_reason=v.run_row(first).get('finish_reason'),artifact=flag in v.sh('docker','exec',f'{v.PROJECT}-web-1','cat',workspace+'/solved.txt',check=False))

    assert summary['first_run']['solved'] is True and summary['first_run']['artifact'],summary
    v.wait(lambda:not worker_containers(),240,'Workers to drain before upgrading')
    upgraded=manage('upgrade',base+'/v2.json')
    assert upgraded.returncode==0,(upgraded.stdout[-2000:],upgraded.stderr[-2000:])
    v.wait(healthy,240,'Web after the v2 upgrade')
    summary['upgraded_version']=json.loads(open(os.path.join(INSTALL_ROOT,'install.json')).read())['current_version']
    summary['runner_worker_image_v2']=service_worker_image('runner')
    assert summary['runner_worker_image_v2']==WORKER_V2,summary
    login()
    v.wait(lambda:not worker_containers(),240,'Workers to be released before the second task')
    second,flag2,workspace2,_=v.start_task(pause_seconds=20)
    live2=v.wait(worker_containers,180,'a Worker container on the v2 image')
    summary['second_worker_images']=sorted({image for _,image in live2})
    assert summary['second_worker_images']==[image_v2],summary
    runner_v2=v.sh('docker','inspect',v.RUNNER,'--format','{{.Id}}')
    refused=manage('rollback')
    assert refused.returncode!=0 and '排空' in refused.stdout+refused.stderr,(refused.stdout,refused.stderr)
    assert v.sh('docker','inspect',v.RUNNER,'--format','{{.Id}}')==runner_v2
    summary['rollback_with_live_worker_refused']=True
    v.wait(lambda:v.run_row(second).get('running') is False,420,'the post-upgrade task to finish')
    summary['second_run']=dict(solved=v.run_row(second).get('solved'),artifact=flag2 in v.sh('docker','exec',f'{v.PROJECT}-web-1','cat',workspace2+'/solved.txt',check=False))
    assert summary['second_run']['solved'] is True and summary['second_run']['artifact'],summary

    v.wait(lambda:not worker_containers(),240,'Workers to be released before the rollback')
    rolled=manage('rollback')
    summary['rollback']=dict(exit=rolled.returncode,output=(rolled.stdout+rolled.stderr)[-1200:])
    assert rolled.returncode==0,summary
    v.wait(healthy,240,'Web after the rollback')
    summary['rolled_back_version']=json.loads(open(os.path.join(INSTALL_ROOT,'install.json')).read())['current_version']
    summary['runner_worker_image_after_rollback']=service_worker_image('runner')
    assert summary['runner_worker_image_after_rollback']==WORKER_V1,summary
    login()
    third,flag3,workspace3,_=v.start_task()
    live3=v.wait(worker_containers,180,'a Worker container after the rollback')
    summary['third_worker_images']=sorted({image for _,image in live3})
    assert summary['third_worker_images']==[image_v1],summary
    v.wait(lambda:v.run_row(third).get('running') is False,420,'the post-rollback task to finish')
    summary['third_run']=dict(solved=v.run_row(third).get('solved'),artifact=flag3 in v.sh('docker','exec',f'{v.PROJECT}-web-1','cat',workspace3+'/solved.txt',check=False))
    assert summary['third_run']['solved'] is True and summary['third_run']['artifact'],summary
    summary['pre_upgrade_run_preserved']=v.run_row(first).get('run_id')==first

    print(json.dumps(summary,indent=1,default=str))
    print('UPGRADE_ROLLBACK_OK')

if __name__=='__main__':main()
