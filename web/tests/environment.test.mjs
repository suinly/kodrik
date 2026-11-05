import {test} from 'node:test';
import assert from 'node:assert/strict';
import {expect} from '@playwright/test';
import {componentPage} from './devices.test.mjs';
import {t,setLanguage} from '../app/utils/i18n.ts';

test('local-only attachment notices translate in both languages',()=>{
  setLanguage('en');
  assert.equal(t('Файл остался на отключённом устройстве.'),'The file remains on the disconnected device.');
  assert.equal(t('Локальный файл не сохранён на сервере.'),'The local file is not saved on the server.');
  setLanguage('ru');
  assert.equal(t('Файл остался на отключённом устройстве.'),'Файл остался на отключённом устройстве.');
});

test('environment status escapes metadata, updates to disconnected tombstone and never revokes on close',async()=>{
  const status={session_id:'session-1',environment:{connection_id:'connection-1',generation:'generation-1'},label:'<img src=x onerror=alert(1)>',project_root:'/work/<script>alert(1)</script>',state:'connected',reason:null};
  const {page,browser,mount}=await componentPage('EnvironmentStatus',{status});
  try {
    const requests=[];
    page.on('request',request=>requests.push(request.url()));
    await mount();
    const badge=page.getByRole('status');
    await expect(badge).toContainText('Connected');
    await expect(badge).toContainText(status.label);
    await expect(badge).toContainText(status.project_root);
    assert.equal(await badge.locator('img,script').count(),0);
    await page.evaluate(()=>{props.status={...props.status,state:'disconnected',reason:'<b>Lease expired</b>'}});
    await expect(badge).toContainText('Disconnected');
    await expect(badge).toContainText('<b>Lease expired</b>');
    await expect(badge).toContainText(status.project_root);
    assert.equal(await badge.locator('b').count(),0);
    await page.evaluate(()=>app.unmount());
    assert.deepEqual(requests,[]);
  } finally {await browser.close()}
});
