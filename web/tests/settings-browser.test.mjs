import {test} from 'node:test'
import assert from 'node:assert/strict'
import {expect} from '@playwright/test'
import {componentPage} from './devices.test.mjs'
test('late owner loads gateway and raw TOML stays hidden until explicit reveal',async()=>{
 const {page,browser,mount}=await componentPage('SettingsPage',{owner:false})
 try {
  let reads=0
  await page.route('**/api/gateway',r=>{reads++;return r.fulfill({json:{instance_id:'old',pid:123,restart_available:true}})})
  await page.route('**/api/config',r=>r.fulfill({json:{config:'# preserved comment\n[web]\nenabled = true\n',revision:'r1'}}))
  await mount()
  await page.evaluate(()=>props.owner=true)
  await expect(page.getByRole('button',{name:'Restart gateway',exact:true})).toBeEnabled()
  assert.equal(reads,1)
  assert.equal(await page.locator('textarea').count(),0)
  await page.getByRole('button',{name:'Reveal full configuration'}).click()
  await expect(page.locator('textarea')).toHaveValue('# preserved comment\n[web]\nenabled = true\n')
  await page.locator('textarea').fill('# preserved comment\n[web]\nenabled = false\n')
  await expect(page.getByRole('button',{name:'Restart gateway',exact:true})).toBeDisabled()
  await page.setViewportSize({width:1100,height:1000})
  await page.screenshot({path:'/tmp/kodrik-settings.png',fullPage:true})
 } finally {await browser.close()}
})
