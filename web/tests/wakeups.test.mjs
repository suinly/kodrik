import {test} from 'node:test';
import assert from 'node:assert/strict';
import {build} from 'esbuild';
import {fileURLToPath} from 'node:url';
import {chromium,expect} from '@playwright/test';
import {componentPage} from './devices.test.mjs';

const root=fileURLToPath(new URL('../',import.meta.url));
async function wakeupsPage() {
  const result=await build({stdin:{contents:`import {createApp,ref} from 'vue';import {useWakeups} from './app/utils/wakeups';window.active=ref(false);window.app=createApp({setup(){window.manager=useWakeups(active);return ()=>null}});app.mount('#app');`,resolveDir:root},bundle:true,write:false,format:'iife'});
  const browser=await chromium.launch({headless:true});
  const page=await browser.newPage();
  await page.route('https://kodrik.test/**',r=>r.fulfill({contentType:'text/html',body:'<script id="kodrik-config" type="application/json">{"prefix":"/nested/"}</script><div id="app"></div>'}));
  await page.goto('https://kodrik.test/nested/app/wakeups');
  await page.clock.install();
  await page.addScriptTag({content:result.outputFiles[0].text});
  return {page,browser};
}
const item={id:'one',instruction:'Check build',due_at:'2026-11-13T12:00:00Z',status:'pending',source_session:'source/one',message:null,last_error:null};

test('wakeups poll GET only while active, switch selection safely and stop on unmount',async()=>{
  const {page,browser}=await wakeupsPage();
  try {
    const calls=[];let first;
    await page.route('**/api/wakeups**',async r=>{
      calls.push({url:r.request().url(),method:r.request().method()});
      if(r.request().url().endsWith('/one/events')) {first=r;return}
      return r.fulfill({json:r.request().url().endsWith('/events')?{messages:[],busy:true,live:{current:{text:'New progress'}}}:[item]});
    });
    await page.clock.runFor(6000);
    assert.equal(calls.length,0);
    await page.evaluate(()=>{active.value=true;manager.choose('one')});
    await expect.poll(()=>calls.length).toBe(2);
    await page.evaluate(()=>manager.choose('two'));
    await expect.poll(()=>page.evaluate(()=>manager.events?.live?.current?.text)).toBe('New progress');
    await first.fulfill({json:{messages:[],busy:false,live:{current:{text:'Stale progress'}}}}).catch(()=>{});
    assert.equal(await page.evaluate(()=>manager.events.live.current.text),'New progress');
    await page.clock.runFor(3000);
    await expect.poll(()=>calls.length).toBe(5);
    await page.evaluate(()=>active.value=false);
    const stopped=calls.length;
    await page.clock.runFor(9000);
    assert.equal(calls.length,stopped);
    await page.evaluate(()=>active.value=true);
    await expect.poll(()=>calls.length).toBe(stopped+2);
    await page.evaluate(()=>app.unmount());
    const unmounted=calls.length;
    await page.clock.runFor(9000);
    assert.equal(calls.length,unmounted);
    assert.ok(calls.every(c=>c.method==='GET' && c.url.includes('/nested/api/wakeups')));
  } finally {await browser.close()}
});

test('wakeups keep errors visible and recover on next poll',async()=>{
  const {page,browser}=await wakeupsPage();
  try {
    let failed=true;
    await page.route('**/api/wakeups**',r=>r.fulfill(failed?{status:503}:{json:r.request().url().endsWith('/events')?{messages:[],busy:false}:[item]}));
    await page.evaluate(()=>{active.value=true;manager.choose('one')});
    await expect.poll(()=>page.evaluate(()=>!!manager.error && !!manager.eventsError)).toBe(true);
    failed=false;
    await page.clock.runFor(3000);
    await expect.poll(()=>page.evaluate(()=>manager.items.length)).toBe(1);
    await expect.poll(()=>page.evaluate(()=>[manager.error,manager.eventsError])).toEqual(['','']);
  } finally {await browser.close()}
});

test('read-only wakeup card escapes errors, links only source, renders messages and live progress',async()=>{
  const manager={items:[item],selected:'one',current:{...item,last_error:'<b>Oops</b>',message:'Result'},loading:false,error:'',eventsError:'',eventsLoading:false,events:{messages:[{id:'event',role:'assistant',parts:[{type:'text',text:'Saved answer'}]}],busy:true,live:{current:{text:'Live progress'}},blocked_reason:null,phase:{Parked:{reason:'Permission required'}},plan:{plan:[{step:'Verify build',status:'in_progress'}]}}};
  const {page,browser,mount}=await componentPage('WakeupsManager',{manager});
  try {
    await page.addScriptTag({content:`props.manager.dateFormat=new Intl.DateTimeFormat('en-US');app.component('UCard',{setup:(_,ctx)=>()=>VueForTests.h('article',ctx.slots.default?.())});app.component('ChatItem',{props:['message','actions','editable'],setup:p=>()=>VueForTests.h('div',{'data-actions':String(p.actions),'data-editable':String(p.editable)},p.message.parts.map(part=>part.text).join(''))});`});
    await mount();
    await expect(page.getByText('Check build',{exact:true})).toBeVisible();
    await expect(page.getByText('Saved answer',{exact:true})).toBeVisible();
    await expect(page.getByText('Live progress',{exact:true})).toBeVisible();
    await expect(page.getByText('<b>Oops</b>',{exact:true})).toBeVisible();
    await expect(page.getByText('Permission required',{exact:true})).toBeVisible();
    assert.deepEqual(await page.locator('[data-actions]').allTextContents(),['Saved answer','Live progress']);
    await page.screenshot({path:'/tmp/kodrik-wakeups-ui.png',fullPage:true});
    assert.equal(await page.locator('b').count(),0);
    await expect(page.getByRole('button',{name:'Source conversation'})).toHaveAttribute('to','/nested/app/chat/source%2Fone');
    assert.equal(await page.locator('[data-actions="false"][data-editable="false"]').count(),2);
    assert.equal(await page.getByRole('textbox').count(),0);
    assert.equal(await page.getByRole('button').count(),1);
  } finally {await browser.close()}
});
