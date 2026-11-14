import {test} from 'node:test';
import assert from 'node:assert/strict';
const pageContext={url:'https://example.com/',title:'Example',text:'Page content',source:'page',captured_at:'2026-11-13T17:00:00.000Z',truncated:false};

import {createTabBridge} from '../app/utils/browser-tab.ts';

function fixture() {
  const parent={sent:[],postMessage(message,origin){this.sent.push({message,origin})}};
  const listeners=new Set();
  const win={parent,location:{search:'?browser_sidebar=1'},addEventListener(_type,fn){listeners.add(fn)},removeEventListener(_type,fn){listeners.delete(fn)}};
  const bridge=createTabBridge(win,20);
  const deliver=(data,origin='chrome-extension://iiajbdommloibgimhfmoojnhknbaklih',source=parent)=>{for(const fn of listeners) fn({data,origin,source})};
  return {bridge,deliver,parent,listeners};
}
test('bridge rejects spoofed origins/sources and fails closed without handshake',async()=>{
  const {bridge,deliver}=fixture();
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true},'https://evil.example');
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true},'chrome-extension://iiajbdommloibgimhfmoojnhknbaklih',{});
  await assert.rejects(()=>bridge.capture(),/подключ/);
  bridge.dispose();
});
test('capture matches exact request and returns structured browser context; disabled capture is empty',async()=>{
  const {bridge,deliver,parent,listeners}=fixture();
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
  const pending=bridge.capture();
  const request=parent.sent.at(-1).message;
  deliver({type:'kodrik-tab-result',channel:'other',id:request.id,context:'bad'});
  deliver({type:'kodrik-tab-result',channel:'x',id:'other',context:'bad'});
  deliver({type:'kodrik-tab-result',channel:'x',id:request.id,context:pageContext});
  const context=await pending;
  assert.deepEqual(context,pageContext);
  deliver({type:'kodrik-tab-init',channel:'x',enabled:false});
  assert.equal(await bridge.capture(),null);
  bridge.dispose();assert.equal(listeners.size,0);
});
test('capture reports errors, invalid payload and timeout instead of sending without context',async()=>{
  for(const result of [{error:'Permission denied'},{context:'я'.repeat(100001)},{context:42},{context:{...pageContext,url:'javascript:alert(1)'}},{context:{...pageContext,text:'я'.repeat(100001)}}]) {
    const {bridge,deliver,parent}=fixture();
    deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
    const pending=bridge.capture();
    deliver({type:'kodrik-tab-result',channel:'x',id:parent.sent.at(-1).message.id,...result});
    await assert.rejects(()=>pending);
    bridge.dispose();
  }
  const {bridge,deliver}=fixture();
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
  await assert.rejects(()=>bridge.capture(),/время/);
  bridge.dispose();
});
test('unknown extension cannot initialize a bridge',async()=>{
  const {bridge,deliver}=fixture();
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true},'chrome-extension://unknown');
  await assert.rejects(()=>bridge.capture(),/подключ/);
  bridge.dispose();
});

test('bridge exposes tab metadata and forwards explicit preview, toggle and settings actions',async()=>{
  const {bridge,deliver,parent}=fixture();
  let state;
  bridge.subscribe(value=>{state=value});
  deliver({type:'kodrik-tab-init',channel:'x',enabled:false,tab:{title:'Article',url:'https://example.com/'}});
  assert.equal(state.title,'Article');assert.equal(state.enabled,false);
  bridge.settings();assert.equal(parent.sent.at(-1).message.type,'kodrik-tab-settings');
  bridge.toggle(true);assert.equal(parent.sent.at(-1).message.enabled,true);
  const preview=bridge.preview();
  const message=parent.sent.at(-1).message;
  assert.equal(message.type,'kodrik-tab-preview');
  deliver({type:'kodrik-tab-result',channel:'x',id:message.id,context:pageContext});
  assert.deepEqual(await preview,pageContext);
  bridge.dispose();
});

test('bridge decodes document attachments separately from bounded text context',async()=>{
  const {bridge,deliver,parent}=fixture();
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
  const pending=bridge.capture();
  deliver({type:'kodrik-tab-result',channel:'x',id:parent.sent.at(-1).message.id,context:pageContext,document:{name:'report.pdf',mime:'application/pdf',base64:btoa('%PDF-1.7\ncontent')}});
  const capture=await pending;
  assert.equal(capture.document?.name,'report.pdf');
  assert.equal(await capture.document.text(),'%PDF-1.7\ncontent');
  assert.equal(capture.text,'Page content');
  bridge.dispose();
});
test('bridge rejects invalid document attachment instead of silently dropping it',async()=>{
  for(const document of [{name:'bad.pdf',mime:'application/pdf',base64:btoa('not a PDF')},{name:'bad.html',mime:'text/html',base64:btoa('<html>')},{name:'bad.pdf',mime:'application/pdf',base64:'!!!'}]) {
    const {bridge,deliver,parent}=fixture();
    deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
    const pending=bridge.capture();
    deliver({type:'kodrik-tab-result',channel:'x',id:parent.sent.at(-1).message.id,context:pageContext,document});
    await assert.rejects(()=>pending);bridge.dispose();
  }
});

test('browser commands use authenticated channel and return bounded results without recasting page context',async()=>{
  const {bridge,deliver,parent}=fixture();
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
  const pending=bridge.command({action:'snapshot'});
  const request=parent.sent.at(-1).message;
  assert.equal(request.type,'kodrik-browser-command');
  assert.deepEqual(request.command,{action:'snapshot'});
  deliver({type:'kodrik-browser-result',channel:'wrong',id:request.id,result:{bad:true}});
  deliver({type:'kodrik-browser-result',channel:'x',id:request.id,result:{snapshot_id:'s',elements:[]}});
  assert.deepEqual(await pending,{snapshot_id:'s',elements:[]});
  const rejected=bridge.command({action:'click',snapshot_id:'s',element_id:'e'});
  deliver({type:'kodrik-browser-result',channel:'x',id:parent.sent.at(-1).message.id,error:'User declined'});
  await assert.rejects(()=>rejected,/declined/);
  const oversized=bridge.command({action:'snapshot'});
  deliver({type:'kodrik-browser-result',channel:'x',id:parent.sent.at(-1).message.id,result:{text:'a'.repeat(100001)}});
  await assert.rejects(()=>oversized,/результат/i);
  bridge.dispose();
});

test('confirmation preflight checks current broker authority and disposal cancels extension work',async()=>{
  const {bridge,deliver,parent}=fixture();
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
  const pending=bridge.command({action:'click'},'broker-id',async()=>false);
  deliver({type:'kodrik-browser-check',channel:'x',id:'broker-id'});
  await new Promise(resolve=>setTimeout(resolve,0));
  assert.deepEqual(parent.sent.at(-1).message,{type:'kodrik-browser-check-result',channel:'x',id:'broker-id',allowed:false});
  const rejected=assert.rejects(()=>pending,/закрыт/i);
  bridge.dispose();await rejected;
  assert.equal(parent.sent.at(-1).message.type,'kodrik-browser-cancel');
});

test('browser request provenance requires a handshake with the trusted extension, not a URL flag',()=>{
  const {bridge,deliver}=fixture();
  assert.equal(bridge.connected(),false);
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true},'https://evil.test');
  assert.equal(bridge.connected(),false);
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
  assert.equal(bridge.connected(),true);
  bridge.dispose();
});
