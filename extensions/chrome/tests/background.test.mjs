import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';

test('action opens a unique tab-scoped panel, never a global panel; closed tabs release their chat mapping',async()=>{
  let action,removed,created;let setting=false;const options=[],opened=[],deleted=[];
  const chrome={
    sidePanel:{setPanelBehavior:async()=>{},setOptions:async value=>{setting=true;options.push(JSON.parse(JSON.stringify(value)));await Promise.resolve();setting=false},open:async value=>{assert.equal(setting,false,'opening must not await panel setup inside the gesture');opened.push(JSON.parse(JSON.stringify(value)))}},
    action:{onClicked:{addListener:fn=>action=fn}},
    tabs:{query:async()=>[{id:11},{id:22}],onCreated:{addListener:fn=>created=fn},onRemoved:{addListener:fn=>removed=fn}},
    storage:{session:{remove:async key=>deleted.push(key)}}
  };
  vm.runInNewContext(await readFile(new URL('../background.js',import.meta.url),'utf8'),{chrome,console});
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(typeof action,'function');
  await action({id:11});await action({id:22});
  assert.deepEqual(options.filter(value=>value.tabId),[
    {tabId:11,path:'sidepanel.html?tab=11',enabled:true},
    {tabId:22,path:'sidepanel.html?tab=22',enabled:true},
  ]);
  assert.deepEqual(opened,[{tabId:11},{tabId:22}]);
  await removed(11);assert.deepEqual(deleted,['tab-chat:11']);
});
