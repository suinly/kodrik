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

test('model picker loads lazily, searches, preserves unavailable current model and only saves explicit choices',async()=>{
  const {page,browser,mount}=await picker();
  try {
    let lists=0;const saves=[];
    await page.route('**/api/chat/models',r=>{lists++;return r.fulfill({json:{models:['alpha','beta'],default_model:'alpha'}})});
    await page.route('**/api/chat/sessions/session-one/model',r=>{saves.push(r.request().postDataJSON());return r.fulfill({status:204})});
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await expect(trigger).toContainText('retired-model');
    assert.equal(lists,0);assert.deepEqual(saves,[]);
    await trigger.click();
    await expect(page.getByRole('button',{name:'alpha',exact:true})).toBeVisible();
    await expect(trigger).toContainText('retired-model');
    assert.equal(lists,1);assert.deepEqual(saves,[]);
    await page.getByRole('textbox',{name:'Search models',exact:true}).fill('BETA');
    await expect(page.getByRole('button',{name:'alpha',exact:true})).toHaveCount(0);
    await page.getByRole('button',{name:'beta',exact:true}).click();
    await expect.poll(()=>saves).toEqual([{model:'beta'}]);
    await expect.poll(()=>page.evaluate(()=>window.refreshes)).toBe(1);
    await page.evaluate(()=>{props.model='beta';props.modelOverride='beta'});
    await trigger.click();
    await page.getByRole('button',{name:'Use default · alpha',exact:true}).click();
    await expect.poll(()=>saves).toEqual([{model:'beta'},{model:null}]);
    await expect.poll(()=>page.evaluate(()=>window.refreshes)).toBe(2);
  } finally {await browser.close()}
});

test('model picker retries failed lists and keeps selection on failed save; saving and external locks block choices',async()=>{
  const {page,browser,mount}=await picker();
  let release;
  try {
    let failList=true;const saves=[];
    await page.route('**/api/chat/models',r=>r.fulfill(failList?{status:503}:{json:{models:['alpha'],default_model:null}}));
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
    const alpha=page.getByRole('button',{name:'alpha',exact:true});
    await alpha.click();
    await expect(trigger).toBeDisabled();
    await expect(alpha).toBeDisabled();
    assert.equal(await page.evaluate(()=>window.saving),true);
    release();release=undefined;
    await expect(page.getByRole('alert')).toContainText('Model unavailable');
    await expect(trigger).toBeEnabled();
    await expect(trigger).toContainText('retired-model');
    assert.equal(await page.evaluate(()=>window.refreshes||0),0);
    await page.evaluate(()=>{props.disabled=true});
    await expect(trigger).toBeDisabled();
    await expect(alpha).toBeDisabled();
    assert.deepEqual(saves,[{model:'alpha'}]);
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
    await page.getByRole('button',{name:'Use default',exact:true}).click();
    await expect.poll(()=>saves).toEqual([{model:null}]);
    await expect.poll(()=>page.evaluate(()=>window.refreshes)).toBe(1);
  } finally {await browser.close()}
});

test('new chat shows its default model before opening the picker without provider prefixes',async()=>{
  const {page,browser,mount}=await picker({sessionId:null,model:null,modelOverride:null});
  try {
    await page.route('**/api/chat/models',r=>r.fulfill({json:{models:['anthropic/claude-sonnet:free'],default_model:'openai:anthropic/claude-sonnet:free'}}));
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await expect(trigger).toHaveText('anthropic/claude-sonnet:free');
    await expect(trigger).toHaveAttribute('title','Default · anthropic/claude-sonnet:free');
    await trigger.click();
    await expect(page.getByRole('button',{name:'Use default · anthropic/claude-sonnet:free',exact:true})).toBeVisible();
    await expect(page.getByRole('button',{name:'anthropic/claude-sonnet:free',exact:true})).toBeVisible();
  } finally {await browser.close()}
});

test('model labels hide the provider but save the full model ID',async()=>{
  const {page,browser,mount}=await picker({model:'openai:vendor/old',modelOverride:null});
  try {
    const saves=[];
    await page.route('**/api/chat/models',r=>r.fulfill({json:{models:['vendor/new'],default_model:'openai:vendor/old'}}));
    await page.route('**/api/chat/sessions/session-one/model',r=>{saves.push(r.request().postDataJSON());return r.fulfill({status:204})});
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await expect(trigger).toHaveText('vendor/old');
    await trigger.click();
    await page.getByRole('textbox',{name:'Search models',exact:true}).fill('vendor/');
    await page.getByRole('button',{name:'vendor/new',exact:true}).click();
    await expect.poll(()=>saves).toEqual([{model:'vendor/new'}]);
  } finally {await browser.close()}
});

test('draft model selection stays local, updates the trigger and can reset to default',async()=>{
  const {page,browser,mount}=await picker({sessionId:null,model:null,modelOverride:null});
  try {
    const saves=[];
    await page.route('**/api/chat/models',r=>r.fulfill({json:{models:['alpha','beta'],default_model:'alpha'}}));
    await page.route('**/api/chat/sessions/**',r=>{saves.push(r.request().url());return r.fulfill({status:500})});
    await page.addScriptTag({content:`props['onUpdate:modelOverride']=value=>{props.modelOverride=value}`});
    await mount();
    const trigger=page.getByRole('button',{name:'Conversation model',exact:true});
    await trigger.click();
    await page.getByRole('button',{name:'beta',exact:true}).click();
    await expect(trigger).toContainText('beta');
    await trigger.click();
    await page.getByRole('button',{name:'Use default · alpha',exact:true}).click();
    await expect(trigger).not.toContainText('beta');
    assert.deepEqual(saves,[]);
    assert.equal(await page.evaluate(()=>window.refreshes||0),0);
  } finally {await browser.close()}
});

test('existing conversation keeps its composer next to the model picker; subagents remain read only',async()=>{
  const {page,browser,mount}=await componentPage('ChatConversation',{
    conversation:{id:'session-one',busy:false},draftState:{text:'',files:[],missingFiles:[],revision:0},blocked:false,active:false,
  });
  try {
    await page.route('**/api/chat/sessions/session-one/run',r=>r.fulfill({json:{
      type:'snapshot',session_id:'session-one',revision:1,busy:false,messages:[],queued:[],live:{current:null,interrupted:null},blocked_reason:null,plan:null,
    }}));
    await page.route('**/api/chat/sessions/session-one/events',r=>r.fulfill({body:''}));
    await page.addScriptTag({content:`
      app.component('ChatModelPicker',{render:()=>VueForTests.h('button',{'aria-label':'Conversation model'})});
      app.component('UChatPrompt',{setup:(_,{slots})=>()=>VueForTests.h('div',[slots.header?.(),slots.body?.(),slots.footer?.()])});
      app.component('UEditor',{render:()=>VueForTests.h('div',{role:'textbox','aria-label':'Message',contenteditable:true})});
    `});
    await mount();
    await expect(page.getByRole('button',{name:'Conversation model',exact:true})).toBeVisible();
    await expect(page.getByRole('textbox',{name:'Message',exact:true})).toBeVisible();
    await page.evaluate(()=>{props.conversation.subagent=true});
    await expect(page.getByRole('textbox',{name:'Message',exact:true})).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Conversation model',exact:true})).toHaveCount(0);
    await page.evaluate(()=>app.unmount());
  } finally {await browser.close()}
});
