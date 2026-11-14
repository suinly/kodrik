import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtemp,cp,readFile,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {chromium} from '../../../web/node_modules/@playwright/test/index.mjs';

test('real MV3 automation binds tokens to documents, confirms mutations and blocks secrets', {timeout:60000}, async()=>{
  const dir=await mkdtemp(join(tmpdir(),'kodrik-automation-'));
  let context;
  try {
    const extension=join(dir,'extension');
    await cp(new URL('../',import.meta.url),extension,{recursive:true});
    const manifest=JSON.parse(await readFile(join(extension,'manifest.json'),'utf8'));
    manifest.host_permissions=['http://127.0.0.1/*'];
    await writeFile(join(extension,'manifest.json'),JSON.stringify(manifest));
    context=await chromium.launchPersistentContext(join(dir,'profile'),{headless:true,channel:'chromium',args:[`--disable-extensions-except=${extension}`,`--load-extension=${extension}`]});
    const worker=context.serviceWorkers()[0] || await context.waitForEvent('serviceworker');
    const page=await context.newPage();
    await page.route('http://127.0.0.1:19992/**',r=>r.fulfill({contentType:'text/html',body:'<title>Actions</title><button onclick="this.textContent=\'Clicked\'">Press</button><input aria-label="Name"><input type="password" value="SECRET"><textarea aria-label="Notes"></textarea><select aria-label="Pick"><option>a</option><option>b</option></select><div contenteditable aria-label="Edit"></div><div style="height:3000px">Long page</div>'}));
    await page.goto('http://127.0.0.1:19992/one');
    const tab=await worker.evaluate(async()=> (await chrome.tabs.query({})).find(t=>t.url==='http://127.0.0.1:19992/one').id);
    const panel=await context.newPage();
    await panel.goto(`chrome-extension://${new URL(worker.url()).host}/sidepanel.html?tab=${tab}`);
    const run=(command,accept=true)=>panel.evaluate(async({tab,command,accept})=>{
      const {executeBrowserCommand}=await import('./automation.mjs');
      return executeBrowserCommand(tab,command,async()=>accept);
    },{tab,command,accept});
    const snap=await run({action:'snapshot'});
    assert.ok(snap.snapshot_id);assert.equal(snap.title,'Actions');
    assert.ok(Buffer.byteLength(JSON.stringify(snap))<=100000);
    assert.doesNotMatch(JSON.stringify(snap),/SECRET/);
    const el=name=>snap.elements.find(e=>e.name===name).id;
    await assert.rejects(run({action:'click',snapshot_id:snap.snapshot_id,element_id:el('Press')},false),/cancel|отмен/i);
    assert.equal(await page.locator('button').textContent(),'Press');
    await run({action:'click',snapshot_id:snap.snapshot_id,element_id:el('Press')});
    assert.equal(await page.locator('button').textContent(),'Clicked');
    for(const [name,value] of [['Name','Alice'],['Notes','Hello'],['Pick','b'],['Edit','Edited']]) await run({action:'fill',snapshot_id:snap.snapshot_id,element_id:el(name),value});
    assert.equal(await page.locator('input').first().inputValue(),'Alice');
    assert.equal(await page.locator('textarea').inputValue(),'Hello');
    assert.equal(await page.locator('select').inputValue(),'b');
    assert.equal(await page.locator('[contenteditable]').textContent(),'Edited');
    const password=snap.elements.find(e=>e.type==='password');
    await assert.rejects(run({action:'fill',snapshot_id:snap.snapshot_id,element_id:password.id,value:'x'}),/password|file/i);
    await run({action:'scroll',snapshot_id:snap.snapshot_id,x:0,y:500});
    assert.ok(await page.evaluate(()=>scrollY)>0);
    assert.equal(await run({action:'evaluate',snapshot_id:snap.snapshot_id,code:'return document.title;'}),'Actions');
    await assert.rejects(run({action:'evaluate',snapshot_id:snap.snapshot_id,code:'return new Promise(()=>{});',timeout_ms:30}),/timed out/);
    await page.locator('button').evaluate(el=>el.remove());
    await assert.rejects(run({action:'click',snapshot_id:snap.snapshot_id,element_id:el('Press')}),/detached|stale/i);
    await page.setContent('<div style="opacity:0"><button>INVISIBLE</button></div><div role="group"><textarea>PRIVATE</textarea><span hidden>HIDDEN</span><b>Public</b></div><a href="/old">Destination</a>');
    const safe=await run({action:'snapshot'});
    assert.doesNotMatch(JSON.stringify(safe),/INVISIBLE|PRIVATE|HIDDEN/);
    const link=safe.elements.find(e=>e.name==='Destination');
    await page.locator('a').evaluate(el=>el.href='/changed');
    await assert.rejects(run({action:'click',snapshot_id:safe.snapshot_id,element_id:link.id}),/changed|stale/i);
    await page.setContent('<input aria-label="Only field">');
    const form=await run({action:'snapshot'});
    assert.equal(form.text,'');assert.equal(form.elements[0].name,'Only field');
    await assert.rejects(panel.evaluate(async tab=>{
      const {executeBrowserCommand}=await import('./automation.mjs');
      return executeBrowserCommand(tab,{action:'snapshot'},async()=>true,{forbiddenOrigin:'http://127.0.0.1:19992'});
    },tab),/forbidden|origin/i);
    await page.goto('http://127.0.0.1:19992/two');
    await assert.rejects(run({action:'scroll',snapshot_id:snap.snapshot_id,y:1}),/document|frame|stale/i);
  } finally {await context?.close();await rm(dir,{recursive:true,force:true});}
});

test('confirmation dialog expires without blocking timers and cannot accept late approval',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.route('https://panel.test/**',async route=>{
      const path=new URL(route.request().url()).pathname;
      await route.fulfill({contentType:path.endsWith('.mjs')?'text/javascript':'text/html',body:path.endsWith('.mjs')?await readFile(new URL(`..${path}`,import.meta.url)):'<body></body>'});
    });
    await page.goto('https://panel.test/');await page.clock.install();
    await page.evaluate(async()=>{
      const {confirmBrowserCommand}=await import('/automation.mjs');
      window.confirmation=confirmBrowserCommand({action:'evaluate',code:'return 1;'}, {title:'Example',url:'https://example.com'}).then(value=>window.answer=value);
    });
    assert.equal(await page.locator('dialog[open]').count(),1);
    await page.clock.fastForward(55001);
    assert.equal(await page.evaluate(()=>window.answer),false);
    assert.equal(await page.locator('dialog').count(),0);
  } finally {await browser.close();}
});
