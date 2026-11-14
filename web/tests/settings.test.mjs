import {test} from 'node:test'
import assert from 'node:assert/strict'
import {settingsRequest, restartGateway} from '../app/utils/settings.ts'

const base=new URL('https://example.test/nested/')
test('config save preserves raw TOML and revision and bypasses caches',async()=>{
  const original=globalThis.fetch
  const body={config:'# comment\nsecret = "token"\n',revision:'r1'}
  try {
    globalThis.fetch=async(url,init)=>{
      assert.equal(url.href,'https://example.test/nested/api/config')
      assert.equal(init.cache,'no-store');assert.equal(init.method,'PUT')
      assert.deepEqual(JSON.parse(init.body),body)
      return Response.json({...body,revision:'r2'})
    }
    assert.deepEqual(await settingsRequest(base,'config','PUT',body),{...body,revision:'r2'})
  } finally {globalThis.fetch=original}
})
test('config conflicts have settings-specific guidance, validation is text not HTML',async()=>{
  const original=globalThis.fetch
  try {
    globalThis.fetch=async()=>Response.json({error:'dialog busy'},{status:409})
    await assert.rejects(settingsRequest(base,'config','PUT',{}),/changed|измен/)
    globalThis.fetch=async()=>Response.json({error:'Invalid TOML (line 3)'},{status:400})
    await assert.rejects(settingsRequest(base,'config','PUT',{}),/Invalid TOML \(line 3\)/)
  } finally {globalThis.fetch=original}
})
test('restart waits for different instance, not merely a healthy HTTP response',async()=>{
  let reads=0,posts=0
  const result=await restartGateway('old',async(method)=>{
    if(method==='POST') {posts++;return}
    reads++
    return {instance_id:reads<3?'old':'new',pid:5,restart_available:true}
  },{timeoutMs:100,intervalMs:1})
  assert.equal(result,'confirmed');assert.equal(posts,1);assert.equal(reads,3)
})
test('uncertain POST is never retried and can still be confirmed by new instance',async()=>{
  let posts=0,reads=0
  const result=await restartGateway('old',async(method)=>{
    if(method==='POST') {posts++;throw new TypeError('offline')}
    return {instance_id:++reads===1?'old':'new',pid:6,restart_available:true}
  },{timeoutMs:50,intervalMs:1})
  assert.equal(result,'confirmed');assert.equal(posts,1)
})
test('unchanged instance and hanging reads stop at the deadline with uncertainty',async()=>{
  for(const hanging of [false,true]) {
    const start=Date.now()
    assert.equal(await restartGateway('old',async(method)=>{
      if(method==='POST') return
      return hanging ? new Promise(()=>{}) : {instance_id:'old',pid:1,restart_available:true}
    },{timeoutMs:20,intervalMs:1}),'uncertain')
    assert.ok(Date.now()-start<500)
  }
})
test('definitive restart rejection is surfaced without polling',async()=>{
  await assert.rejects(restartGateway('old',async(method)=>{
    if(method==='GET') return {instance_id:'old',pid:1,restart_available:true}
    throw new Error('Unavailable')
  },{timeoutMs:20,intervalMs:1}),/Unavailable/)
})

test('restart already pending is polled; invalid config 422 is shown safely',async()=>{
  const original=globalThis.fetch
  try {
    globalThis.fetch=async()=>Response.json({error:'pending'},{status:409})
    assert.equal(await settingsRequest(base,'gateway/restart','POST'),undefined)
    globalThis.fetch=async()=>Response.json({error:'Invalid saved configuration'},{status:422})
    await assert.rejects(settingsRequest(base,'gateway/restart','POST'),/Invalid saved configuration/)
  } finally {globalThis.fetch=original}
})
test('canceling tracking stops polling without retrying restart',async()=>{
  const controller=new AbortController();let reads=0
  const tracking=restartGateway('old',async(method)=>{
    if(method==='POST') return
    reads++;controller.abort();return {instance_id:'old',pid:1,restart_available:true}
  },{timeoutMs:100,intervalMs:1,signal:controller.signal})
  assert.equal(await tracking,'uncertain');assert.equal(reads,1)
})

test('POST baseline B supersedes cached A and only C confirms restart',async()=>{
 let reads=0
 const result=await restartGateway('A',async(method)=>{
  if(method==='POST') return {instance_id:'B'}
  return {instance_id:++reads<3?'B':'C',pid:1,restart_available:true}
 },{timeoutMs:50,intervalMs:1})
 assert.equal(result,'confirmed');assert.equal(reads,3)
})
test('lost POST does not confirm a stale cached baseline',async()=>{
 let posts=0
 assert.equal(await restartGateway('A',async(method)=>{
  if(method==='POST') {posts++;throw new TypeError('lost response')}
  return {instance_id:'B',pid:1,restart_available:true}
 },{timeoutMs:20,intervalMs:1}),'uncertain')
 assert.equal(posts,1)
})
test('accepted restart retains the server instance baseline',async()=>{
 const original=globalThis.fetch
 try {
  globalThis.fetch=async()=>Response.json({instance_id:'B'},{status:202})
  assert.deepEqual(await settingsRequest(base,'gateway/restart','POST'),{instance_id:'B'})
 } finally {globalThis.fetch=original}
})
