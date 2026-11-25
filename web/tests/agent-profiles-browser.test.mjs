import {test} from 'node:test'
import assert from 'node:assert/strict'
import {expect} from '@playwright/test'
import {componentPage} from './devices.test.mjs'

const saved={id:'research',name:'Research',instructions:'Read sources.',model:'fast',tools:['web_read'],skills:[]}
const draft={...saved,id:'summary',name:'Summary draft',model:null}
const catalog={profiles:[saved],drafts:[draft],tools:['web_read','web_search'],skills:['news-digest'],models:['fast','smart']}

test('Settings opens the catalog, saves a new profile and confirms deletion',async()=>{
  const {page,browser,mount}=await componentPage('SettingsPage',{owner:true})
  try {
    await page.route('**/api/gateway',r=>r.fulfill({json:{instance_id:'old',pid:1,restart_available:true}}))
    await page.route('**/nested/api/chat/agent-profiles',r=>r.fulfill({json:catalog}))
    let body,deletions=0
    await page.route('**/nested/api/chat/agent-profiles/notes',r=>{
      if(r.request().method()==='DELETE') {deletions++;return r.fulfill({status:204})}
      assert.equal(r.request().method(),'PUT')
      body=r.request().postDataJSON()
      return r.fulfill({status:204})
    })
    await page.setViewportSize({width:390,height:844})
    await mount()
    const panel=page.getByRole('region',{name:'Agent profiles',exact:true})
    await panel.getByText('Browse agent profiles',{exact:true}).click()
    await panel.getByRole('button',{name:'New agent profile',exact:true}).click()
    await expect(panel.getByRole('button',{name:/Research/})).toBeVisible()
    await panel.getByLabel('Profile ID',{exact:true}).fill('research')
    await panel.getByLabel('Profile name',{exact:true}).fill('Notes')
    await panel.getByLabel('Instructions',{exact:true}).fill('Summarize notes.')
    await panel.getByLabel('Model role',{exact:true}).selectOption('smart')
    await panel.getByLabel('web_search',{exact:true}).check()
    await panel.getByLabel('news-digest',{exact:true}).check()
    const save=panel.getByRole('button',{name:'Save profile',exact:true})
    await expect(save).toBeDisabled()
    await panel.getByLabel('Profile ID',{exact:true}).fill('notes')
    await expect(panel.getByLabel('Timeout (seconds)',{exact:true})).toHaveCount(0)
    await expect(panel.getByLabel('Maximum model calls',{exact:true})).toHaveCount(0)
    await save.click()
    await expect(panel.getByRole('status')).toHaveText('Profile saved.')
    assert.deepEqual(body,{name:'Notes',instructions:'Summarize notes.',model:'smart',tools:['web_search'],skills:['news-digest']})
    await expect(panel.getByLabel('Profile ID',{exact:true})).toBeDisabled()
    const remove=panel.getByRole('button',{name:'Delete profile',exact:true})
    page.once('dialog',dialog=>dialog.dismiss())
    await remove.click()
    assert.equal(deletions,0)
    page.once('dialog',dialog=>dialog.accept())
    await remove.click()
    await expect(panel.getByRole('status')).toHaveText('Profile deleted.')
    await expect(panel.getByRole('button',{name:/Notes/})).toHaveCount(0)
    assert.equal(deletions,1)
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false)
  } finally {await browser.close()}
})

test('bundled starter roles are read-only and can be copied to a custom profile',async()=>{
  const {page,browser,mount}=await componentPage('AgentProfilesSettings')
  try {
    const starter={...saved,id:'builtin-research',name:'Starter research',builtin:true}
    const reserved={...saved,id:'builtin-reserved',name:'Reserved starter'}
    await page.route('**/nested/api/chat/agent-profiles',r=>r.fulfill({json:{...catalog,profiles:[{...starter,timeout_secs:60,max_model_calls:8},reserved]}}))
    const writes=[]
    await page.route('**/nested/api/chat/agent-profiles/*',r=>{
      const id=new URL(r.request().url()).pathname.split('/').at(-1)
      const body=r.request().postDataJSON()
      writes.push([r.request().method(),id,body])
      return r.fulfill({json:{id,...body}})
    })
    await mount()
    for(const label of ['Reserved starter','Starter research']) {
      await page.getByRole('button',{name:label,exact:false}).click()
      await expect(page.getByLabel('Instructions',{exact:true})).toBeDisabled()
      await expect(page.getByRole('button',{name:'Save profile',exact:true})).toHaveCount(0)
      await expect(page.getByRole('button',{name:'Delete profile',exact:true})).toHaveCount(0)
    }
    await page.getByRole('button',{name:'Copy as new profile',exact:true}).click()
    await expect(page.getByLabel('Instructions',{exact:true})).toHaveValue(starter.instructions)
    await expect(page.getByLabel('Profile ID',{exact:true})).toHaveValue('')
    await page.getByLabel('Profile ID',{exact:true}).fill('builtin-override')
    const save=page.getByRole('button',{name:'Save profile',exact:true})
    await expect(save).toBeDisabled()
    await page.getByLabel('Profile ID',{exact:true}).fill('my-research')
    await save.click()
    await expect(page.getByRole('status')).toHaveText('Profile saved.')
    const {id,builtin,...body}=starter
    assert.deepEqual(writes,[['PUT','my-research',body]])
  } finally {await browser.close()}
})

test('drafts are only saved on explicit approval and failed saves preserve changes',async()=>{
  const {page,browser,mount}=await componentPage('AgentProfilesSettings')
  try {
    let loads=0,writes=0,body
    await page.route('**/nested/api/chat/agent-profiles',r=>{
      loads++
      return loads===1?r.fulfill({status:503,json:{}}):r.fulfill({json:catalog})
    })
    await page.route('**/nested/api/chat/agent-profiles/summary',r=>{
      assert.equal(r.request().method(),'PUT')
      writes++;body=r.request().postDataJSON()
      return writes===1?r.fulfill({status:422,json:{error:'<b>Invalid profile</b>'}}):r.fulfill({json:{id:'summary',...body}})
    })
    await mount()
    await expect(page.getByRole('alert')).toBeVisible()
    await page.getByRole('button',{name:'Retry',exact:true}).click()
    await page.getByRole('button',{name:/Summary draft/}).click()
    const instructions=page.getByLabel('Instructions',{exact:true})
    await expect(instructions).toHaveValue(draft.instructions)
    await expect(page.getByRole('button',{name:'Delete profile',exact:true})).toHaveCount(0)
    await instructions.fill('User-reviewed instructions.')
    page.once('dialog',dialog=>dialog.dismiss())
    await page.getByRole('button',{name:/Research/}).click()
    await expect(instructions).toHaveValue('User-reviewed instructions.')
    assert.equal(writes,0)
    const save=page.getByRole('button',{name:'Save profile',exact:true})
    await save.click()
    await expect(page.getByRole('alert')).toHaveText('<b>Invalid profile</b>')
    await expect(instructions).toHaveValue('User-reviewed instructions.')
    await save.click()
    await expect(page.getByRole('status')).toHaveText('Profile saved.')
    assert.deepEqual(body,{name:draft.name,instructions:'User-reviewed instructions.',model:null,tools:draft.tools,skills:[]})
    await expect(page.getByText('Drafts for review',{exact:true})).toHaveCount(0)
  } finally {await browser.close()}
})
