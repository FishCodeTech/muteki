"""Real localhost Streamable HTTP and SSE MCP interoperability across providers."""
from __future__ import annotations
import asyncio
import os
from pathlib import Path
import socket
import sys
import tempfile
import httpx
sys.path.insert(0,str(Path(__file__).resolve().parents[1]))
from muteki.conversation.chat_plugins import ChatPluginService, ENGINES
from muteki.conversation.chat_providers import PROVIDERS

# Engines without Gateway delivery must report no injected tools rather than claim them.
GATEWAY_ENGINES=[e for e in ENGINES if e not in PROVIDERS or PROVIDERS[e].gateway_tools]
NO_GATEWAY_ENGINES=[e for e in ENGINES if e not in GATEWAY_ENGINES]

SERVER='''import sys
from mcp.server.fastmcp import FastMCP
server=FastMCP("transport-check", host="127.0.0.1", port=int(sys.argv[1]))
@server.tool()
def echo(text: str) -> str: return text
@server.resource("check://resource")
def resource() -> str: return "HTTP_RESOURCE_OK"
@server.prompt()
def greeting(name: str) -> str: return "Hello " + name
server.run(transport=sys.argv[2])
'''

async def check():
 with tempfile.TemporaryDirectory(prefix='muteki-mcp-http-') as temp:
  root=Path(temp);script=root/'server.py';script.write_text(SERVER)
  for transport,suffix in [('streamable-http','/mcp'),('sse','/sse')]:
   with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
   proc=await asyncio.create_subprocess_exec(sys.executable,str(script),str(port),transport,cwd=root,
         env={k:v for k,v in os.environ.items() if k in {'PATH','LANG','LC_ALL'}}|{'HOME':temp},
         stdout=asyncio.subprocess.DEVNULL,stderr=asyncio.subprocess.DEVNULL)
   service=ChatPluginService(root/transport)
   try:
    async with httpx.AsyncClient(timeout=.5) as client:
     for _ in range(80):
      try:await client.get(f'http://127.0.0.1:{port}/health');break
      except httpx.HTTPError:await asyncio.sleep(.1)
     else:raise RuntimeError('fixture server startup failed')
    service.add_mcp('transport-check',{'check':{'url':f'http://127.0.0.1:{port}{suffix}','type':transport}})
    for engine in NO_GATEWAY_ENGINES:
     assert await service.prepare_tools(engine)==[],engine
    for engine in GATEWAY_ENGINES:
     tools=await service.prepare_tools(engine)
     echo=next(t for t in tools if t['_tool']=='echo')
     result=await service.invoke(engine,echo['name'],{'text':engine})
     assert result['content'][0]['text']==engine
     resource=next(t for t in tools if t.get('_method')=='read_resource')
     result=await service.invoke(engine,resource['name'],{'uri':'check://resource'})
     assert result['contents'][0]['text']=='HTTP_RESOURCE_OK'
     prompt=next(t for t in tools if t.get('_method')=='get_prompt')
     result=await service.invoke(engine,prompt['name'],{'name':'greeting','arguments':{'name':engine}})
     assert result['messages'][0]['content']['text']=='Hello '+engine
    print('PASS:',transport,f'tools, resources and prompts across {len(GATEWAY_ENGINES)} providers;',
          f'no Gateway tools claimed for {", ".join(NO_GATEWAY_ENGINES) or "none"}',flush=True)
   finally:
    await service.close();proc.terminate();await proc.wait()
if __name__=='__main__':asyncio.run(check())
