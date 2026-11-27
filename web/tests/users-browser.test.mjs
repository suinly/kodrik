import assert from 'node:assert/strict'
import {test} from 'node:test'
import {expect} from '@playwright/test'
import {componentPage} from './devices.test.mjs'

const url='https://kodrik.test/nested/api/admin/users'
const empty={calls:0,input_tokens:0,cached_tokens:0,output_tokens:0,cost_usd:0,partial:false,tokens_partial:false}
const owner={telegram_id:'42',name:'Ada',owner:true,blocked:false}
const member={telegram_id:'9007199254740993',name:'Ben',owner:false,blocked:false}
const accounts=(route)=>route.fulfill({json:{users:[owner,member].map(account=>({...account,usage:empty}))}})

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

test('overview links use the owner key and preserve a large Telegram ID',async()=>{
  const {page,browser,mount}=await componentPage('UsersPage')
  try {
    await page.route(url,accounts)
    await mount()
    await expect(page.getByRole('link',{name:'Ada'})).toHaveAttribute('href','/nested/app/users/owner')
    await expect(page.getByRole('link',{name:'Ben'})).toHaveAttribute('href','/nested/app/users/9007199254740993')
  } finally {await browser.close()}
})

test('account edits send one field at a time and the owner cannot be blocked',async()=>{
  const {page,browser,mount}=await componentPage('UsersPage',{account:member.telegram_id})
  try {
    const posts=[]
    let saved={...member}
    await page.route(url,accounts)
    await page.route(`${url}/*/usage`,route=>route.fulfill({json:{usage:empty,calls:[],next:null,undated_calls:0}}))
    await page.route(`${url}/*`,route=>{
      const request=route.request()
      const body=request.postDataJSON()
      posts.push([request.method(),request.url(),body])
      if(request.url().endsWith('/owner')) return route.fulfill({json:{...owner,...body}})
      if(body.blocked && posts.length===2) return route.fulfill({status:503})
      saved={...saved,...body}
      return route.fulfill({json:saved})
    })
    await mount()
    const name=page.getByRole('textbox',{name:'Name',exact:true})
    const save=page.getByRole('button',{name:'Save name',exact:true})
    await expect(name).toHaveValue('Ben')
    await name.fill('  Benji  ')
    await save.click()
    await expect(page.getByRole('heading',{name:'Benji',exact:true})).toBeVisible()
    const block=page.getByRole('button',{name:'Block account',exact:true})
    const cancelled=page.waitForEvent('dialog',{timeout:5000}).then(dialog=>dialog.dismiss())
    await block.click()
    await cancelled
    assert.equal(posts.length,1)
    page.on('dialog',dialog=>dialog.accept())
    await block.click()
    await expect(page.getByRole('alert')).toContainText('Could not update the account')
    await block.click()
    const unblock=page.getByRole('button',{name:'Unblock account',exact:true})
    await expect(unblock).toBeVisible()
    await expect(page.getByText(/sign in again and reconnect/i)).toBeVisible()
    await unblock.click()
    await expect(block).toBeVisible()
    await page.evaluate(()=>{window.props.account='owner'})
    await expect(name).toHaveValue('Ada')
    await expect(block).toHaveCount(0)
    await expect(unblock).toHaveCount(0)
    await name.fill('Ada L.')
    await save.click()
    await expect(page.getByRole('heading',{name:'Ada L.',exact:true})).toBeVisible()
    assert.deepEqual(posts,[
      ['POST',`${url}/${member.telegram_id}`,{name:'Benji'}],
      ['POST',`${url}/${member.telegram_id}`,{blocked:true}],
      ['POST',`${url}/${member.telegram_id}`,{blocked:true}],
      ['POST',`${url}/${member.telegram_id}`,{blocked:false}],
      ['POST',`${url}/owner`,{name:'Ada L.'}],
    ])
  } finally {await browser.close()}
})

test('an account with no name uses its role as the owner display label',async()=>{
  const {page,browser,mount}=await componentPage('UsersPage',{account:'owner'})
  try {
    await page.route(url,route=>route.fulfill({json:{users:[{...owner,name:'',usage:empty}]}}))
    await page.route(`${url}/owner/usage`,route=>route.fulfill({json:{usage:empty,calls:[],next:null,undated_calls:0}}))
    await mount()
    await expect(page.getByRole('heading',{name:'Owner',exact:true})).toBeVisible()
    await expect(page.getByRole('textbox',{name:'Name',exact:true})).toHaveValue('')
  } finally {await browser.close()}
})

test('usage filters by local calendar days across DST and paginates the applied range',async()=>{
  const {page,browser,mount}=await componentPage('UsersPage',{account:'owner'})
  try {
    const cdp=await page.context().newCDPSession(page)
    await cdp.send('Emulation.setTimezoneOverride',{timezoneId:'America/New_York'})
    const requests=[]
    const call={id:'one',model:'first-model',created_at:1772998200,cost_usd:0.012345,partial:true}
    const legacy={id:'old',model:'legacy-model',created_at:null,cost_usd:null,partial:true}
    await page.route(url,accounts)
    await page.route(`${url}/owner/usage**`,route=>{
      const params=Object.fromEntries(new URL(route.request().url()).searchParams)
      requests.push(params)
      const calls=params.before?[{...call,id:'two',model:'second-model',cost_usd:0,partial:false}]:params.from?[call]:[call,legacy]
      return route.fulfill({json:{usage:{...empty,calls:params.from?2:3},calls,next:params.from&&!params.before?'opaque +/= cursor':null,undated_calls:1}})
    })
    await mount()
    const row=model=>page.getByRole('row').filter({hasText:model})
    await expect(row('legacy-model')).toContainText('Date unknown')
    await expect(row('legacy-model')).toContainText('No cost data')
    const start=page.getByLabel('Start date',{exact:true})
    const end=page.getByLabel('End date (inclusive)',{exact:true})
    await start.fill('2026-03-08')
    await end.fill('2026-03-07')
    await expect(page.getByRole('button',{name:'Apply dates'})).toBeDisabled()
    await end.fill('2026-03-08')
    await page.getByRole('button',{name:'Apply dates'}).click()
    await expect(row('legacy-model')).toHaveCount(0)
    await expect(row('first-model')).toContainText(/Mar 8, 2026.*3:30 PM/)
    await expect(row('first-model')).toContainText('$0.012345')
    await expect(row('first-model')).toContainText('Known cost subtotal')
    await expect(page.getByText(/undated calls.*all-time/i)).toBeVisible()
    await end.fill('2026-03-09')
    await page.getByRole('button',{name:'Show more',exact:true}).click()
    await expect(row('second-model')).toContainText('$0.00')
    await expect(row('first-model')).toBeVisible()
    await page.getByRole('button',{name:'All time',exact:true}).click()
    await expect(row('legacy-model')).toBeVisible()
    await expect(row('second-model')).toHaveCount(0)
    assert.deepEqual(requests,[
      {},{from:'1772946000',to:'1773028800'},
      {from:'1772946000',to:'1773028800',before:'opaque +/= cursor'}, {},
    ])
  } finally {await browser.close()}
})
