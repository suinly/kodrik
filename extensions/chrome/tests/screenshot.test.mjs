import {test} from 'node:test';
import assert from 'node:assert/strict';
import {executeBrowserCommand} from '../automation.mjs';

function fixture(t) {
  const previous=globalThis.chrome;
  t.after(()=>{globalThis.chrome=previous;});
  const state={tab:{id:7,windowId:1,active:true,url:'https://example.com/',title:'Example'},captures:0,activations:[],updates:[],documentId:'doc',data:'data:image/png;base64,AAAA'};
  const event=list=>({addListener:fn=>list.push(fn),removeListener:fn=>list.splice(list.indexOf(fn),1)});
  globalThis.chrome={tabs:{get:async()=>({...state.tab}),query:async()=>state.tab.active?[{...state.tab}]:[],onActivated:event(state.activations),onUpdated:event(state.updates),captureVisibleTab:async()=>{state.captures++;await state.during?.();return state.data;}},scripting:{executeScript:async()=>[{documentId:state.documentId,result:state.tab.url}]}};
  return state;
}

test('screenshot returns image metadata and requires live preflight',async t=>{
  const state=fixture(t);
  await assert.rejects(executeBrowserCommand(7,{action:'screenshot'},async()=>false),/cancel/i);
  assert.equal(state.captures,0);
  const result=await executeBrowserCommand(7,{action:'screenshot'},async()=>true);
  assert.equal(result.data_url,state.data);
  assert.equal(result.url,'https://example.com/');
  assert.ok(Number.isFinite(Date.parse(result.captured_at)));
  assert.equal(state.activations.length,0);
  assert.equal(state.updates.length,0);
});

test('screenshot rejects inactive, forbidden, switched and navigated tabs',async t=>{
  const state=fixture(t);
  state.tab.active=false;
  await assert.rejects(executeBrowserCommand(7,{action:'screenshot'},async()=>true),/active/i);
  state.tab.active=true;
  await assert.rejects(executeBrowserCommand(7,{action:'screenshot'},async()=>true,{forbiddenOrigin:'https://example.com'}),/Forbidden/);
  state.during=()=>{for(const fn of state.activations) fn({windowId:1,tabId:9});};
  await assert.rejects(executeBrowserCommand(7,{action:'screenshot'},async()=>true),/changed/i);
  state.during=()=>{state.documentId='new';};
  await assert.rejects(executeBrowserCommand(7,{action:'screenshot'},async()=>true),/changed/i);
  assert.equal(state.activations.length,0);
});

test('screenshot rejects over-budget images',async t=>{
  const state=fixture(t);
  state.data='data:image/png;base64,'+'a'.repeat(27962032);
  await assert.rejects(executeBrowserCommand(7,{action:'screenshot'},async()=>true),/large|20 MiB/i);
});
