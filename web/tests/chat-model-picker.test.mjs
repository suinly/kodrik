import {test} from 'node:test';
import assert from 'node:assert/strict';
import {expect} from '@playwright/test';
import {componentPage} from './devices.test.mjs';

async function picker(props={}) {
  const fixture=await componentPage('ChatModelPicker',{sessionId:'session-one',model:'retired-model',modelOverride:'retired-model',disabled:false,...props});
  await fixture.page.addScriptTag({content:`
    app.component('UPopover',{props:['open'],emits:['update:open'],setup:(p,{slots,emit})=>{
      return ()=>VueForTests.h('div',[
        VueForTests.h('div',{onClick:()=>emit('update:open',!p.open)},slots.default?.()),
        p.open?slots.content?.():null
      ]);
    }});
    props.refresh=async()=>{window.refreshes=(window.refreshes||0)+1};
    props.onSaving=value=>window.saving=value;
  `});
  return fixture;
}

test('model picker loads roles lazily, preserves a legacy override and only saves explicit choices',async()=>{
  const {page,browser,mount}=await picker();
  try {
    let lists=0;const saves=[];
    await page.route('**/api/chat/models',r=>{lists++;return r.fulfill({json:{models:['fast','smart','vendor/advertised'],default_model:'vendor/default'}})});
    await page.route('**/api/chat/sessions/session-one/model',r=>{saves.push(r.request().postDataJSON());return r.fulfill({status:204})});
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await expect(trigger).toContainText('retired-model');
    assert.equal(lists,0);assert.deepEqual(saves,[]);
    await trigger.click();
    await expect(page.getByRole('button',{name:'Fast',exact:true})).toBeVisible();
    await expect(trigger).toContainText('retired-model');
    assert.equal(lists,1);assert.deepEqual(saves,[]);
    await expect(page.getByRole('button',{name:'vendor/advertised',exact:true})).toHaveCount(0);
    await page.getByRole('button',{name:'Smart',exact:true}).click();
    await expect.poll(()=>saves).toEqual([{model:'smart'}]);
    await expect.poll(()=>page.evaluate(()=>window.refreshes)).toBe(1);
    await page.evaluate(()=>{props.model='vendor/resolved';props.modelOverride='smart'});
    await trigger.click();
    await page.getByRole('button',{name:'Auto',exact:true}).click();
    await expect.poll(()=>saves).toEqual([{model:'smart'},{model:null}]);
    await expect.poll(()=>page.evaluate(()=>window.refreshes)).toBe(2);
  } finally {await browser.close()}
});

test('configured model IDs accompany localized roles without changing the saved role',async()=>{
  const {page,browser,mount}=await picker({modelOverride:'deep'});
  try {
    const saves=[];
    const visionId='vendor/'+'unbroken-model-id'.repeat(8);
    await page.route('**/api/chat/models',r=>r.fulfill({json:{
      models:['deep','vision','vendor/advertised'],default_model:null,
      model_names:{deep:'vendor/reasoner',vision:visionId},
    }}));
    await page.route('**/api/chat/sessions/session-one/model',r=>{saves.push(r.request().postDataJSON());return r.fulfill({status:204})});
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await expect(trigger).toHaveText('Deep');
    await trigger.click();
    await expect(page.getByRole('button',{name:`Vision ${visionId}`,exact:true})).toBeVisible();
    await expect(page.getByRole('button',{name:'Deep vendor/reasoner',exact:true})).toHaveAttribute('aria-pressed','true');
    await expect(trigger).toHaveText('Deep');
    await expect(page.getByRole('button',{name:'Fast',exact:true})).toHaveCount(0);
    await expect(page.getByRole('button',{name:'vendor/advertised',exact:true})).toHaveCount(0);
    await page.evaluate(()=>{
      Object.defineProperty(navigator,'language',{configurable:true,value:'ru-RU'});
      window.dispatchEvent(new Event('languagechange'));
    });
    await expect(page.getByRole('button',{name:'Глубокая vendor/reasoner',exact:true})).toHaveAttribute('aria-pressed','true');
    await page.getByRole('button',{name:`Зрение ${visionId}`,exact:true}).click();
    await expect.poll(()=>saves).toEqual([{model:'vision'}]);
  } finally {await browser.close()}
});

test('model picker retries failed lists and keeps selection on failed save; saving and external locks block choices',async()=>{
  const {page,browser,mount}=await picker();
  let release;
  try {
    let failList=true;const saves=[];
    await page.route('**/api/chat/models',r=>r.fulfill(failList?{status:503}:{json:{models:['fast'],default_model:null}}));
    await page.route('**/api/chat/sessions/session-one/model',async r=>{
      saves.push(r.request().postDataJSON());
      await new Promise(resolve=>{release=resolve});
      await r.fulfill({status:400,json:{error:'Model unavailable'}});
    });
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await trigger.click();
    await expect(page.getByRole('alert')).toBeVisible();
    failList=false;
    await page.getByRole('button',{name:'Retry',exact:true}).click();
    const fast=page.getByRole('button',{name:'Fast',exact:true});
    await expect(page.getByRole('button',{name:'Smart',exact:true})).toHaveCount(0);
    await fast.click();
    await expect(trigger).toBeDisabled();
    await expect(fast).toBeDisabled();
    assert.equal(await page.evaluate(()=>window.saving),true);
    release();release=undefined;
    await expect(page.getByRole('alert')).toContainText('Model unavailable');
    await expect(trigger).toBeEnabled();
    await expect(trigger).toContainText('retired-model');
    assert.equal(await page.evaluate(()=>window.refreshes||0),0);
    await page.evaluate(()=>{props.disabled=true});
    await expect(trigger).toBeDisabled();
    await expect(fast).toBeDisabled();
    assert.deepEqual(saves,[{model:'fast'}]);
  } finally {release?.();await browser.close()}
});

