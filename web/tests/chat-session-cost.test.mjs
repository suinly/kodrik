import {test} from 'node:test';
import {expect} from '@playwright/test';
import {componentPage} from './devices.test.mjs';

test('session cost distinguishes partial totals, very small costs, zero and missing prices',async t=>{
  const {page,browser,mount}=await componentPage('ChatTokenUsage');
  t.after(()=>browser.close());
  await page.evaluate(()=>{
    const {h}=VueForTests;
    app.component('UPopover',{setup:(_,{slots})=>()=>h('div',[slots.default?.(),slots.content?.()])});
  });
  await mount();
  const details=page.getByTestId('session-cost');
  await expect(details).toHaveCount(0);
  await page.evaluate(()=>{props.cost={total_usd:0.001234,partial:true,calls:[
    {id:'one',model:'vendor/fast',kind:'chat',cost_usd:0.001234,run_id:'run'},
    {id:'two',model:'vendor/free',kind:'title',cost_usd:0},
    {id:'three',model:'vendor/smart',kind:'review',cost_usd:null},
  ]}});
  await expect(page.getByTestId('token-usage')).toContainText('≥$0.001234');
  await expect(details).toContainText('Incomplete tracking');
  const calls=details.getByTestId('model-call-group');
  await expect(calls.locator('details[open]')).toHaveCount(0);
  for(const group of await calls.all()) await group.locator('summary').click();
  await expect(calls.filter({hasText:'vendor/fast'})).toContainText(/chat.*\$0\.001234/);
  await expect(calls.filter({hasText:'vendor/free'})).toContainText(/title.*\$0\.00/);
  await expect(calls.filter({hasText:'vendor/smart'})).toContainText(/review.*No data/);
  await page.evaluate(()=>{props.cost={total_usd:0.005,partial:true,calls:[
    {id:'a',model:'vendor/repeated',kind:'chat',cost_usd:0.001,run_id:'run'},
    {id:'b',model:'vendor/other',kind:'review',cost_usd:0.004,run_id:'run'},
    {id:'c',model:'vendor/repeated',kind:'title',cost_usd:null,run_id:'run'},
  ]}});
  const groups=details.getByTestId('model-call-group');
  await expect(groups).toHaveCount(2);
  await expect(groups.filter({hasText:'vendor/repeated'})).toContainText(/chat.*\$0\.001.*title.*No data/);
  await expect(groups.filter({hasText:'vendor/repeated'}).locator('summary')).toContainText('≥$0.001');
  await page.evaluate(()=>{props.cost={total_usd:0.000000014,partial:false,calls:[]}});
  await expect(page.getByTestId('token-usage')).toContainText('<$0.000001');
  await expect(details).not.toContainText('Incomplete tracking');
});

test('collapsed model groups load historical calls only when opened and show more within the scroll area',async t=>{
  const {page,browser,mount}=await componentPage('ChatTokenUsage');
  t.after(()=>browser.close());
  const requests=[];
  await page.route('**/api/chat/sessions/conversation/cost/calls?*',route=>{
    const query=new URL(route.request().url()).searchParams;
    requests.push(query.get('before'));
    const last=query.has('before');
    return route.fulfill({json:{calls:Array.from({length:last?1:100},(_,i)=>({id:`${last?'old':'new'}-${i}`,model:'vendor/old',kind:last?'first call':'chat',cost_usd:0.01})),next:last?null:2}});
  });
  await page.evaluate(()=>{
    const {h}=VueForTests;
    app.component('UPopover',{setup:(_,{slots})=>()=>h('div',[slots.default?.(),slots.content?.()])});
    props.sessionId='conversation';
    props.cost={total_usd:1.01,partial:false,calls:[],models:[{model:'vendor/old',count:101,total_usd:1.01,partial:false}]};
  });
  await mount();
  const group=page.getByTestId('model-call-group');
  await expect(group.locator('summary')).toContainText('vendor/old');
  expect(requests).toEqual([]);
  await group.locator('summary').click();
  const scroll=group.getByTestId('model-call-scroll');
  await expect(scroll.locator('li')).toHaveCount(100);
  await expect(scroll).toHaveClass(/max-h-48.*overflow-y-auto/);
  await scroll.getByText('Show more').click();
  await expect(scroll.locator('li')).toHaveCount(101);
  await expect(scroll).toContainText('first call');
  expect(requests).toEqual([null,'2']);
});
