import {test} from 'node:test';
import assert from 'node:assert/strict';
import {chromium} from '../../../web/node_modules/@playwright/test/index.mjs';
import {extractPage} from '../core.mjs';
import {parseChatMarkdown} from '../../../web/app/utils/chat-markdown.ts';

test('captured table cells preserve literal backslashes and pipes when rendered as Markdown',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.setContent('<main><table><tr><th>Name</th><th>Value</th></tr><tr><td></td><td>42</td></tr></table></main>');
    for(const value of ['a|b',String.raw`a\|b`,String.raw`a\\|b`,String.raw`a\*b`,'trailing\\']) {
      await page.locator('td').first().evaluate((cell,value)=>{cell.textContent=value},value);
      const {text}=await page.evaluate(extractPage);
      const {body}=await parseChatMarkdown(text);
      const table=body.children.find(node=>node.tag==='table');
      assert.ok(table,`table preserved for ${JSON.stringify(value)}`);
      const row=table.children.find(node=>node.tag==='tbody').children[0];
      assert.equal(row.children.length,2);
      assert.deepEqual(row.children[0].children,[{type:'text',value}]);
      assert.deepEqual(row.children[1].children,[{type:'text',value:'42'}]);
    }
  } finally {await browser.close();}
});

test('captured preformatted table cells preserve literal backslashes',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.setContent('<main><table><tr><th>Name</th><th>Value</th></tr><tr><td><pre></pre></td><td>42</td></tr></table></main>');
    const value=String.raw`C:\temp\file.txt`;
    await page.locator('pre').evaluate((node,value)=>{node.textContent=value},value);
    const {text}=await page.evaluate(extractPage);
    const {body}=await parseChatMarkdown(text);
    const nodes=node=>[node,...(node.children||[]).flatMap(nodes)];
    const code=nodes(body).find(node=>node.tag==='code');
    assert.ok(code);
    assert.equal(code.children.map(node=>node.value||'').join(''),value);
  } finally {await browser.close();}
});

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

test('capture preserves headings, paragraphs, links, lists, tables and image alternatives',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.setContent(`<base href="https://example.com/docs/"><nav>Outside navigation</nav><main>
      <h1>Guide</h1><p>Read <a href="start">the guide</a> today.</p>
      <ul><li>First item</li><li>Second item</li></ul>
      <table><tr><th>Name</th><th>Value</th></tr><tr><td>Alpha</td><td>42</td></tr></table>
      <img alt="Architecture diagram" width="20" height="20"><pre>const answer = 42;\n  run();</pre>
      <a href="javascript:alert(1)">Unsafe link</a>
    </main>`);
    const {text}=await page.evaluate(extractPage);
    assert.match(text,/# Guide/);
    assert.match(text,/Read \[the guide\]\(https:\/\/example.com\/docs\/start\) today\./);
    assert.match(text,/- First item\n- Second item/);
    assert.match(text,/\| Name \| Value \|\n\| --- \| --- \|\n\| Alpha \| 42 \|/);
    assert.match(text,/Architecture diagram/);
    assert.match(text,/```\nconst answer = 42;\n  run\(\);\n```/);
    assert.doesNotMatch(text,/Outside navigation|javascript:/);
  } finally {await browser.close();}
});

test('capture traverses open shadow trees and slots without exposing editable or hidden content',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.setContent('<main><section id="host"><p>Slotted paragraph</p></section><div style="display:none">hidden secret</div><div style="opacity:0">opacity secret</div><div role="textbox">textbox secret</div><select><option>option secret</option></select><output>output secret</output><div contenteditable="plaintext-only">editable secret</div></main>');
    await page.locator('#host').evaluate(el=>{el.attachShadow({mode:'open'}).innerHTML='<h2>Shadow heading</h2><slot></slot><textarea>shadow secret</textarea><div hidden>hidden shadow secret</div>';});
    const {text}=await page.evaluate(extractPage);
    assert.match(text,/## Shadow heading/);
    assert.equal(text.match(/Slotted paragraph/g)?.length,1);
    assert.doesNotMatch(text,/secret/);
  } finally {await browser.close();}
});

test('selection cannot bypass editable and hidden content filtering',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.setContent('<main><p>Selected public text</p><div contenteditable>editable secret</div><div style="font-size:0">zero-size secret</div><p>Last public text</p></main>');
    await page.locator('main').evaluate(el=>{const range=document.createRange();range.selectNodeContents(el);getSelection().removeAllRanges();getSelection().addRange(range)});
    const capture=await page.evaluate(extractPage);
    assert.equal(capture.selection,true);
    assert.match(capture.text,/Selected public text/);
    assert.doesNotMatch(capture.text,/secret/);
  } finally {await browser.close();}
});

test('capture bounds ordinary page output and selection to 1048576 characters',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.setContent(`<main><p>${'x'.repeat(1049000)}</p></main>`);
    let capture=await page.evaluate(extractPage);
    assert.equal(capture.truncated,true);assert.ok(capture.text.length<=1048576);
    await page.locator('p').evaluate(el=>{const range=document.createRange();range.selectNodeContents(el);getSelection().removeAllRanges();getSelection().addRange(range)});
    capture=await page.evaluate(extractPage);
    assert.equal(capture.truncated,true);assert.equal(capture.text.length,1048576);
  } finally {await browser.close();}
});

test('main root cannot escape an editable ancestor privacy boundary',async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage();
    await page.setContent('<div contenteditable><main><p>ancestor secret</p></main></div>');
    await assert.rejects(page.evaluate(extractPage),/Текст недоступен/);
  } finally {await browser.close();}
});
