import {test} from 'node:test';
import assert from 'node:assert/strict';
import {chromium} from '../../../web/node_modules/@playwright/test/index.mjs';
import {extractPage} from '../core.mjs';

test('capture excludes hidden content, form fields and editable secrets; prefers selection',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.setContent('<title>Article</title><article><h1>Visible heading</h1><p>Visible paragraph</p><span hidden>hidden secret</span><textarea>textarea secret</textarea><input value="input secret"><div contenteditable>editable secret</div></article>');
    let capture=await page.evaluate(extractPage);
    assert.match(capture.text,/Visible heading/);assert.match(capture.text,/Visible paragraph/);assert.doesNotMatch(capture.text,/secret/);
    await page.locator('p').evaluate(el=>{const range=document.createRange();range.selectNodeContents(el);getSelection().removeAllRanges();getSelection().addRange(range)});
    capture=await page.evaluate(extractPage);
    assert.equal(capture.text,'Visible paragraph');assert.equal(capture.selection,true);
  } finally {await browser.close();}
});
