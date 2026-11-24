import { test } from 'node:test';
import assert from 'node:assert/strict';
import { expect } from '@playwright/test';
import { componentPage } from './devices.test.mjs';

const snapshot = (text, revision = 1) => ({
  type:'snapshot', session_id:'one', run_id:null, revision, busy:false, cancel_requested:false,
  phase:null, blocked_reason:null, live:{current:null,interrupted:null}, queued:[], plan:null,
  messages:[{id:'message',role:'user',parts:[{type:'text',text}]}],
});

async function conversation(t, sidebar = false) {
  const fixture = await componentPage('ChatConversation', {
    conversation:{id:'one', title:'Chat', busy:false, subagent:true},
    draftState:{text:'', files:[], missingFiles:[], revision:0}, blocked:false, active:true, browserSidebar:sidebar,
  });
  t.after(() => fixture.browser.close());
  await fixture.page.evaluate(() => {
    app.component('ChatItem',{props:['message'],setup:p=>()=>VueForTests.h('article',p.message.parts[0].text)});
  });
  return fixture;
}

test('a healthy first SSE snapshot opens chat without a duplicate run request', async t => {
  const {page,mount} = await conversation(t);
  const calls = [];
  page.on('request',request=>{
    if(/\/sessions\/one\/(run|events)$/.test(request.url())) calls.push(request.url().split('/').at(-1));
  });
  await page.route('**/sessions/one/run',route=>route.fulfill({json:snapshot('From run')}));
  await page.route('**/sessions/one/events',route=>route.fulfill({
    contentType:'text/event-stream', body:`data: ${JSON.stringify(snapshot('From stream',2))}\n\n`,
  }));
  // Keep the stream open after delivering the first frame; EOF is tested separately.
  await page.evaluate(() => {
    const fetchOriginal=window.fetch;
    window.fetch=async (url,init)=>{
      const response=await fetchOriginal(url,init);
      if(!url.endsWith('/events')) return response;
      const frame=new Uint8Array(await response.arrayBuffer());
      return new Response(new ReadableStream({start(stream){
        stream.enqueue(frame);
        init.signal.addEventListener('abort',()=>stream.error(init.signal.reason),{once:true});
      }}));
    };
  });
  await page.clock.install();
  await mount();
  await expect(page.locator('article')).toHaveText('From stream');
  await page.clock.fastForward(3000);
  assert.deepEqual(calls,['events']);
});

test('a dropped stream refreshes canonical history and reconnects', async t => {
  const {page,mount} = await conversation(t);
  const calls = [];
  await page.route('**/sessions/one/run',route=>{calls.push('run');return route.fulfill({json:snapshot('Recovered history',2)})});
  await page.route('**/sessions/one/events',async route=>{
    calls.push('events');
    if(calls.length===1) return route.fulfill({
      contentType:'text/event-stream',body:`data: ${JSON.stringify(snapshot('First frame'))}\n\n`,
    });
    await route.abort();
  });
  await mount();
  await expect(page.locator('article')).toHaveText('Recovered history');
  await expect.poll(()=>calls.slice(0,3)).toEqual(['events','run','events']);
});

for (const unavailable of [false,true]) test(`chat falls back to run when SSE ${unavailable?'is unavailable':'stalls at startup'}`, async t => {
  const {page,mount} = await conversation(t,!unavailable);
  const calls = [];
  let current=snapshot('Initial fallback');
  let release;
  const wait=new Promise(resolve=>{release=resolve});
  t.after(release);
  await page.route('**/sessions/one/run',route=>{calls.push('run');return route.fulfill({json:current})});
  await page.route('**/sessions/one/events',async route=>{
    calls.push('events');
    if(unavailable) return route.fulfill({status:404});
    await wait;
    await route.abort().catch(()=>{});
  });
  await mount();
  await expect(page.locator('article')).toHaveText('Initial fallback');
  assert.deepEqual(calls.slice(0,2),['events','run']);
  if(unavailable) return;
  await page.evaluate(() => {
    Object.defineProperty(document,'visibilityState',{configurable:true,value:'hidden'});
    document.dispatchEvent(new Event('visibilitychange'));
    window.dispatchEvent(new Event('blur'));
  });
  current=snapshot('Canonical state after return',2);
  await page.evaluate(() => {
    Object.defineProperty(document,'visibilityState',{configurable:true,value:'visible'});
    document.dispatchEvent(new Event('visibilitychange'));
    window.dispatchEvent(new Event('focus'));
  });
  await expect(page.locator('article')).toHaveText('Canonical state after return');
  assert.equal(calls.filter(value=>value==='run').length,2);
});

test('a canonical 404 after SSE failure removes the conversation', async t => {
  const {page,mount} = await conversation(t);
  await page.route('**/sessions/one/{events,run}',route=>route.fulfill({status:404}));
  await page.evaluate(()=>{props.onRemoved=()=>{window.wasRemoved=true}});
  await mount();
  await expect.poll(()=>page.evaluate(()=>window.wasRemoved)).toBe(true);
});

test('model pickers share pending and recent catalogs across remounts, without caching failures', async t => {
  const {page,browser,mount}=await componentPage('ChatModelPicker',{disabled:false});
  t.after(()=>browser.close());
  let calls=0;
  let release;
  const wait=new Promise(resolve=>{release=resolve});
  t.after(release);
  await page.route('**/api/chat/models',async route=>{
    calls++;
    if(calls===1) {await wait;return route.fulfill({status:503})}
    return route.fulfill({json:{models:['fast'],default_model:calls===2?'alpha':'beta'}});
  });
  await page.evaluate(()=>{
    const {h}=VueForTests;
    const render=app._component.render;
    props.instances=[1,2];
    app._component.render=()=>h('div',props.instances.map(key=>{
      const picker=render();
      return h(picker.type,{disabled:false,key});
    }));
    app.component('UPopover',{setup:(_,{slots})=>()=>h('div',[slots.default?.(),slots.content?.()])});
  });
  await mount();
  const triggers=page.getByRole('button',{name:'Conversation model',exact:true});
  await expect(triggers).toHaveCount(2);
  await expect.poll(()=>calls).toBe(1);
  assert.deepEqual(await triggers.allTextContents(),['Auto','Auto']);
  release();
  await expect(page.getByRole('alert')).toHaveCount(2);
  await page.evaluate(()=>{props.instances=[3]});
  await expect(triggers).toHaveAttribute('title','Auto · alpha');
  await page.evaluate(()=>{props.instances=[4]});
  await expect(triggers).toHaveAttribute('title','Auto · alpha');
  assert.equal(calls,2);
  await page.evaluate(()=>{const now=Date.now();Date.now=()=>now+10*60_000;props.instances=[5]});
  await expect(triggers).toHaveAttribute('title','Auto · beta');
  assert.equal(calls,3);
});
