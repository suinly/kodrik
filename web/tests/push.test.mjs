import {test} from 'node:test'
import assert from 'node:assert/strict'
import {readFile} from 'node:fs/promises'
import vm from 'node:vm'

async function worker(prefix='/kodrik/') {
  const handlers={},shown=[],opened=[],focused=[]
  const context=vm.createContext({URL,self:{
    registration:{scope:`https://example.com${prefix}`,showNotification:async(...args)=>shown.push(args)},
    addEventListener:(name,handler)=>{handlers[name]=handler},
    skipWaiting:async()=>{},
    clients:{claim:async()=>{},matchAll:async()=>[],openWindow:async url=>opened.push(url)},
  }})
  vm.runInContext(await readFile(new URL('../public/chat/push-sw.js',import.meta.url),'utf8'),context)
  async function fire(name,data) {
    let task
    handlers[name]({...data,waitUntil:value=>{task=value}})
    await task
  }
  return {context,shown,opened,focused,fire}
}

test('push displays generic text, a stable event tag and a prefix-aware conversation target',async()=>{
  for(const prefix of ['/','/kodrik/']) {
    const w=await worker(prefix)
    await w.fire('push',{data:{json:()=>({session_id:'telegram:main:7',event_id:'event-1',locale:'ru',text:'secret'})}})
    assert.equal(w.shown.length,1)
    const [title,options]=w.shown[0]
    assert.equal(title,'Кодрик')
    assert.equal(options.body,'Новое сообщение в Kodrik')
    assert.equal(options.tag,'kodrik-event-1')
    assert.equal(options.data.url,`https://example.com${prefix}app/chat/telegram%3Amain%3A7`)
    assert.ok(!JSON.stringify(w.shown).includes('secret'))
  }
})

test('approval push identifies required action without exposing tool arguments',async()=>{
  for(const [locale,body] of [['ru','Требуется подтверждение действия'],['en','Action approval required']]) {
    const w=await worker()
    await w.fire('push',{data:{json:()=>({session_id:'personal',event_id:'approval:request-1',locale,arguments:'secret command'})}})
    const [,options]=w.shown[0]
    assert.equal(options.body,body)
    assert.equal(options.tag,'kodrik-approval:request-1')
    assert.equal(options.data.url,'https://example.com/kodrik/app/chat/personal')
    assert.ok(!JSON.stringify(w.shown).includes('secret command'))
  }
})

test('notification click focuses/navigates an existing app window or opens the conversation',async()=>{
  const w=await worker()
  let closed=0
  await w.fire('notificationclick',{notification:{close:()=>closed++,data:{url:'https://example.com/kodrik/app/chat/one'}}})
  assert.equal(closed,1)
  assert.deepEqual(w.opened,['https://example.com/kodrik/app/chat/one'])
  const navigated=[]
  w.context.self.clients.matchAll=async()=>[{url:'https://example.com/kodrik/app/profile',navigate:async url=>navigated.push(url),focus:async()=>w.focused.push(true)}]
  await w.fire('notificationclick',{notification:{close(){},data:{url:'https://example.com/kodrik/app/chat/two'}}})
  assert.deepEqual(navigated,['https://example.com/kodrik/app/chat/two'])
  assert.deepEqual(w.focused,[true])
})

test('malformed push still shows a generic notification; click cannot leave the app origin/scope',async()=>{
  const w=await worker()
  await w.fire('push',{data:{json:()=>{throw new Error('invalid')}}})
  assert.equal(w.shown.length,1)
  await w.fire('push',{data:{json:()=>null}})
  assert.equal(w.shown.length,2)
  for(const url of ['https://evil.example/','https://example.com/other','javascript:alert(1)']) {
    await w.fire('notificationclick',{notification:{close(){},data:{url}}})
  }
  assert.deepEqual(w.opened,Array(3).fill('https://example.com/kodrik/app/chat'))
})
