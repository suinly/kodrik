import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {chromium} from '../../../web/node_modules/@playwright/test/index.mjs';

test('Chromium parses extension frame sources without invalid-source warnings',async()=>{
  const manifest=JSON.parse(await readFile(new URL('../manifest.json',import.meta.url),'utf8'));
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    const warnings=[];
    page.on('console',message=>{if(/invalid source/i.test(message.text())) warnings.push(message.text());});
    // Only frame-src is exercised; script-src extension semantics differ from web meta CSP.
    const frames=manifest.content_security_policy.extension_pages.split(';').find(rule=>rule.trim().startsWith('frame-src'));
    await page.setContent(`<meta http-equiv="Content-Security-Policy" content="${frames}"><p>Policy parsed</p>`);
    assert.deepEqual(warnings,[]);
  } finally {await browser.close();}
});
