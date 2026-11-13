import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {chromium,expect} from '../../../web/node_modules/@playwright/test/index.mjs';

async function panelFixture(t, configured=true) {
  const browser=await chromium.launch({headless:true});
  t.after(()=>browser.close());
  const page=await browser.newPage();
  await page.addInitScript(configured=>{
    window.testState={tab:{id:7,title:'Example article',url:'https://article.test/read'},captures:0,updates:[]};
    window.chrome={
      tabs:{get:async()=>testState.tab, onActivated:{addListener(){}},onUpdated:{addListener(fn){testState.updates.push(fn);}},create:async()=>{}},
      windows:{getCurrent:async()=>({id:1})},
      storage:{local:{get:async()=>configured?{server:'https://server.test/'}:{},set:async()=>{}},session:{get:async()=>({}),set:async()=>{}}},
      permissions:{request:async()=>true},
      scripting:{executeScript:async()=>{testState.captures++;return [{result:{url:testState.tab.url,title:testState.tab.title,text:'Fresh page content',source:'page'}}];}}
    };
  },configured);
  await page.route('https://panel.test/**',async route=>{
    const path=new URL(route.request().url()).pathname.slice(1);
    await route.fulfill({body:await readFile(new URL(`../${path}`,import.meta.url)),contentType:path.endsWith('.html')?'text/html':path.endsWith('.css')?'text/css':'text/javascript'});
  });
  await page.route('https://server.test/**',route=>route.fulfill({contentType:'text/html',body:`<script>
    window.messages=[];
    addEventListener('message',event=>{messages.push(event.data);if(event.data.type==='kodrik-tab-init') parent.postMessage({type:'kodrik-tab-ready',channel:event.data.channel},'*');});
  </script>`}));
  await page.goto('https://panel.test/sidepanel.html?tab=7');
  if(configured) await expect(page.locator('#status')).toHaveText('');
  const chat=()=>page.frames().find(frame=>frame.url().startsWith('https://server.test/'));
  const send=(type,extra={})=>chat().evaluate(({type,extra})=>parent.postMessage({type,channel:messages.find(m=>m.type==='kodrik-tab-init').channel,...extra},'*'),{type,extra});
  return {page,chat,send};
}

test('sidepanel handshake tracks tab metadata and toggle; preview reads even when capture disabled',async t=>{
  const {page,chat,send}=await panelFixture(t);
  await expect.poll(()=>chat().evaluate(()=>messages.find(m=>m.type==='kodrik-tab-init')?.tab)).toEqual({title:'Example article',url:'https://article.test/read'});
  await send('kodrik-tab-toggle',{enabled:false});
  await expect.poll(()=>chat().evaluate(()=>messages.filter(m=>m.type==='kodrik-tab-init').at(-1)?.enabled)).toBe(false);
  await send('kodrik-tab-capture',{id:'disabled'});
  await expect.poll(()=>chat().evaluate(()=>messages.find(m=>m.id==='disabled')?.context)).toBe(null);
  assert.equal(await page.evaluate(()=>testState.captures),0);
  await send('kodrik-tab-preview',{id:'preview'});
  await expect.poll(()=>chat().evaluate(()=>messages.find(m=>m.id==='preview')?.context?.text)).toBe('Fresh page content');
  await page.evaluate(()=>{testState.tab.title='Updated title';for(const fn of testState.updates) fn(7,{title:'Updated title'},testState.tab);});
  await expect.poll(()=>chat().evaluate(()=>messages.filter(m=>m.type==='kodrik-tab-init').at(-1)?.tab?.title)).toBe('Updated title');
  await send('kodrik-tab-toggle',{enabled:'yes'});
  await send('kodrik-tab-toggle',{enabled:true,channel:'invalid'});
  await send('kodrik-tab-capture',{id:'still-disabled'});
  await expect.poll(()=>chat().evaluate(()=>messages.find(m=>m.id==='still-disabled')?.context)).toBe(null);
  await send('kodrik-tab-toggle',{enabled:true});
  await expect.poll(()=>chat().evaluate(()=>messages.filter(m=>m.type==='kodrik-tab-init').at(-1)?.enabled)).toBe(true);
  await send('kodrik-tab-capture',{id:'enabled'});
  await expect.poll(()=>chat().evaluate(()=>messages.find(m=>m.id==='enabled')?.context?.title)).toBe('Updated title');
  assert.equal(await page.evaluate(()=>testState.captures),2);
});

test('settings require trusted frame and channel; native dialog closes and failures stay visible',async t=>{
  const {page,chat,send}=await panelFixture(t);
  const dialog=page.getByRole('dialog',{name:'Настройки подключения'});
  await send('kodrik-tab-settings',{channel:'invalid'});
  await page.evaluate(()=>postMessage({type:'kodrik-tab-settings'},'*'));
  await expect(dialog).not.toBeVisible();
  await send('kodrik-tab-settings');
  await expect(dialog).toBeVisible();
  await expect(dialog.getByLabel('Адрес сервера')).toHaveValue('https://server.test/');
  await dialog.getByRole('button',{name:'Закрыть настройки'}).click();
  await expect(dialog).not.toBeVisible();
  await send('kodrik-tab-settings');
  await expect(dialog).toBeVisible();
  await page.keyboard.press('Escape');
  await expect(dialog).not.toBeVisible();
  await send('kodrik-tab-settings');
  await expect(dialog).toBeVisible();
  await page.mouse.click(2,2);
  await expect(dialog).not.toBeVisible();
  await page.evaluate(()=>{testState.tab.url='https://server.test/private';});
  await send('kodrik-tab-preview',{id:'forbidden'});
  await expect.poll(()=>chat().evaluate(()=>messages.find(m=>m.id==='forbidden')?.error)).toMatch(/недоступна/);
  await expect(page.locator('#access')).toBeVisible();
  await expect(page.locator('#status')).toContainText('недоступна');
  assert.equal(await page.evaluate(()=>testState.captures),0);
});

test('unconfigured sidepanel opens connection modal on startup',async t=>{
  const {page}=await panelFixture(t,false);
  await expect(page.getByRole('dialog',{name:'Настройки подключения'})).toBeVisible();
  await expect(page.getByLabel('Адрес сервера')).toBeVisible();
});