test('reset to default remains available when the model list fails',async()=>{
  const {page,browser,mount}=await picker();
  try {
    const saves=[];
    await page.route('**/api/chat/models',r=>r.fulfill({status:503}));
    await page.route('**/api/chat/sessions/session-one/model',r=>{saves.push(r.request().postDataJSON());return r.fulfill({status:204})});
    await mount();
    await page.getByRole('button',{name:'Conversation model',exact:true}).click();
    await expect(page.getByRole('alert')).toBeVisible();
    await page.getByRole('button',{name:'Auto',exact:true}).click();
    await expect.poll(()=>saves).toEqual([{model:null}]);
    await expect.poll(()=>page.evaluate(()=>window.refreshes)).toBe(1);
  } finally {await browser.close()}
});

test('auto shows the actual default identity without offering it as an override',async()=>{
  const {page,browser,mount}=await picker({sessionId:null,model:null,modelOverride:null});
  try {
    await page.route('**/api/chat/models',r=>r.fulfill({json:{models:['fast'],default_model:'openai:vendor/default'}}));
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await expect(trigger).toHaveText('Auto');
    await expect(trigger).toHaveAttribute('title','Auto · vendor/default');
    await trigger.click();
    await expect(page.getByRole('button',{name:'Auto',exact:true})).toHaveAttribute('aria-pressed','true');
    await expect(page.getByRole('button',{name:'Fast',exact:true})).toBeVisible();
    await expect(page.getByRole('button',{name:'Smart',exact:true})).toHaveCount(0);
  } finally {await browser.close()}
});

test('draft model selection stays local, updates the trigger and can reset to default',async()=>{
  const {page,browser,mount}=await picker({sessionId:null,model:null,modelOverride:null});
  try {
    const saves=[];
    await page.route('**/api/chat/models',r=>r.fulfill({json:{models:['fast','smart','vendor/advertised'],default_model:'vendor/default'}}));
    await page.route('**/api/chat/sessions/**',r=>{saves.push(r.request().url());return r.fulfill({status:500})});
    await page.addScriptTag({content:`props['onUpdate:modelOverride']=value=>{props.modelOverride=value}`});
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await trigger.click();
    await page.getByRole('button',{name:'Smart',exact:true}).click();
    await expect(trigger).toContainText('Smart');
    await trigger.click();
    await page.getByRole('button',{name:'Auto',exact:true}).click();
    await expect(trigger).toHaveText('Auto');
    assert.deepEqual(saves,[]);
    assert.equal(await page.evaluate(()=>window.refreshes||0),0);
  } finally {await browser.close()}
});

for (const [state,run,pickerDisabled] of [
  ['idle',{},false],
  ['running',{busy:true},false],
  ['queued',{queued:[{id:'queued-one',text:'Follow-up'}]},false],
  ['paused queue',{queued:[{id:'queued-one',text:'Follow-up'}],blocked_reason:'Stopped by user'},false],
  ['cancelling',{busy:true,cancel_requested:true},true],
]) test(`existing conversation model picker: ${state}`,async()=>{
  const {page,browser,mount}=await componentPage('ChatConversation',{
    conversation:{id:'session-one',busy:false},draftState:{text:'',files:[],missingFiles:[],revision:0},blocked:false,active:false,
  });
  try {
    await page.route('**/api/chat/sessions/session-one/run',r=>r.fulfill({json:{
      type:'snapshot',session_id:'session-one',run_id:null,revision:1,busy:false,cancel_requested:false,phase:null,
      messages:[],queued:[],live:{current:null,interrupted:null},blocked_reason:null,plan:null,model:'alpha',...run,
    }}));
    await page.route('**/api/chat/sessions/session-one/events',r=>r.fulfill({body:''}));
    await page.addScriptTag({content:`
      app.component('ChatModelPicker',{props:['disabled','model'],setup:p=>()=>VueForTests.h('button',{'aria-label':'Conversation model',disabled:p.disabled},p.model)});
      app.component('UChatPrompt',{setup:(_,{slots})=>()=>VueForTests.h('div',[slots.header?.(),slots.body?.(),slots.footer?.()])});
      app.component('UEditor',{render:()=>VueForTests.h('div',{role:'textbox','aria-label':'Message',contenteditable:true})});
    `});
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await expect(trigger).toHaveText('alpha');
    if(pickerDisabled) await expect(trigger).toBeDisabled();
    else await expect(trigger).toBeEnabled();
    await expect(page.getByRole('textbox',{name:'Message',exact:true})).toBeVisible();
    await page.evaluate(()=>{props.blocked=true});
    await expect(trigger).toBeDisabled();
    await page.evaluate(()=>{props.conversation.subagent=true});
    await expect(page.getByRole('textbox',{name:'Message',exact:true})).toHaveCount(0);
    await expect(trigger).toHaveCount(0);
    await page.evaluate(()=>app.unmount());
  } finally {await browser.close()}
});
