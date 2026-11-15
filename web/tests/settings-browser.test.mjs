import {test} from 'node:test'
import assert from 'node:assert/strict'
import {expect} from '@playwright/test'
import {componentPage} from './devices.test.mjs'

const values={web:{enabled:true,listen:'0.0.0.0:8080',trusted_proxies:[],base_url:null,max_event_streams_per_user:4},companion:{learning_enabled:true,proactive_enabled:true,interface:null,chat_id:null,timezone:'Europe/Moscow',proactive_hour:12,quiet_start:22,quiet_end:9},sessions:{restart_limit:5,compact_threshold_tokens:250000,compact_background_margin_tokens:50000},research:{searxng_url:null,degoog_url:null,obscura_path:'obscura'},provider:{main:{type:'openai',api_key:'secret-key',model:'test',api_base:null,future:'keep'}},gateway:{main:{type:'telegram',token:'secret-token',stream:true}},mcp:{},future:{keep:true}}

test('sections keep edits and unknown values, keys are optional, and saving includes hidden sections',async()=>{
 const {page,browser,mount}=await componentPage('SettingsPage',{owner:true})
 try {
  let saved
  await page.route('**/api/gateway',r=>r.fulfill({json:{instance_id:'old',pid:1,restart_available:true}}))
  await page.route('**/api/config/form',r=>{
   if(r.request().method()==='PUT') {saved=r.request().postDataJSON();return r.fulfill({json:{values:saved.values,revision:'r2'}})}
   return r.fulfill({json:{values,revision:'r1'}})
  })
  await page.setViewportSize({width:1200,height:900})
  await mount();await page.getByRole('button',{name:'Reveal full configuration'}).click()
  await page.locator('summary').filter({hasText:'main'}).click()
  await expect(page.getByLabel('provider.main.api_key',{exact:true})).toHaveAttribute('type','password')
  await page.getByLabel('provider.main.model',{exact:true}).fill('new-model')
  await page.getByRole('button',{name:'Web server',exact:true}).click()
  await expect(page.getByLabel('provider.main.model',{exact:true})).toHaveCount(0)
  await page.getByLabel('web.max_event_streams_per_user',{exact:true}).fill('12')
  await expect(page.locator('span.font-mono').filter({hasText:'web.max_event_streams_per_user'})).toHaveCount(0)
  await page.getByLabel('Show configuration keys',{exact:true}).check()
  await expect(page.locator('span.font-mono').filter({hasText:'web.max_event_streams_per_user'})).toBeVisible()
  await page.getByText('Advanced settings',{exact:true}).click()
  await page.getByLabel('web.listen',{exact:true}).fill('127.0.0.1:9999')
  await page.getByRole('button',{name:'Models',exact:true}).click()
  await page.locator('summary').filter({hasText:'main'}).click()
  await expect(page.getByLabel('provider.main.model',{exact:true})).toHaveValue('new-model')
  await expect(page.getByRole('button',{name:'Restart gateway',exact:true})).toBeDisabled()
  await page.getByRole('button',{name:'Save configuration',exact:true}).click()
  await expect(page.getByRole('button',{name:'Restart gateway',exact:true})).toBeEnabled()
  assert.equal(saved.revision,'r1');assert.equal(saved.values.web.max_event_streams_per_user,12)
  assert.equal(saved.values.web.listen,'127.0.0.1:9999')
  assert.equal(saved.values.provider.main.model,'new-model')
  assert.equal(saved.values.provider.main.future,'keep');assert.deepEqual(saved.values.future,{keep:true})
  await page.setViewportSize({width:390,height:844})
  await page.locator('select').selectOption('research')
  await expect(page.getByLabel('research.degoog_url',{exact:true})).toBeVisible()
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth>innerWidth),false)
 } finally {await browser.close()}
})

test('conflict retains edits, reload requires discard, and validation is plain text',async()=>{
 const {page,browser,mount}=await componentPage('SettingsPage',{owner:true})
 try {
  let failure=409,reads=0
  await page.route('**/api/gateway',r=>r.fulfill({json:{instance_id:'old',pid:1,restart_available:true}}))
  await page.route('**/api/config/form',r=>{
   if(r.request().method()==='PUT') return r.fulfill({status:failure,json:{error:'<img src=x onerror=alert(1)>'}})
   reads++;return r.fulfill({json:{values,revision:'r1'}})
  })
  await page.setViewportSize({width:1200,height:900})
  await mount();await page.getByRole('button',{name:'Reveal full configuration'}).click()
  await page.locator('summary').filter({hasText:'main'}).click()
  const model=page.getByLabel('provider.main.model',{exact:true})
  await model.fill('changed')
  await page.getByRole('button',{name:'Save configuration',exact:true}).click()
  await expect(page.getByRole('alert')).toContainText(/changed|измен/)
  await expect(model).toHaveValue('changed')
  page.once('dialog',d=>d.dismiss())
  await page.getByRole('button',{name:'Reload from server',exact:true}).click();assert.equal(reads,1)
  failure=422
  await page.getByRole('button',{name:'Save configuration',exact:true}).click()
  await expect(page.getByRole('alert')).toHaveText('<img src=x onerror=alert(1)>')
  assert.equal(await page.locator('img').count(),0)
  page.once('dialog',d=>d.accept())
  await page.getByRole('button',{name:'Reload from server',exact:true}).click()
  await page.locator('summary').filter({hasText:'main'}).click()
  await expect(model).toHaveValue('test')
  await expect(page.getByRole('button',{name:'Restart gateway',exact:true})).toBeEnabled()
 } finally {await browser.close()}
})


test('field labels switch between English and Russian without changing input values',async()=>{
 const {page,browser,mount}=await componentPage('SettingsPage',{owner:true})
 try {
  await page.route('**/api/gateway',r=>r.fulfill({json:{instance_id:'old',pid:1,restart_available:true}}))
  await page.route('**/api/config/form',r=>r.fulfill({json:{values,revision:'r1'}}))
  await page.setViewportSize({width:1200,height:900})
  await mount();await page.getByRole('button',{name:'Reveal full configuration'}).click()
  await page.getByRole('button',{name:'Web server',exact:true}).click()
  await expect(page.getByLabel('web.max_event_streams_per_user',{exact:true}).locator('..')).toContainText('Live connections per user')
  await page.getByLabel('web.max_event_streams_per_user',{exact:true}).fill('12')
  await page.evaluate(()=>{
   Object.defineProperty(navigator,'language',{configurable:true,value:'ru-RU'})
   window.dispatchEvent(new Event('languagechange'))
  })
  await expect(page.getByLabel('web.max_event_streams_per_user',{exact:true}).locator('..')).toContainText('Подключения на пользователя')
  await expect(page.getByRole('button',{name:'Модели',exact:true})).toBeVisible()
  await expect(page.getByLabel('web.max_event_streams_per_user',{exact:true})).toHaveValue('12')
  await expect(page.getByRole('checkbox',{name:'Включено',exact:true})).toBeChecked()
 } finally {await browser.close()}
})
