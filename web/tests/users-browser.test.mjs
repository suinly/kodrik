import assert from 'node:assert/strict'
import {test} from 'node:test'
import {expect} from '@playwright/test'
import {componentPage} from './devices.test.mjs'

const url='https://kodrik.test/nested/api/admin/users'
const empty={calls:0,input_tokens:0,cached_tokens:0,output_tokens:0,cost_usd:0,partial:false,tokens_partial:false}

test('account usage distinguishes unknown data from zero and sums only known usage',async()=>{
  const {page,browser,mount}=await componentPage('UsersPage')
  try {
    await page.route(url,route=>{
      assert.equal(route.request().method(),'GET')
      return route.fulfill({json:{users:[
        {telegram_id:'9007199254740993',name:'Ada',owner:true,usage:{...empty,calls:2,input_tokens:1200,cached_tokens:1000,output_tokens:70,cost_usd:0.125,tokens_partial:true}},
        {telegram_id:'2',name:'Ben',owner:false,usage:{...empty,calls:3,input_tokens:45,output_tokens:15,cost_usd:null,partial:true}},
        {telegram_id:'3',name:'Casey',owner:false,usage:null},
        {telegram_id:'4',name:'Drew',owner:false,usage:{...empty,cost_usd:null,partial:true,tokens_partial:true}},
        {telegram_id:'5',name:'Eli',owner:false,usage:{...empty,calls:1,input_tokens:25,cached_tokens:5,output_tokens:5,cost_usd:0.25,partial:true}},
      ]}})
    })
    await mount()
    const table=page.getByRole('table')
    const row=name=>table.getByRole('row').filter({has:page.getByRole('rowheader',{name})})
    await expect(row(/Ada/)).toContainText('9007199254740993')
    await expect(row(/Ada/)).toContainText('Owner')
    await expect(row(/Ada/)).toContainText('Token counts are incomplete')
    await expect(row(/Ben/).getByRole('cell').last()).toHaveText('No cost data')
    await expect(row(/Casey/)).toContainText('Usage unavailable')
    await expect(row(/Drew/).getByRole('cell').last()).toHaveText('No cost data')
    await expect(row(/Eli/)).toContainText('Known cost subtotal')
    const total=row(/Total for listed accounts/)
    await expect(total.getByRole('cell')).toHaveText(['6','1,270','1,005','90',/\$0\.375.*Known cost subtotal/])
    await expect(total).toContainText('Token counts are incomplete')
    await expect(total).toContainText('Some accounts could not be read')
  } finally {await browser.close()}
})

for(const [name,usage,expected] of [
  ['all accounts are empty',empty,'$0.00'],
  ['a legacy gap has no recorded calls',{...empty,cost_usd:null,partial:true,tokens_partial:true},'No cost data'],
  ['an account is unreadable',null,'No cost data'],
  ['a partial account reports a zero subtotal',{...empty,cost_usd:0,partial:true},'No cost data'],
  ['recorded calls have no cost data',{...empty,calls:1,cost_usd:null,partial:true},'No cost data'],
  ['recorded calls have only a zero subtotal',{...empty,calls:1,partial:true},'No cost data'],
]) test(`aggregate cost when ${name}`,async()=>{
  const {page,browser,mount}=await componentPage('UsersPage')
  try {
    await page.route(url,route=>route.fulfill({json:{users:[
      {telegram_id:null,name:'Empty owner',owner:true,usage:empty},
      {telegram_id:'1',name:'Other account',owner:false,usage},
    ]}}))
    await mount()
    const cost=name=>page.getByRole('row').filter({has:page.getByRole('rowheader',{name})}).getByRole('cell').last()
    await expect(cost('Empty owner')).toHaveText('$0.00')
    await expect(cost(/Total for listed accounts/)).toHaveText(expected)
  } finally {await browser.close()}
})

test('usage reports loading and allows retry after a failed request',async()=>{
  const {page,browser,mount}=await componentPage('UsersPage')
  try {
    let respond
    await page.route(url,async route=>{
      if(respond) return route.fulfill({json:{users:[]}})
      await new Promise(resolve=>{respond=resolve})
      await route.fulfill({status:503})
    })
    await mount()
    await expect(page.getByRole('status')).toContainText('Loading usage')
    await expect.poll(()=>typeof respond).toBe('function')
    respond()
    await expect(page.getByRole('alert')).toContainText('Could not load users and usage')
    await page.getByRole('button',{name:'Retry',exact:true}).click()
    await expect(page.getByText('No accounts to show.')).toBeVisible()
  } finally {await browser.close()}
})
