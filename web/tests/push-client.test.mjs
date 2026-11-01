import {test} from 'node:test'
import assert from 'node:assert/strict'
import '../app/utils/i18n.ts'

test('push requires a click to subscribe, restores existing subscriptions and clears foreground presence',async()=>{
  const originals=Object.fromEntries(['window','document','navigator','Notification','PushManager','localStorage','fetch'].map(key=>[key,Object.getOwnPropertyDescriptor(globalThis,key)]))
  const calls=[],stored=new Map()
  let subscribed=null,requests=0,permission='default',focused=true,failSave=false
  const key=Buffer.from(Array(65).fill(4)).toString('base64url')
  const registration={active:{},pushManager:{
    getSubscription:async()=>subscribed,
    subscribe:async options=>{
      assert.equal(permission,'granted')
      assert.equal(options.userVisibleOnly,true)
      requests++
      subscribed={endpoint:'https://fcm.googleapis.com/device',options:{applicationServerKey:Uint8Array.from(Array(65).fill(4)).buffer},toJSON:()=>({endpoint:'https://fcm.googleapis.com/device',keys:{p256dh:'public',auth:'auth'}}),unsubscribe:async()=>{subscribed=null;return true}}
      return subscribed
    },
  }}
  Object.defineProperties(globalThis,{
    window:{configurable:true,value:{isSecureContext:true,location:new URL('https://example.com/kodrik/app/chat/one')}},
    document:{configurable:true,value:{getElementById:()=>({textContent:'{"prefix":"/kodrik/"}'}),visibilityState:'visible',hasFocus:()=>focused}},
    navigator:{configurable:true,value:{serviceWorker:{register:async(url,options)=>{assert.equal(String(url),'https://example.com/kodrik/push-sw.js');assert.equal(options.scope,'/kodrik/');return registration},ready:Promise.resolve(registration)}}},
    Notification:{configurable:true,value:{get permission(){return permission},requestPermission:()=>{permission='granted';return Promise.resolve(permission)}}},
    PushManager:{configurable:true,value:class {}},
    localStorage:{configurable:true,value:{getItem:key=>stored.get(key),setItem:(key,value)=>stored.set(key,value)}},
    fetch:{configurable:true,value:async(url,options)=>{
      calls.push({url:String(url),method:options?.method??'GET',body:options?.body?JSON.parse(options.body):null})
      if(failSave && options?.method==='POST' && String(url).endsWith('/push')) return new Response(null,{status:503})
      return options?.method==='POST'||options?.method==='DELETE'?new Response(null,{status:204}):Response.json({public_key:key,endpoints:[]})
    }},
  })
  try {
    const p=await import('../app/utils/push.ts')
    await p.updatePushPresence('one')
    assert.equal(calls.at(-1)?.body?.session_id,'one','Presence must work before push setup')
    assert.equal(requests,0)
    await p.initializePush()
    assert.equal(requests,0)
    assert.equal(permission,'default')
    await p.enablePush()
    assert.equal(requests,1)
    assert.equal(p.pushEnabled.value,true)
    await p.updatePushPresence('one')
    assert.equal(calls.at(-1).body.session_id,'one')
    assert.equal(calls.at(-1).url,'https://example.com/kodrik/api/chat/push/presence')
    focused=false
    await p.updatePushPresence('one')
    assert.equal(calls.at(-1).body.session_id,null)
    const originalSubscription=subscribed
    subscribed={...originalSubscription,endpoint:'https://fcm.googleapis.com/new-device',toJSON:()=>({endpoint:'https://fcm.googleapis.com/new-device',keys:{p256dh:'public',auth:'auth'}})}
    focused=true
    await p.updatePushPresence('one')
    assert.equal(calls.findLast(call=>call.method==='POST' && call.url.endsWith('/push')).body.subscription.endpoint,'https://fcm.googleapis.com/new-device')
    assert.equal(calls.findLast(call=>call.url.endsWith('/presence')).body.endpoint,undefined)
    stored.set('kodrik.push.disabled','true')
    subscribed=null
    await p.updatePushPresence('one')
    assert.equal(p.pushEnabled.value,false)
    assert.equal(calls.at(-1).body.session_id,'one','Disabling notifications must not disable presence')
    stored.set('kodrik.push.disabled','false')
    subscribed=originalSubscription
    await p.updatePushPresence('one')
    assert.equal(p.pushEnabled.value,true)
    await p.disablePush()
    assert.equal(p.pushEnabled.value,false)
    assert.equal(subscribed,null)
    assert.ok(calls.some(call=>call.method==='DELETE'))
    failSave=true
    await p.enablePush()
    assert.equal(p.pushEnabled.value,false)
    assert.equal(subscribed,null)
    assert.ok(p.pushProblem.value)
    delete globalThis.PushManager
    delete globalThis.Notification
    const unsupported=await import('../app/utils/push.ts?unsupported')
    assert.equal(unsupported.pushSupported,false)
    await unsupported.updatePushPresence('one')
    assert.equal(calls.at(-1).body.session_id,'one','Presence must work without Push API support')
  } finally {
    for(const [key,value] of Object.entries(originals)) {if(value) Object.defineProperty(globalThis,key,value);else delete globalThis[key]}
  }
})
