"""Managed component compatibility and native hook/extension isolation checks."""
from __future__ import annotations
import json
from pathlib import Path
import subprocess
import sys
import tempfile
from unittest.mock import patch
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from muteki.conversation.chat_plugins import ChatPluginService
from muteki.conversation.composer_capabilities import resolve_capability_refs
from muteki.conversation.chat_plugin_components import inspect_components

with tempfile.TemporaryDirectory(prefix='muteki-components-') as tmp:
 root=Path(tmp);source=root/'source';source.mkdir();home=root/'operator';home.mkdir()
 (source/'commands').mkdir();(source/'commands/greet.md').write_text('---\ndescription: Greeting test\n---\nReply GREETING_OK with $ARGUMENTS.\n')
 (source/'agents').mkdir();(source/'agents/helper.md').write_text('---\nname: helper\ndescription: Test helper\n---\nAnswer briefly.\n')
 (source/'extensions').mkdir();(source/'extensions/main.js').write_text('export default function(api) {}')
 manifest={'name':'components-test','version':'1.0.0','hooks':{'hooks':{'SessionStart':[{'hooks':[{'type':'command','command':'echo HOOK_OK > "$PLUGIN_DATA/marker"; echo HOOK_OK'}]}]}},'muteki':{'extensions':{'pi':['extensions/main.js'],'omp':['extensions/main.js']}}}
 (source/'plugin.json').write_text(json.dumps(manifest))
 service=ChatPluginService(root/'private');public=service.install({'path':str(source)})
 public=service.update('components-test',native_hooks=True,digest=public['digest'])
 assert public['compatibility']['claude']['components']==['agents','commands','hooks','skills']
 assert public['compatibility']['kimi']['status']=='partial'
 assert any(x['name']=='components-test:greet' for x in service.skill_rows('kimi'))
 row=next(x for x in service.skill_rows('kimi') if x['name']=='components-test:greet')
 selected,context=resolve_capability_refs([{**row,'arguments':'quoted value'}],engine='kimi',plugin_service=service)
 assert 'Reply GREETING_OK with quoted value.' in context and selected[0]['arguments']=='quoted value'
 with patch('pathlib.Path.home',return_value=home):
  for engine in ['pi','omp']:
   env=service.prepare_environment(engine,'fixture',{})
   target=next((Path(env['PI_CODING_AGENT_DIR'])/'extensions').glob('muteki-components-test-main-*.js'))
   assert target.exists() and 'file://' in target.read_text()
  env=service.prepare_environment('claude','fixture',{})
  options=service.native_launch_options('claude',env)
  package=Path(options['plugins'][0]['path'])
  native=json.loads((package/'.claude-plugin/plugin.json').read_text())
  command=native['hooks']['SessionStart'][0]['hooks'][0]['command']
  result=subprocess.run(command,shell=True,input='{}',capture_output=True,text=True,timeout=15)
  assert result.returncode==0,(result.returncode,result.stderr)
  assert result.stdout.strip()=='HOOK_OK'
  assert (Path(env['MUTEKI_CHAT_PRIVATE_ROOT'])/'hook-state/components-test/marker').read_text().strip()=='HOOK_OK'
  assert not (home/'marker').exists()
  old=env['HOME'];service.update('components-test',enabled=False)
  new=service.prepare_environment('claude','fixture',{})
  assert old!=new['HOME'],'disabled component reused old native home'
  assert service.native_launch_options('claude',new)=={'plugins':[]}
 manifest['muteki']['requires']={'executables':['muteki-no-such-program-fixture']}
 (source/'plugin.json').write_text(json.dumps(manifest));public=service.install({'path':str(source)})
 assert all(c['status']=='blocked' for c in public['compatibility'].values())
 assert not service.skill_rows('codex')
 manifest['muteki']['requires']={'tools':['only-native-fixture-tool']}
 (source/'plugin.json').write_text(json.dumps(manifest));public=service.install({'path':str(source)})
 assert public['compatibility']['kimi']['status']=='blocked'
 service._verified_tools['codex']={'only-native-fixture-tool'}
 assert service.skill_rows('codex') and not service.skill_rows('kimi')
 (source/'skills/native-only').mkdir(parents=True)
 (source/'skills/native-only/SKILL.md').write_text('---\nname: native-only\ndescription: Native permissions\ndisallowed-tools: Bash\n---\nReply NATIVE_SKILL_OK $ARGUMENTS.\n')
 manifest['muteki'].pop('requires');manifest['commands']={'inline':{'content':'First=$0; second=$ARGUMENTS[1]; all=$ARGUMENTS'}}
 (source/'plugin.json').write_text(json.dumps(manifest));service.install({'path':str(source)})
 assert any(r['native_name']=='muteki-components-test:native-only' for r in service.skill_rows('claude'))
 assert not any(r['name'].endswith(':native-only') for r in service.skill_rows('kimi'))
 row=next(r for r in service.skill_rows('kimi') if r['name'].endswith(':inline'))
 _,context=resolve_capability_refs([{**row,'arguments':'"one two" $0'}],engine='kimi',plugin_service=service)
 assert 'First=one two; second=$0; all="one two" $0' in context,'arguments were re-expanded'
 with patch('pathlib.Path.home',return_value=home):
  env=service.prepare_environment('claude','native-only',{})
  options=service.native_launch_options('claude',env)
  native=json.loads((Path(options['plugins'][0]['path'])/'.claude-plugin/plugin.json').read_text())
  assert './skills/native-only' in native['skills'] and './.muteki-commands/inline.md' in native['commands']
 inspected=inspect_components(source,{'pi':{'extensions':['extensions/main.js']},'omp':{'agents':['agents/helper.md']}})
 assert next(c for c in inspected['components'] if c['kind']=='extensions')['engines']==['pi']
 assert any(c['kind']=='agents' and c['engines']==['omp'] for c in inspected['components'])
 assert not any(c['kind']=='agents' and 'opencode' in c['engines'] for c in inspected['components'])
 (source/'skills/scoped').mkdir()
 (source/'skills/scoped/SKILL.md').write_text('---\nname: scoped\nhooks:\n  PreToolUse:\n    - hooks:\n        - type: command\n          command: echo SCOPED_HOOK_OK\n---\nReply briefly.\n')
 public=service.install({'path':str(source)})
 assert 'echo SCOPED_HOOK_OK' in public['hook_commands']
 assert not any(r['name'].endswith(':scoped') for r in service.skill_rows('claude'))
 with patch('pathlib.Path.home',return_value=home):
  env=service.prepare_environment('claude','scoped',{})
  package=Path(service.native_launch_options('claude',env)['plugins'][0]['path'])
  assert not (package/'skills/scoped/SKILL.md').exists(),'unapproved scoped hook exposed'
  service.update('components-test',native_hooks=True,digest=public['digest'])
  env=service.prepare_environment('claude','scoped',{})
  package=Path(service.native_launch_options('claude',env)['plugins'][0]['path'])
  assert 'chat_hook_runner.py' in (package/'skills/scoped/SKILL.md').read_text()
 print('PASS: prompt templates, native agent/hooks/extensions matrix, sandboxed real hook, enable/revoke, dependencies and tool ownership')
