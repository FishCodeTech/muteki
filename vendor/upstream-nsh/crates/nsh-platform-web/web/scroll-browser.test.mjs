import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import test from 'node:test';

let chromium;
try { execFileSync('chromium', ['--version'], { stdio: 'ignore' }); chromium = 'chromium'; } catch {}

test('entering a sticky track cancels the content-coordinate layout animation', { skip: !chromium }, async () => {
  const dir = await mkdtemp(join(tmpdir(), 'nsh-scroll-browser-'));
  try {
    const source = await readFile(new URL('./nsh.js', import.meta.url), 'utf8');
    const module = 'data:text/javascript;base64,' + Buffer.from(source + '\nexport { Player, KEYFRAMES };').toString('base64');
    await writeFile(join(dir, 'index.html'), `<!doctype html><style>body { margin:0; }</style>
    <script type="module">
      import { Player, KEYFRAMES } from '${module}';
      try {
        const style=document.createElement('style'); style.textContent=KEYFRAMES; document.head.append(style);
        for (const horizontal of [false,true]) {
          const list=document.createElement('div'), cell=document.createElement('div');
          list.className='nsh-v nsh-scroll'; cell.className='nsh-v';
          list.style.cssText='left:0;top:0;width:300px;height:200px;';
          list._id=1; list._kind='scroll'; list._horizontal=horizontal;
          list._content=horizontal?[2000,200]:[300,2000];
          cell._id=2; cell._kind='cell'; list.append(cell); document.body.append(list);
          const player=Object.create(Player.prototype); player.views=new Map([[1,list],[2,cell]]);
          const frame=position=>horizontal?[position,0,40,200]:[0,position,300,40];
          player.op_f(['f',2,...frame(400)]);
          player.layoutAnimation={update:{duration:100000,delay:0,easing:'linear'}};
          player.op_f(['f',2,...frame(500)]);
          const animation=cell._frameAnimation;
          if (!animation) throw Error('missing initial animation');
          animation.pause(); animation.currentTime=0;
          player.op_sticky(['sticky',1,[[2,500,1000]]]);
          if (horizontal) list.scrollLeft=600; else list.scrollTop=600;
          const rect=cell.getBoundingClientRect(), position=horizontal?rect.left:rect.top;
          if (Math.abs(position)>0.01) throw Error('sticky position: '+position);
          if (animation.playState!=='idle' || cell._frameAnimation) throw Error('animation still active');
          list.remove();
        }
        document.body.dataset.result='passed';
      } catch(e) { document.body.dataset.result='failed: '+e.message; }
    </script>`);
    const html = execFileSync(chromium, ['--headless', '--no-sandbox', '--disable-dev-shm-usage',
      '--disable-gpu', '--no-first-run', '--no-default-browser-check', '--virtual-time-budget=1000',
      '--user-data-dir=' + join(dir, 'profile'), '--dump-dom', pathToFileURL(join(dir, 'index.html')).href],
      { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout: 20000, maxBuffer: 1024 * 1024 });
    assert.equal(html.match(/data-result="([^"]*)"/)?.[1], 'passed');
  } finally { await rm(dir, { recursive: true, force: true }); }
});
