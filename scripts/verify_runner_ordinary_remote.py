#!/usr/bin/env python3
"""Real ordinary ContainerHandle path, TLS node, Docker, credentials, mailbox and outputs."""
import os,subprocess,tempfile
from pathlib import Path

def main():
    with tempfile.TemporaryDirectory(prefix='runner-ordinary-remote-',dir='/tmp') as directory:
        root=Path(directory).resolve();ca=root/'ca.pem';key=root/'key.pem';cert=root/'cert.pem'
        def run(args):subprocess.run(args,check=True,capture_output=True)
        run(['openssl','req','-x509','-newkey','rsa:2048','-nodes','-days','1','-subj','/CN=Ordinary Runner CA','-addext','basicConstraints=critical,CA:TRUE','-keyout',str(root/'ca-key.pem'),'-out',str(ca)])
        run(['openssl','req','-new','-newkey','rsa:2048','-nodes','-subj','/CN=localhost','-keyout',str(key),'-out',str(root/'server.csr')])
        (root/'extensions').write_text('basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost\n')
        run(['openssl','x509','-req','-in',str(root/'server.csr'),'-CA',str(ca),'-CAkey',str(root/'ca-key.pem'),'-CAcreateserial','-days','1','-extfile',str(root/'extensions'),'-out',str(cert)])
        env=dict(os.environ,RUSTC_WRAPPER='',MUTEKI_TEST_REMOTE_FIXTURE=str(root),MUTEKI_TEST_SERVER_BINARY=str(Path('.cache/target/debug/muteki-server').resolve()))
        env.pop('MUTEKI_RUNNER_SOCKET',None)
        try:
            result=subprocess.run(['cargo','test','-p','muteki-server','--lib','ordinary_container_handle_executes_remotely_with_mailbox_and_credentials','--offline','--','--ignored','--nocapture'],env=env)
            if result.returncode:
                log=root/'node.log'
                if log.exists():print(log.read_text())
                raise SystemExit(result.returncode)
        finally:
            ids=subprocess.check_output(['docker','ps','-a','--filter',f'label=muteki.state_dir={root}/node','--format','{{.ID}}'],text=True).splitlines()
            if ids:subprocess.run(['docker','rm','-f',*ids],check=True,capture_output=True)
if __name__=='__main__':main()
