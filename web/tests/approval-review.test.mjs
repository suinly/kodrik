import {test} from 'node:test';
import assert from 'node:assert/strict';
import {expect} from '@playwright/test';
import {componentPage} from './devices.test.mjs';

const approval={id:'one',run_id:'run',call_id:'call',tool_name:'local__exec',arguments_json:'{"cmd":"pwd"}',cmd:'pwd',workdir:null,reason:'Review this invocation',target:{tool_kind:'exec',label:'<img src=x onerror=alert(1)>',project_root:'/work/<script>alert(1)</script>',environment:{connection_id:'connection',generation:'generation'}}};

async function approvalPage(item=approval) {
  const fixture=await componentPage('ChatCommandApprovals',{approvals:[item],deciding:[],problems:{},disabled:false});
  await fixture.page.evaluate(()=>{props.deciding=new Set();window.decisions=[];props.onDecide=(...decision)=>decisions.push(decision)});
  await fixture.mount();
  return fixture;
}

test('short approval renders literal origin label and root and permits explicit allow without extra consent',async()=>{
  const {page,browser}=await approvalPage();
  try {
    const details=page.locator('pre');
    const rendered=JSON.parse(await details.textContent());
    assert.equal(rendered.target.label,'<img src=x onerror=alert(1)>');
    assert.equal(rendered.target.project_root,'/work/<script>alert(1)</script>');
    assert.equal(await details.locator('img,script').count(),0);
    assert.equal(await page.getByRole('checkbox').count(),0);
    await page.getByRole('button',{name:'Allow once',exact:true}).click();
    assert.deepEqual(await page.evaluate(()=>decisions),[['one',true]]);
  } finally {await browser.close()}
});

test('long approval requires full invocation review and resets consent on same-id argument or origin changes',async()=>{
  const cmd='start-'+ 'x'.repeat(4500)+'-FINAL-MARKER';
  const {page,browser}=await approvalPage({...approval,cmd,arguments_json:JSON.stringify({cmd})});
  try {
    const allow=page.getByRole('button',{name:'Allow once',exact:true});
    const checkbox=page.getByRole('checkbox',{name:'I reviewed the full invocation',exact:true});
    await expect(allow).toBeDisabled();
    await expect(page.getByRole('button',{name:'Reject',exact:true})).toBeEnabled();
    assert.equal(JSON.parse(await page.locator('pre').textContent()).arguments.cmd,cmd);
    await expect(page.locator('pre')).toHaveAttribute('tabindex','0');
    await checkbox.check();
    await allow.click();
    assert.deepEqual(await page.evaluate(()=>decisions),[['one',true]]);
    await page.evaluate(()=>{props.approvals[0].arguments_json=JSON.stringify({cmd:'changed-'+ 'y'.repeat(4500)})});
    await expect(checkbox).not.toBeChecked();
    await expect(allow).toBeDisabled();
    await checkbox.check();
    await page.evaluate(()=>{props.approvals[0].target.project_root='/different/root'});
    await expect(checkbox).not.toBeChecked();
    await expect(allow).toBeDisabled();
    await page.evaluate(()=>{props.approvals=[]});
    await expect(allow).toHaveCount(0);
    await page.evaluate(item=>{props.approvals=[item]}, {...approval,cmd,arguments_json:JSON.stringify({cmd})});
    await expect(checkbox).not.toBeChecked();
    await expect(allow).toBeDisabled();
  } finally {await browser.close()}
});
