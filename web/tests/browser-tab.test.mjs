import {test} from 'node:test';
import assert from 'node:assert/strict';
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
test('capture matches exact request and creates bounded text attachment; disabled capture is empty',async()=>{
  const {bridge,deliver,parent,listeners}=fixture();
  deliver({type:'kodrik-tab-init',channel:'x',enabled:true});
  const pending=bridge.capture();
  const request=parent.sent.at(-1).message;
  deliver({type:'kodrik-tab-result',channel:'other',id:request.id,context:'bad'});
  deliver({type:'kodrik-tab-result',channel:'x',id:'other',context:'bad'});
  deliver({type:'kodrik-tab-result',channel:'x',id:request.id,context:'UNTRUSTED page'});
  const file=await pending;
  assert.equal(file.name,'browser-tab.txt');assert.equal(file.type,'text/plain');assert.equal(await file.text(),'UNTRUSTED page');
  deliver({type:'kodrik-tab-init',channel:'x',enabled:false});
  assert.equal(await bridge.capture(),null);
  bridge.dispose();assert.equal(listeners.size,0);
});
test('capture reports errors, invalid payload and timeout instead of sending without context',async()=>{
  for(const result of [{error:'Permission denied'},{context:'я'.repeat(100001)},{context:42}]) {
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
