import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtemp, rm, readFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {chromium, expect} from '../../../web/node_modules/@playwright/test/index.mjs';
import {extractPage} from '../core.mjs';
import {readDocument} from '../documents.mjs';
import {normalizeLanguage} from '../i18n.mjs';

test('unsupported locales fall back to English; Russian regions normalize',()=>{
  assert.equal(normalizeLanguage('ru-RU'),'ru');
  assert.equal(normalizeLanguage('RU'),'ru');
  assert.equal(normalizeLanguage('en-GB'),'en');
  assert.equal(normalizeLanguage('de-DE'),'en');
  assert.equal(normalizeLanguage(null),'en');
});

test('Chrome loads localized manifest and injected errors without module closures',async t=>{
  const dir=await mkdtemp(join(tmpdir(),'kodrik-locale-'));
  const extension=fileURLToPath(new URL('../',import.meta.url));
  const context=await chromium.launchPersistentContext(dir,{headless:true,channel:'chromium',locale:'en-US',args:[`--disable-extensions-except=${extension}`,`--load-extension=${extension}`]});
  t.after(async()=>{await context.close();await rm(dir,{recursive:true,force:true});});
  const worker=context.serviceWorkers()[0] || await context.waitForEvent('serviceworker');
  const manifest=await worker.evaluate(()=>chrome.runtime.getManifest());
  assert.equal(manifest.name,'Kodrik — chat with your tab');
  assert.equal(manifest.action.default_title,'Open Kodrik');
  const panel=await context.newPage();
  await panel.goto(new URL('sidepanel.html',worker.url()).href);
  await expect(panel.getByRole('dialog',{name:'Connection settings'})).toBeVisible();
  await expect(panel.getByLabel('Server address')).toBeVisible();
  // Read both real catalogs, pass only plain strings across Chrome's function serialization.
  const ru=JSON.parse(await readFile(new URL('../_locales/ru/messages.json',import.meta.url)));
  const en=JSON.parse(await readFile(new URL('../_locales/en/messages.json',import.meta.url)));
  const messages=Object.fromEntries(Object.keys(ru).map(key=>[ru[key].message,en[key].message]));
  const page=await context.newPage();
  await page.setContent('<p></p>');
  await assert.rejects(page.evaluate(extractPage,{messages}),/Text is unavailable/);
  await assert.rejects(page.evaluate(({fn,messages})=>new Function(`return (${fn})`)()('https://different.test/',false,messages),{fn:readDocument.toString(),messages}),/The tab changed while being read/);
});
