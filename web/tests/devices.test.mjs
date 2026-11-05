import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {fileURLToPath} from 'node:url';
import {build} from 'esbuild';
import {parse,compileScript} from '@vue/compiler-sfc';
import {chromium,expect} from '@playwright/test';

// Compile the real SFCs in memory: no app server, generated assets or Rust fixture needed.
export async function componentPage(name,props={}) {
  const root=fileURLToPath(new URL('../',import.meta.url));
  const child=name==='ProfilePage'?'DeviceConnections':name==='ChatConversation'?'EnvironmentStatus':null;
  const result=await build({stdin:{contents:`import * as VueForTests from 'vue';import {createApp,h,reactive} from 'vue';import Component from './app/components/${name}.vue';${child?`import Child from './app/components/${child}.vue';`:''}window.VueForTests=VueForTests;window.props=reactive(${JSON.stringify(props)});window.app=createApp({render:()=>h(Component,window.props)});${child?`app.component('${child}',Child);`:''}`,resolveDir:root},bundle:true,write:false,format:'iife',plugins:[{
    name:'vue-test',setup(builder){builder.onLoad({filter:/\.vue$/},async({path})=>{
      const {descriptor}=parse(await readFile(path,'utf8'),{filename:path});
      const compiled=compileScript(descriptor,{id:path,inlineTemplate:true});
      return {contents:`import {ref,reactive,computed,onMounted,onUnmounted,onBeforeUnmount,watch,toRef,useTemplateRef,nextTick} from 'vue';${compiled.content}`,loader:'ts'};
    })},
  }]});
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'en-US'});
    await page.route('https://kodrik.test/**',route=>route.fulfill({contentType:'text/html',body:'<script id="kodrik-config" type="application/json">{"prefix":"/nested/"}</script><div id="app"></div>'}));
    await page.goto('https://kodrik.test/nested/app/profile');
    await page.addScriptTag({content:result.outputFiles[0].text});
    await page.addScriptTag({content:`
      const {h}=VueForTests;
      app.component('UButton',{props:['label','loading','disabled','type'],setup:(p,{slots,attrs})=>()=>h('button',{...attrs,type:p.type||'button',disabled:p.disabled||p.loading},p.label||slots.default?.())});
      app.component('UInput',{props:['modelValue'],emits:['update:modelValue'],setup:(p,{emit,attrs})=>()=>h('input',{...attrs,value:p.modelValue,onInput:e=>emit('update:modelValue',e.target.value)})});
      app.component('UCheckbox',{props:['modelValue','label'],emits:['update:modelValue'],setup:(p,{emit})=>()=>h('label',[h('input',{type:'checkbox',checked:p.modelValue,onChange:e=>emit('update:modelValue',e.target.checked)}),p.label])});
      for(const name of ['UFormField','UBadge']) app.component(name,{props:['label'],setup:(p,{slots})=>()=>h('div',[p.label,slots.default?.()])});
      for(const name of ['CommandAuditMenu','PushSettings','USeparator']) app.component(name,{render:()=>null});
    `});
    return {page,browser,mount:()=>page.evaluate(()=>app.mount('#app'))};
  } catch(error) {await browser.close();throw error;}
}

if(process.argv[1]===fileURLToPath(import.meta.url)) {
test('owner explicitly compares request UUID and code, then approves and revokes a registered device',async()=>{
  const {page,browser,mount}=await componentPage('ProfilePage');
  try {
    const calls=[];let devices=[{id:'device-1',label:'<img src=x onerror=alert(1)>',expires_at:2000000000}];
    await page.route('**/api/profile',r=>r.fulfill({json:{name:'Owner',owner:true,telegram_id:null,password_set:true}}));
    await page.route('**/api/devices**',async r=>{
      const request=r.request();calls.push({url:request.url(),method:request.method(),body:request.postDataJSON()});
      if(request.method()==='DELETE') {devices=[];return r.fulfill({status:204})}
      if(request.method()==='POST') return r.fulfill({status:204});
      return r.fulfill({json:devices});
    });
    await mount();
    const section=page.getByRole('region',{name:'Device connections',exact:true});
    await expect(section).toBeVisible();
    await expect(section.getByText('<img src=x onerror=alert(1)>',{exact:true})).toBeVisible();
    assert.equal(await section.locator('img').count(),0);
    const approve=section.getByRole('button',{name:'Approve connection',exact:true});
    await expect(approve).toBeDisabled();
    await section.getByRole('textbox',{name:'Pairing request UUID',exact:true}).fill('not-a-uuid');
    await section.getByRole('textbox',{name:'Comparison code',exact:true}).fill('123456');
    await section.getByRole('checkbox').check();
    await expect(approve).toBeDisabled();
    await section.getByRole('textbox',{name:'Pairing request UUID',exact:true}).fill('550e8400-e29b-41d4-a716-446655440000');
    await expect(section.getByRole('checkbox')).not.toBeChecked();
    await section.getByRole('checkbox').check();
    await approve.click();
    await expect(section.getByRole('status').filter({hasText:'Connection approved.'})).toBeVisible();
    const post=calls.find(c=>c.method==='POST');
    assert.deepEqual(post,{url:'https://kodrik.test/nested/api/devices/pair/approve',method:'POST',body:{request_id:'550e8400-e29b-41d4-a716-446655440000',code:'123456'}});
    await expect(section.getByRole('textbox',{name:'Comparison code',exact:true})).toHaveValue('');
    await section.getByRole('button',{name:'Revoke access',exact:true}).click();
    await expect(section.getByText('No registered devices.',{exact:true})).toBeVisible();
    assert.equal(calls.filter(c=>c.method==='DELETE').length,1);
    assert.equal(calls.find(c=>c.method==='DELETE').url,'https://kodrik.test/nested/api/devices/device-1');
    await page.evaluate(()=>app.unmount());
    assert.equal(calls.filter(c=>c.method==='DELETE').length,1);
  } finally {await browser.close()}
});

test('invited profile never mounts device controls or requests owner device APIs',async()=>{
  const {page,browser,mount}=await componentPage('ProfilePage');
  try {
    const calls=[];
    await page.route('**/api/profile',r=>r.fulfill({json:{name:'Guest',owner:false,telegram_id:1,password_set:false}}));
    await page.route('**/api/devices**',r=>{calls.push(r.request().url());return r.fulfill({status:403})});
    await mount();
    await expect(page.getByRole('textbox',{name:'Name',exact:true})).toHaveValue('Guest');
    assert.equal(await page.getByRole('region',{name:'Device connections',exact:true}).count(),0);
    assert.deepEqual(calls,[]);
  } finally {await browser.close()}
});

test('device API failures stay visible and failed revoke retains the registered device',async()=>{
  const {page,browser,mount}=await componentPage('DeviceConnections');
  try {
    let failLoad=true;
    await page.route('**/api/devices**',r=>r.fulfill(r.request().method()==='DELETE'||failLoad?{status:503}:{json:[{id:'one',label:'Laptop',expires_at:2000000000}]}));
    await mount();
    await expect(page.getByRole('alert')).toBeVisible();
    failLoad=false;
    await page.getByRole('button',{name:'Refresh',exact:true}).click();
    await expect(page.getByText('Laptop',{exact:true})).toBeVisible();
    await page.getByRole('button',{name:'Revoke access',exact:true}).click();
    await expect(page.getByRole('alert')).toBeVisible();
    await expect(page.getByText('Laptop',{exact:true})).toBeVisible();
  } finally {await browser.close()}
});
}
