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
  const calls=details.getByRole('listitem');
  await expect(calls.filter({hasText:'vendor/fast'})).toContainText(/chat.*\$0\.001234/);
  await expect(calls.filter({hasText:'vendor/free'})).toContainText(/title.*\$0\.00/);
  await expect(calls.filter({hasText:'vendor/smart'})).toContainText(/review.*No data/);
  await page.evaluate(()=>{props.cost={total_usd:0.000000014,partial:false,calls:[]}});
  await expect(page.getByTestId('token-usage')).toContainText('<$0.000001');
  await expect(details).not.toContainText('Incomplete tracking');
});
