import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtemp,cp,readFile,writeFile,rm,mkdir} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {fileURLToPath} from 'node:url';
import {chromium,expect} from '../../../web/node_modules/@playwright/test/index.mjs';
import {startFixture,stopFixture} from '../../../web/tests/fixture-process.mjs';

test('extension embeds authenticated chat and sends fresh page data as a text attachment', {timeout:120000},async()=>{
  const dir=await mkdtemp(join(tmpdir(),'kodrik-extension-test-'));
  let context,server;
  try {
    const fixture=await startFixture();server=fixture.server;
    const base=fixture.url;
    const extension=join(dir,'extension');
    await cp(fileURLToPath(new URL('../',import.meta.url)),extension,{recursive:true});
    // A browser test pregrants only the fixture hosts; shipping manifest uses optional permissions.
    const manifest=JSON.parse(await readFile(join(extension,'manifest.json'),'utf8'));
    manifest.host_permissions=['http://127.0.0.1/*'];
    await writeFile(join(extension,'manifest.json'),JSON.stringify(manifest));
    context=await chromium.launchPersistentContext(join(dir,'profile'),{headless:true,channel:'chromium',args:[`--disable-extensions-except=${extension}`,`--load-extension=${extension}`]});
    const worker=context.serviceWorkers()[0] || await context.waitForEvent('serviceworker');
    const extensionId=new URL(worker.url()).host;
    const login=await context.newPage();
    await login.goto(new URL('auth/login',base).href);
    await login.locator('input[type=password]').fill('preview-password');
    await login.locator('button[type=submit]').click();
    await expect(login.locator('input[type=password]')).toHaveCount(0);
    await worker.evaluate(server=>chrome.storage.local.set({server}),base);
    const panel=await context.newPage();
    await panel.goto(`chrome-extension://${extensionId}/sidepanel.html`);
    const chat=panel.frameLocator('#chat');
    const input=chat.getByRole('textbox',{name:/^(Сообщение|Message)$/,exact:true});
    await expect(input).toBeEditable();
    await expect(panel.locator('#status')).toHaveText('');
    const article=await context.newPage();
    await article.route('http://127.0.0.1:19991/article',route=>route.fulfill({contentType:'text/html',body:'<title>Test article</title><article>ORIGINAL ARTICLE CONTENT</article>'}));
    await article.goto('http://127.0.0.1:19991/article');
    // Side panel focus doesn't activate a browser tab; keep the article active.
    await article.bringToFront();
    await input.fill('Объясни открытую статью');
    const sent=panel.waitForRequest(req=>req.method()==='POST' && req.url().endsWith('/api/chat/send'));
    await chat.getByRole('button',{name:/^(Отправить|Send)$/,exact:true}).click();

    const request=await sent;
    assert.match(request.headers()['content-type'],/multipart\/form-data/);
    const attachment=chat.getByRole('link',{name:'browser-tab.txt',exact:true});
    await expect(attachment).toHaveCount(1);
    const body=await (await context.request.get(await attachment.getAttribute('href'))).text();
    assert.match(body,/ORIGINAL ARTICLE CONTENT/);assert.match(body,/UNTRUSTED/);
    await expect(input).toBeEditable();
    await article.locator('article').evaluate(el=>el.textContent='UPDATED ARTICLE CONTENT');
    await article.bringToFront();
    await input.fill('А сейчас?');
    const second=panel.waitForRequest(req=>req.method()==='POST' && req.url().endsWith('/api/chat/send'));
    await chat.getByRole('button',{name:/^(Отправить|Send)$/,exact:true}).click();
    await second;
    await expect(attachment).toHaveCount(2);
    const next=await (await context.request.get(await attachment.last().getAttribute('href'))).text();
    assert.match(next,/UPDATED ARTICLE CONTENT/);assert.doesNotMatch(next,/ORIGINAL ARTICLE CONTENT/);
    await chat.getByRole('button',{name:/^(Новый диалог|New conversation)$/,exact:true}).first().click();
    await input.fill('Новый диалог о вкладке');
    await article.bringToFront();
    const third=panel.waitForRequest(req=>req.method()==='POST' && req.url().endsWith('/api/chat/send'));
    await chat.getByRole('button',{name:/^(Отправить|Send)$/,exact:true}).click();
    await third;
    await expect(attachment).toHaveCount(1);
    await panel.setViewportSize({width:420,height:850});
    await panel.waitForTimeout(700);
    await mkdir(join(tmpdir(),'kodrik-sidebar-check'),{recursive:true});
    await panel.screenshot({path:join(tmpdir(),'kodrik-sidebar-check/sidebar.png')});
  } finally {await context?.close();await stopFixture(server);await rm(dir,{recursive:true,force:true});}
});
