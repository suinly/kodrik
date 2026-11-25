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
  const child=name==='ProfilePage'?'DeviceConnections':null;
  const result=await build({stdin:{contents:`import * as VueForTests from 'vue';import {createApp,h,reactive} from 'vue';import Component from './app/components/${name}.vue';${child?`import Child from './app/components/${child}.vue';`:''}window.VueForTests=VueForTests;window.props=reactive(${JSON.stringify(props)});window.app=createApp({render:()=>h(Component,window.props)});${child?`app.component('${child}',Child);`:''}`,resolveDir:root},bundle:true,write:false,format:'iife',plugins:[{
    name:'vue-test',setup(builder){builder.onLoad({filter:/\.vue$/},async({path})=>{
      const {descriptor}=parse(await readFile(path,'utf8'),{filename:path});
      const compiled=compileScript(descriptor,{id:path,inlineTemplate:true});
      return {contents:`import {ref,shallowRef,reactive,computed,onMounted,onUnmounted,onBeforeUnmount,watch,toRef,useTemplateRef,nextTick} from 'vue';${compiled.content}`,loader:'ts'};
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
      app.component('UTextarea',{props:['modelValue'],emits:['update:modelValue'],setup:(p,{emit,attrs})=>()=>h('textarea',{...attrs,value:p.modelValue,onInput:e=>emit('update:modelValue',e.target.value)})});
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

test('invited profile shows device controls without owner password controls',async()=>{
  const {page,browser,mount}=await componentPage('ProfilePage');
  try {
    const calls=[];
    await page.route('**/api/profile',r=>r.fulfill({json:{name:'Guest',owner:false,telegram_id:1,password_set:false}}));
    await page.route('**/api/devices**',r=>{calls.push(r.request().url());return r.fulfill({json:[]})});
    await mount();
    await expect(page.getByRole('textbox',{name:'Name',exact:true})).toHaveValue('Guest');
    await expect(page.getByRole('region',{name:'Device connections',exact:true})).toBeVisible();
    await expect(page.getByText('No registered devices.',{exact:true})).toBeVisible();
    assert.deepEqual(calls,['https://kodrik.test/nested/api/devices']);
    assert.equal(await page.locator('input[name="new_password"]').count(),0);
  } finally {await browser.close()}
});

const pairingUuid='550e8400-e29b-41d4-a716-446655440000';

for(const hash of ['#device-connections','']) test(`linked pairing waits for the profile and requires an exact terminal code ${hash?'with the anchor':'after login'}`,async()=>{
  const {page,browser,mount}=await componentPage('ProfilePage');
  try {
    // Login preserves the query; do not rely on the fragment surviving.
    await page.evaluate(query=>history.replaceState(null,'',query),`?pair_request=${pairingUuid}${hash}`);
    await page.addStyleTag({content:'#app { height: 100vh; overflow-y: auto; } form:has(input[name="name"]) { min-height: 200vh; }'});
    let releaseProfile;
    const profileReady=new Promise(resolve=>{releaseProfile=resolve});
    await page.route('**/api/profile',async r=>{
      await profileReady;
      return r.fulfill({json:{name:'Owner',owner:true,telegram_id:null,password_set:true}});
    });
    const posts=[];
    await page.route('**/api/devices**',r=>{
      if(r.request().method()!=='POST') return r.fulfill({json:[]});
      posts.push({url:r.request().url(),body:r.request().postDataJSON()});
      return r.fulfill({status:204});
    });
    const section=page.getByRole('region',{name:'Device connections',exact:true});
    await mount();
    await expect(section).toHaveCount(0);
    releaseProfile();
    const input=section.getByRole('textbox',{name:'Comparison code',exact:true});
    const approve=section.getByRole('button',{name:'Approve connection',exact:true});
    await expect(input).toBeFocused();
    await expect(input).toBeInViewport();
    await expect(section).toHaveAttribute('id','device-connections');
    await expect(section.getByRole('textbox')).toHaveCount(1);
    await expect(section.getByRole('checkbox')).toHaveCount(0);
    await expect(section.getByText(/Only approve a request you started locally/)).toBeVisible();
    await expect(approve).toBeDisabled();
    for(const value of ['Ab12Cd3','Ab12Cd345','Ab12Cd3!','Ab12Cd3é',' Ab12Cd3','Ab12Cd34 ']) {
      await input.fill(value);
      await expect(approve).toBeDisabled();
    }
    await input.fill('Ab12Cd34');
    await expect(approve).toBeEnabled();
    assert.deepEqual(posts,[]);
    await approve.click();
    await expect(section.getByText('Connection approved.',{exact:true})).toBeVisible();
    assert.deepEqual(posts,[{url:'https://kodrik.test/nested/api/devices/pair/approve',body:{request_id:pairingUuid,code:'Ab12Cd34'}}]);
    await expect(input).toHaveValue('');
    assert.equal(new URL(page.url()).search,`?pair_request=${pairingUuid}`);
  } finally {await browser.close()}
});

for(const query of ['pair_request=bogus',`pair_request=${pairingUuid}%0A`,`pair_request=${pairingUuid}&pair_request=${pairingUuid}`]) test(`invalid pairing query falls back to the manual form: ${query}`,async()=>{
  const {page,browser,mount}=await componentPage('DeviceConnections');
  try {
    await page.route('**/api/devices',r=>r.fulfill({json:[]}));
    await page.evaluate(query=>history.replaceState(null,'',`?${query}#device-connections`),query);
    await mount();
    await expect(page.getByRole('textbox',{name:'Pairing request UUID',exact:true})).toHaveValue('');
    await expect(page.getByRole('checkbox')).not.toBeChecked();
    await expect(page.getByRole('alert')).toContainText('Invalid pairing link');
    await expect(page.getByRole('button',{name:'Approve connection',exact:true})).toBeDisabled();
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
