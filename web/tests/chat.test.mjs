import { after, before, test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, rm, readFile, mkdir, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium, expect } from '@playwright/test';

let server, base, cookie, storage;
async function start(listen) {
  server = spawn('cargo', ['test', 'web::chat_tests::browser_fixture', '--', '--ignored', '--nocapture'], {
    cwd: fileURLToPath(new URL('../../', import.meta.url)), env: {...process.env, CHAT_TEST_DATA:storage, ...(listen ? {CHAT_TEST_LISTEN:listen}: {})}, detached: true, stdio: ['ignore', 'pipe', 'pipe'],
  });
  base = await new Promise((resolve, reject) => {
    let output = '';
    const timer = setTimeout(() => reject(new Error(`Fixture startup timed out:\n${output}`)), 60000);
    for (const stream of [server.stdout, server.stderr]) stream.on('data', data => {
      output += data;
      const match = output.match(/CHAT_TEST_URL=(http:\/\/\S+)/);
      if (match) { clearTimeout(timer); resolve(match[1]); }
    });
    server.once('exit', code => { clearTimeout(timer); reject(new Error(`Fixture exited ${code}:\n${output}`)); });
  });
}
before(async () => {
  storage=await mkdtemp(join(tmpdir(),'kodrik-web-test-'));
  await start();
  const response = await fetch(new URL('auth/login', base), {
    method: 'POST', redirect: 'manual', headers: { Origin: new URL(base).origin },
    body: new URLSearchParams({ password: 'preview-password', return_to: new URL('chat/', base).pathname }),
  });
  assert.equal(response.status, 303);
  cookie = response.headers.get('set-cookie').split(';')[0];
});
after(async () => {
  if (server?.pid) {
    const exited = once(server, 'exit');
    try { process.kill(-server.pid, 'SIGTERM'); } catch {}
    await exited;
  }
  await rm(storage,{recursive:true,force:true});
});

async function request(path, body) {
  const response = await fetch(new URL(`api/chat/${path}`, base), {
    method: body === undefined ? 'GET' : 'POST',
    headers: { Cookie: cookie, Origin: new URL(base).origin, 'Content-Type': 'application/json' },
    ...(body === undefined ? {} : { body: JSON.stringify(body) }),
  });
  assert.ok(response.ok, `${path}: ${response.status} ${response.ok ? '' : await response.text()}`);
  return response;
}

test('Telegram approval login shows only that user shared history without a code form', {timeout:60000},async()=>{
  const owner=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'ru-RU'});
    await page.goto(new URL('auth/login?return_to=/kodrik/chat/',base).href);
    await expect(page.getByLabel('Пароль',{exact:true})).toBeVisible();
    await expect(page.locator('input[name="code"]')).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Войти по коду из Telegram',exact:true})).toHaveCount(0);
    await page.getByRole('button',{name:'Войти через Telegram',exact:true}).click();
    const open=page.getByRole('link',{name:'Открыть Telegram',exact:true});
    await expect(open).toBeVisible();
    assert.equal((await decideTelegram(loginId(await open.getAttribute('href')),true)).ok,true);
    await expect(page).toHaveURL(new URL('chat/',base).href);
    const rows=page.locator('nav[aria-label="История диалогов"] [data-conversation-id]');
    await expect(rows).toHaveCount(1);
    await expect(rows.first()).toHaveAttribute('data-conversation-id','telegram:main:7');
    await rows.first().click();
    await expect(page.locator('.message')).toContainText('История из Telegram');
    assert.equal(await page.evaluate(async id=>(await fetch(`/kodrik/api/chat/sessions/${id}`)).status,owner.id),404);
    await page.reload();
    await expect(page.locator('.message')).toContainText('История из Telegram');
  } finally {await browser.close();}
});

test('pages catalog searches published titles and addresses without losing the conversation draft', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const pages=page.getByRole('button',{name:'Страницы',exact:true});
    const dialogs=page.getByRole('button',{name:'Чат',exact:true});
    await pages.click();
    await expect(page.getByText('Здесь появятся опубликованные страницы.',{exact:true})).toBeVisible();
    await dialogs.click();
    const composer=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await composer.fill('Несохранённый черновик');
    await request('send',{id,text:'ui'});
    await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
    await pages.click();
    const navigation=page.getByRole('navigation',{name:'Каталог страниц'});
    const link=navigation.getByRole('button',{name:/^Дневник питания/});
    await expect(link).toBeVisible();
    await expect(navigation.getByRole('img',{name:'Приватная',exact:true})).toBeVisible();
    const search=page.getByRole('textbox',{name:'Поиск страниц',exact:true});
    const searchToggle=page.getByRole('button',{name:'Поиск страниц',exact:true});
    await expect(search).toHaveCount(0);
    await searchToggle.click();
    await expect(search).toBeFocused();
    for(const query of ['ДНЕВНИК','NUTRITION']) {
      await search.fill(query);
      await expect(link).toBeVisible();
    }
    await search.fill('нет такой страницы');
    await expect(link).toHaveCount(0);
    await expect(page.getByText('Страницы не найдены.',{exact:true})).toBeVisible();
    await search.press('Escape');
    await expect(search).toHaveCount(0);
    await expect(searchToggle).toBeFocused();
    await searchToggle.click();
    await expect(search).toHaveValue('');
    await link.click();
    await expect(page).toHaveURL(new URL('chat/?page=nutrition',base).href);
    const viewer=page.getByRole('region',{name:'Просмотр страницы'});
    await expect(viewer.getByRole('heading',{name:'Дневник питания',exact:true})).toBeVisible();
    await expect(viewer.getByRole('table')).toBeVisible();
    await expect(viewer.getByRole('button',{name:'Открыть чат',exact:true})).toHaveCount(0);
    const external=page.getByRole('link',{name:'Открыть в новой вкладке',exact:true});
    await expect(external).toHaveAttribute('href',new URL('pages/nutrition/',base).href);
    const opened=page.waitForEvent('popup');
    await external.click();
    const document=await opened;
    await expect(document).toHaveURL(new URL('pages/nutrition/',base).href);
    await expect(document.getByRole('table')).toBeVisible();
    await document.close();
    await page.screenshot({path:'/tmp/kodrik-pages-viewer-desktop.png',fullPage:true,animations:'disabled'});
    await page.getByRole('button',{name:'Вернуться в диалог',exact:true}).click();
    await dialogs.click();
    await expect(composer).toHaveValue('Несохранённый черновик');
    await page.route('**/api/pages',route=>route.fulfill({status:503,body:''}));
    await pages.click();
    await expect(page.getByRole('alert')).toContainText('Не удалось загрузить страницы.');
    await page.unroute('**/api/pages');
    await page.getByRole('button',{name:'Повторить загрузку',exact:true}).click();
    await expect(link).toBeVisible();
    await page.screenshot({path:'/tmp/kodrik-pages-catalog-desktop.png',fullPage:true,animations:'disabled'});
    await page.setViewportSize({width:390,height:844});
    await page.getByRole('button',{name:'Показать страницы',exact:true}).click();
    const mobileCatalog=page.getByRole('dialog').getByRole('navigation',{name:'Каталог страниц'});
    await expect(mobileCatalog).toBeVisible();
    await expect.poll(async()=> (await mobileCatalog.boundingBox())?.x ?? -1).toBeGreaterThanOrEqual(0);
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-pages-catalog-mobile.png',fullPage:true,animations:'disabled'});
    await page.getByRole('dialog').getByRole('button',{name:/^Дневник питания/}).click();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await expect(viewer.getByRole('table')).toBeVisible();
    await page.screenshot({path:'/tmp/kodrik-pages-viewer-mobile.png',fullPage:true,animations:'disabled'});
    await page.getByRole('button',{name:'Вернуться в диалог',exact:true}).click();
    await expect(composer).toHaveValue('Несохранённый черновик');

    const release=crypto.randomUUID();
    const directory=join(storage,'pages','releases','legacy-preview',release);
    await mkdir(directory,{recursive:true});
    await writeFile(join(directory,'index.html'),'<!doctype html><h1>Legacy published report</h1>');
    await writeFile(join(storage,'pages','manifests','legacy-preview.json'),JSON.stringify({release,visibility:'private',document:false}));
    await page.setViewportSize({width:1280,height:720});
    await pages.click();
    await page.getByRole('button',{name:'Обновить',exact:true}).click();
    await navigation.getByRole('button',{name:/^legacy-preview/}).click();
    await expect(page.frameLocator('iframe[title="legacy-preview"]').getByRole('heading',{name:'Legacy published report'})).toBeVisible();
    await page.getByRole('button',{name:'Вернуться в диалог',exact:true}).click();
    await expect(composer).toHaveValue('Несохранённый черновик');

    const nativeRelease=crypto.randomUUID();
    const nativeDirectory=join(storage,'pages','releases','relative-links',nativeRelease);
    const body=`[Relative report](../nutrition/)

[Anchor](#section)

::u-button{to="../nutrition/" label="Relative button"}
::

:::u-dropdown-menu{:items='[{"label":"Nested report","to":"../nutrition/"}]'}
::u-button{label="Open links"}
::
:::

## Section {#section}`;
    await mkdir(nativeDirectory,{recursive:true});
    await writeFile(join(nativeDirectory,'document.json'),JSON.stringify({document:{title:'Relative links',body,sources:[],views:[],actions:[]},snapshot:{}}));
    await writeFile(join(storage,'pages','manifests','relative-links.json'),JSON.stringify({release:nativeRelease,visibility:'private',document:true}));
    await pages.click();
    await page.getByRole('button',{name:'Обновить',exact:true}).click();
    await navigation.getByRole('button',{name:/^Relative links/}).click();
    for(const label of ['Relative report','Relative button']) await expect(viewer.getByRole('link',{name:label,exact:true})).toHaveAttribute('href',new URL('pages/nutrition/',base).href);
    await expect(viewer.getByRole('link',{name:'Anchor',exact:true})).toHaveAttribute('href','#section');
    await viewer.getByRole('button',{name:'Open links',exact:true}).click();
    await expect(page.getByRole('menuitem',{name:'Nested report',exact:true})).toHaveAttribute('href',new URL('pages/nutrition/',base).href);
  } finally {await browser.close();}
});

test('page access indicators stay visible beside long addresses on desktop and touch screens', {timeout:60000},async()=>{
  const pages=[{slug:`private-${'a'.repeat(56)}`,visibility:'private',label:'Приватная'},
    {slug:`public-${'b'.repeat(57)}`,visibility:'public',label:'Публичная'}];
  for(const {slug,visibility} of pages) {
    const release=crypto.randomUUID(),directory=join(storage,'pages','releases',slug,release);
    await mkdir(directory,{recursive:true});
    await writeFile(join(directory,'index.html'),'<!doctype html><h1>Page</h1>');
    await mkdir(join(storage,'pages','manifests'),{recursive:true});
    await writeFile(join(storage,'pages','manifests',`${slug}.json`),JSON.stringify({release,visibility,document:false}));
  }
  const browser=await chromium.launch({headless:true});
  try {
    for(const mobile of [false,true]) {
      const page=await signedPage(browser,mobile?{viewport:{width:320,height:720},isMobile:true,hasTouch:true}:{});
      await page.goto(new URL('chat/?section=pages',base).href);
      if(mobile) await page.getByRole('button',{name:'Показать страницы',exact:true}).click();
      const catalog=page.getByRole('navigation',{name:'Каталог страниц'});
      for(const {slug,label} of pages) {
        const row=catalog.getByRole('button',{name:new RegExp(`^${slug}`)});
        await row.scrollIntoViewIfNeeded();
        const indicator=row.getByRole('img',{name:label,exact:true});
        await expect(indicator).toBeVisible();
        await expect(indicator).toHaveAttribute('title',label);
        await expect(row).toHaveAttribute('aria-description',label);
        const address=row.getByText(slug,{exact:true}).last();
        assert.ok(await address.evaluate(el=>el.scrollWidth>el.clientWidth));
        const iconBox=await indicator.boundingBox(),addressBox=await address.boundingBox();
        assert.ok(iconBox.x+iconBox.width<=addressBox.x);
        const remove=row.locator('..').getByRole('button',{name:`Удалить страницу «${slug}»`,exact:true});
        assert.ok(iconBox.x+iconBox.width<=(await remove.boundingBox()).x);
      }
      assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
      await page.screenshot({path:`/tmp/kodrik-page-access-${mobile?'mobile':'desktop'}.png`,animations:'disabled'});
      await page.close();
    }
  } finally {await browser.close();}
});

test('Telegram owner opens the catalog and private pages after one chat login', {timeout:60000},async()=>{
  // Give this additional sign-in its own login rate-limit window.
  const listen=new URL(base).host;
  const exited=once(server,'exit');process.kill(-server.pid,'SIGTERM');await exited;
  await start(listen);
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'ui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'ru-RU'});
    await page.goto(new URL('auth/login?return_to=/kodrik/chat/',base).href);
    await page.getByRole('button',{name:'Войти через Telegram',exact:true}).click();
    const open=page.getByRole('link',{name:'Открыть Telegram',exact:true});
    await expect(open).toBeVisible();
    assert.equal((await decideTelegram(loginId(await open.getAttribute('href')),true,42)).ok,true);
    await expect(page).toHaveURL(new URL('chat/',base).href);
    await page.getByRole('button',{name:'Страницы',exact:true}).click();
    await page.getByRole('navigation',{name:'Каталог страниц'}).getByRole('button',{name:/^Дневник питания/}).click();
    const viewer=page.getByRole('region',{name:'Просмотр страницы'});
    await expect(viewer.getByRole('table')).toBeVisible();
    await viewer.getByRole('button',{name:'Далее',exact:true}).click();
    await expect(viewer.getByRole('cell',{name:'400',exact:true})).toBeVisible();
    const popup=page.waitForEvent('popup');
    await page.getByRole('link',{name:'Открыть в новой вкладке',exact:true}).click();
    const document=await popup;
    await expect(document).toHaveURL(new URL('pages/nutrition/',base).href);
    await expect(document.getByRole('table')).toBeVisible();
    await expect(document.getByRole('textbox',{name:'Пароль',exact:true})).toHaveCount(0);
  } finally {await browser.close();}
});

test('section rail switches context menus and main content without losing chat drafts', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'ui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{viewport:{width:1280,height:900}});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const sections=page.getByRole('navigation',{name:'Разделы',exact:true});
    await expect(sections).toBeVisible();
    assert.ok((await sections.boundingBox()).width<=64);
    const composer=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await composer.fill('Черновик между разделами');
    await page.screenshot({path:'/tmp/kodrik-sidebar-chat-desktop.png',animations:'disabled'});
    await sections.getByRole('button',{name:'Страницы',exact:true}).click();
    await expect(page.getByRole('navigation',{name:'История диалогов'})).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Новый диалог',exact:true})).toHaveCount(0);
    await page.getByRole('navigation',{name:'Каталог страниц'}).getByRole('button',{name:/^Дневник питания/}).click();
    await expect(page.getByRole('region',{name:'Просмотр страницы'}).getByRole('table')).toBeVisible();
    await sections.getByRole('button',{name:'Вебхуки',exact:true}).click();
    await expect(page.getByRole('navigation',{name:'Каталог страниц'})).toHaveCount(0);
    await page.getByRole('button',{name:'Создать вебхук',exact:true}).click();
    await page.getByLabel('Название вебхука',{exact:true}).fill('Навигационный вебхук');
    await page.getByLabel('Инструкция агенту',{exact:true}).fill('Обработай событие.');
    await page.getByRole('button',{name:'Сохранить вебхук',exact:true}).click();
    await expect(page.getByLabel('Bearer-токен',{exact:true})).toHaveValue(/[0-9a-f]{64}/);
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await expect(page.getByRole('heading',{name:'Навигационный вебхук',exact:true})).toBeVisible();
    const webhookSearch=page.getByRole('textbox',{name:'Поиск вебхуков',exact:true});
    const webhookSearchToggle=page.getByRole('button',{name:'Поиск вебхуков',exact:true});
    await expect(webhookSearch).toHaveCount(0);
    await webhookSearchToggle.click();
    await expect(webhookSearch).toBeFocused();
    const webhookList=page.getByRole('navigation',{name:'Список вебхуков'});
    await webhookSearch.fill('ничего не найдено');
    await expect(webhookList.getByRole('button')).toHaveCount(0);
    await expect(page.getByText('Вебхуки не найдены.',{exact:true})).toBeVisible();
    await webhookSearch.fill('НАВИГАЦИОННЫЙ');
    await expect(webhookList.getByRole('button',{name:'Навигационный вебхук',exact:true})).toBeVisible();
    await expect(webhookList.getByText('Включён',{exact:true})).toBeVisible();
    await webhookSearchToggle.click();
    await expect(webhookSearch).toHaveCount(0);
    await webhookSearchToggle.click();
    await expect(webhookSearch).toHaveValue('');
    await webhookSearch.press('Escape');
    await expect(webhookSearchToggle).toBeFocused();
    await page.screenshot({path:'/tmp/kodrik-section-rail-desktop.png',animations:'disabled'});
    await sections.getByRole('button',{name:'Чат',exact:true}).click();
    await expect(composer).toHaveValue('Черновик между разделами');
    await expect(page.getByLabel('Bearer-токен',{exact:true})).toHaveCount(0);
    await sections.getByRole('button',{name:'Страницы',exact:true}).click();
    await expect(page.getByRole('region',{name:'Просмотр страницы'}).getByRole('table')).toBeVisible();
    await page.screenshot({path:'/tmp/kodrik-sidebar-pages-desktop.png',animations:'disabled'});
    await page.setViewportSize({width:390,height:844});
    await expect(sections).toHaveCount(0);
    await page.getByRole('button',{name:'Показать страницы',exact:true}).click();
    const menu=page.getByRole('dialog');
    const mobileSections=menu.getByRole('navigation',{name:'Разделы',exact:true});
    await expect(mobileSections).toBeVisible();
    const buttons=mobileSections.getByRole('button');
    await expect(buttons).toHaveText(['Чат','Страницы','Вебхуки']);
    for(let index=0;index<3;index++) {
      await expect(buttons.nth(index).locator('[data-slot="leadingIcon"]')).toBeVisible();
      if(index) assert.ok((await buttons.nth(index).boundingBox()).y>=(await buttons.nth(index-1).boundingBox()).y+(await buttons.nth(index-1).boundingBox()).height);
    }
    assert.ok((await mobileSections.boundingBox()).y+(await mobileSections.boundingBox()).height<=(await menu.getByRole('heading',{name:'Страницы',exact:true}).boundingBox()).y);
    await page.screenshot({path:'/tmp/kodrik-section-menu-mobile.png',animations:'disabled'});
    await mobileSections.getByRole('button',{name:'Вебхуки',exact:true}).click();
    await expect(mobileSections.getByRole('button',{name:'Вебхуки',exact:true})).toHaveAttribute('aria-current','page');
    await page.screenshot({path:'/tmp/kodrik-sidebar-webhooks-mobile.png',animations:'disabled'});
    await menu.getByRole('navigation',{name:'Список вебхуков'}).getByRole('button',{name:'Навигационный вебхук',exact:true}).click();
    await expect(menu).toHaveCount(0);
    await expect(page.getByLabel('Название вебхука',{exact:true})).toHaveValue('Навигационный вебхук');
    assert.equal((await page.locator('[id$="-panel-chat"]').boundingBox()).x,0);
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
    await page.screenshot({path:'/tmp/kodrik-section-rail-mobile.png',animations:'disabled'});
    await page.getByRole('button',{name:'Удалить вебхук',exact:true}).click();
    await page.getByRole('button',{name:'Подтвердить удаление',exact:true}).click();
    await page.getByRole('button',{name:'Показать вебхуки',exact:true}).click();
    await mobileSections.getByRole('button',{name:'Чат',exact:true}).click();
    await expect(menu.getByRole('navigation',{name:'История диалогов'})).toBeVisible();
    await page.screenshot({path:'/tmp/kodrik-sidebar-chat-mobile.png',animations:'disabled'});
    await page.keyboard.press('Escape');
    await expect(menu).toHaveCount(0);
    await expect(composer).toHaveValue('Черновик между разделами');
  } finally {await browser.close();}
});

test('page deletion confirms, keeps failed attempts and clears selected pages on desktop and mobile', {timeout:60000},async()=>{
  for(const slug of ['delete-desktop','delete-mobile']) {
    const release=crypto.randomUUID();
    const directory=join(storage,'pages','releases',slug,release);
    await mkdir(directory,{recursive:true});
    await writeFile(join(directory,'index.html'),`<!doctype html><h1>${slug}</h1>`);
    await mkdir(join(storage,'pages','manifests'),{recursive:true});
    await writeFile(join(storage,'pages','manifests',`${slug}.json`),JSON.stringify({release,visibility:'public',document:false}));
  }
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.goto(new URL('chat/?page=delete-desktop',base).href);
    await expect(page.locator('iframe[title="delete-desktop"]')).toBeVisible();
    const remove=page.getByRole('button',{name:'Удалить страницу «delete-desktop»',exact:true});
    await expect(remove).toHaveCSS('opacity','0');
    await remove.locator('..').hover();
    await expect(remove).toHaveCSS('opacity','1');
    await page.mouse.move(1000,400);
    await expect(remove).toHaveCSS('opacity','0');
    await remove.focus();
    await expect(remove).toHaveCSS('opacity','1');
    await remove.click();
    const confirmation=page.getByRole('dialog',{name:'Удалить страницу?',exact:true});
    await expect(confirmation).toContainText('delete-desktop');
    await confirmation.getByRole('button',{name:'Отмена',exact:true}).click();
    await expect(remove).toBeVisible();
    assert.equal((await fetch(new URL('pages/delete-desktop/',base))).status,200);
    await remove.click();
    await page.route('**/api/pages/delete-desktop',route=>route.fulfill({status:503,body:''}));
    await confirmation.getByRole('button',{name:'Подтвердить удаление',exact:true}).click();
    await expect(confirmation.getByRole('alert')).toContainText('Не удалось удалить страницу. Попробуйте ещё раз.');
    await expect(page.locator('iframe[title="delete-desktop"]')).toHaveCount(1);
    await page.unroute('**/api/pages/delete-desktop');
    await confirmation.getByRole('button',{name:'Подтвердить удаление',exact:true}).click();
    await expect(confirmation).toHaveCount(0);
    await expect(remove).toHaveCount(0);
    await expect(page.locator('iframe[title="delete-desktop"]')).toHaveCount(0);
    await expect(page).toHaveURL(new URL('chat/?section=pages',base).href);
    assert.equal((await fetch(new URL('pages/delete-desktop/',base))).status,404);
    await page.reload();
    await expect(remove).toHaveCount(0);
    const mobile=await signedPage(browser,{viewport:{width:390,height:844},isMobile:true,hasTouch:true});
    await mobile.goto(new URL('chat/?section=pages',base).href);
    await mobile.getByRole('button',{name:'Показать страницы',exact:true}).click();
    const mobileRemove=mobile.getByRole('dialog').getByRole('button',{name:'Удалить страницу «delete-mobile»',exact:true});
    await expect(mobileRemove).toHaveCSS('opacity','1');
    await mobileRemove.click();
    const mobileConfirmation=mobile.getByRole('dialog',{name:'Удалить страницу?',exact:true});
    await expect(mobileConfirmation).toBeVisible();
    await mobile.screenshot({path:'/tmp/kodrik-delete-page-mobile.png',animations:'disabled'});
    await mobileConfirmation.getByRole('button',{name:'Подтвердить удаление',exact:true}).click();
    await expect(mobileConfirmation).toHaveCount(0);
    await mobile.getByRole('button',{name:'Показать страницы',exact:true}).click();
    await expect(mobileRemove).toHaveCount(0);
    assert.equal((await fetch(new URL('pages/delete-mobile/',base))).status,404);
    assert.equal(await mobile.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
  } finally {await browser.close();}
});

test('page and webhook links restore selections on reload and in a fresh tab', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'ui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await page.getByRole('button',{name:'Страницы',exact:true}).click();
    await page.getByRole('navigation',{name:'Каталог страниц'}).getByRole('button',{name:/^Дневник питания/}).click();
    await expect.poll(()=>new URL(page.url()).searchParams.get('page')).toBe('nutrition');
    assert.equal(new URL(page.url()).searchParams.has('conversation'),false);
    const pageUrl=page.url();
    await page.reload();
    await expect(page.getByRole('region',{name:'Просмотр страницы'}).getByRole('table')).toBeVisible();
    await page.getByRole('button',{name:'Вебхуки',exact:true}).click();
    await page.getByRole('button',{name:'Создать вебхук',exact:true}).click();
    await page.getByLabel('Название вебхука',{exact:true}).fill('Вебхук по ссылке');
    await page.getByLabel('Инструкция агенту',{exact:true}).fill('Диагностика события.');
    await page.getByRole('button',{name:'Сохранить вебхук',exact:true}).click();
    await expect(page.getByLabel('Bearer-токен',{exact:true})).toHaveValue(/[0-9a-f]{64}/);
    const token=await page.getByLabel('Bearer-токен',{exact:true}).inputValue();
    await expect.poll(()=>new URL(page.url()).searchParams.get('webhook')).not.toBeNull();
    const hookUrl=page.url();
    assert.equal(hookUrl.includes(token),false);
    const anonymous=await browser.newPage({locale:'ru-RU'});
    await anonymous.goto(hookUrl);
    await expect(anonymous.getByLabel('Пароль',{exact:true})).toBeVisible();
    await expect(anonymous.locator('input[name="return_to"]')).toHaveValue(new URL(hookUrl).pathname+new URL(hookUrl).search);
    await anonymous.close();
    await page.reload();
    await expect(page.getByLabel('Название вебхука',{exact:true})).toHaveValue('Вебхук по ссылке');
    await expect(page.getByLabel('Инструкция агенту',{exact:true})).toHaveValue('Диагностика события.');
    await expect(page.getByLabel('Bearer-токен',{exact:true})).toHaveCount(0);
    await page.getByRole('button',{name:'Страницы',exact:true}).click();
    await page.getByRole('navigation',{name:'Каталог страниц'}).getByRole('button',{name:/^Дневник питания/}).click();
    await expect(page).toHaveURL(pageUrl);
    await expect(page.getByRole('region',{name:'Просмотр страницы'}).getByRole('table')).toBeVisible();
    const fresh=await signedPage(browser);
    await fresh.goto(hookUrl);
    await expect(fresh.getByLabel('Название вебхука',{exact:true})).toHaveValue('Вебхук по ссылке');
    await fresh.getByRole('button',{name:'Удалить вебхук',exact:true}).click();
    await fresh.getByRole('button',{name:'Подтвердить удаление',exact:true}).click();
    await expect.poll(()=>new URL(fresh.url()).searchParams.has('webhook')).toBe(false);
    await fresh.goto(hookUrl);
    await expect(fresh.getByRole('alert')).toContainText('Вебхук удалён или не найден.');
    await expect(fresh.getByLabel('Название вебхука',{exact:true})).toHaveCount(0);
    await fresh.goto(new URL('chat/?page=does-not-exist',base).href);
    await expect(fresh.getByRole('alert')).toContainText('Страница удалена или не найдена.');
    await expect(fresh.getByRole('region',{name:'Просмотр страницы'}).getByRole('table')).toHaveCount(0);
    await fresh.route('**/api/webhooks',route=>route.fulfill({status:503,body:''}));
    await fresh.goto(hookUrl);
    await expect(fresh.getByRole('alert')).toContainText('503');
    await fresh.unroute('**/api/webhooks');
    await fresh.getByRole('button',{name:'Повторить',exact:true}).click();
    await expect(fresh.getByRole('alert')).toContainText('Вебхук удалён или не найден.');
  } finally {await browser.close();}
});

test('a delayed new conversation response cannot override a newer section', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'ui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  let release=()=>{};
  let created;
  try {
    const page=await signedPage(browser);
    await page.goto(new URL('chat/?page=nutrition',base).href);
    await expect(page.getByRole('region',{name:'Просмотр страницы'}).getByRole('table')).toBeVisible();
    await page.getByRole('button',{name:'Чат',exact:true}).click();
    let started;const creating=new Promise(resolve=>{started=resolve});
    const pending=new Promise(resolve=>{release=resolve});
    await page.route('**/api/chat/sessions',async route=>{
      if(route.request().method()!=='POST') return route.continue();
      started();await pending;await route.continue();
    });
    await page.getByRole('button',{name:'Новый диалог',exact:true}).first().click();await creating;
    await page.getByRole('button',{name:'Страницы',exact:true}).click();
    await expect(page.getByRole('region',{name:'Просмотр страницы'})).toBeVisible();
    const completed=page.waitForResponse(response=>response.request().method()==='POST' && response.url().endsWith('/api/chat/sessions'));
    release();created=(await(await completed).json()).id;
    await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
    await expect(page).toHaveURL(new URL('chat/?page=nutrition',base).href);
    await expect(page.getByRole('region',{name:'Просмотр страницы'})).toBeVisible();
  } finally {
    release();await browser.close();
    if(created) assert.equal((await fetch(new URL(`api/chat/sessions/${created}`,base),{method:'DELETE',headers:{Cookie:cookie,Origin:new URL(base).origin}})).status,204);
  }
});

test('typing restores composer focus without losing the first character or cursor position', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    const keyboard=await page.context().newCDPSession(page);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    for(const path of ['chat/',`chat/?conversation=${id}`]) {
      await page.goto(new URL(path,base).href);
      await expect(input).toBeFocused();
      for(const [key,text] of [['x','x'],['Shift+!','!'],['Space',' ']]) {
        await input.fill('');
        await input.evaluate(el=>el.blur());
        await page.keyboard.press(key);
        await expect(input).toBeFocused();
        await expect(input).toHaveValue(text);
      }
      await input.fill('до конца');
      await input.evaluate(el=>{el.setSelectionRange(2,3);el.blur();});
      await keyboard.send('Input.dispatchKeyEvent',{type:'keyDown',key:'Ж',text:'Ж',code:'Semicolon',modifiers:8});
      await keyboard.send('Input.dispatchKeyEvent',{type:'keyUp',key:'Ж',code:'Semicolon'});
      await expect(input).toHaveValue('доЖконца');
      await page.keyboard.press('ControlOrMeta+z');
      await expect(input).toHaveValue('до конца');
      await page.reload();
      await expect(input).toHaveValue('до конца');
    }
  } finally {await browser.close();}
});

test('composer focus recovery respects other fields, shortcuts, overlays and hidden chat', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.locator('textarea[aria-label="Сообщение"]');
    await input.fill('Черновик');
    await input.evaluate(el=>el.blur());
    for(const key of ['Control+x','Meta+x','Alt+x','ArrowLeft','Escape','Enter','Tab']) {
      await page.keyboard.press(key);
      await expect(input).not.toBeFocused();
      await expect(input).toHaveValue('Черновик');
    }
    await page.evaluate(()=>{
      document.activeElement?.blur();
      document.body.addEventListener('keydown',event=>event.preventDefault(),{once:true});
    });
    await page.keyboard.press('x');
    await expect(input).not.toBeFocused();
    await expect(input).toHaveValue('Черновик');
    await page.evaluate(()=>document.body.dispatchEvent(new KeyboardEvent('keydown',{key:'x',isComposing:true,bubbles:true})));
    await expect(input).not.toBeFocused();
    await page.getByRole('button',{name:'Найти в диалоге',exact:true}).click();
    const search=page.getByRole('textbox',{name:'Поиск в диалоге',exact:true});
    await expect(search).toBeFocused();
    await page.keyboard.type('query');
    await expect(search).toHaveValue('query');
    await expect(input).toHaveValue('Черновик');
    await page.getByRole('button',{name:'Закрыть поиск',exact:true}).click();
    const searchButton=page.getByRole('button',{name:'Найти в диалоге',exact:true});
    await searchButton.focus();
    await page.keyboard.press('Space');
    await expect(search).toBeVisible();
    await expect(input).toHaveValue('Черновик');
    await page.getByRole('button',{name:'Закрыть поиск',exact:true}).click();
    await page.getByRole('button',{name:'Переименовать диалог',exact:true}).click();
    const title=page.getByRole('textbox',{name:'Название диалога',exact:true});
    await title.fill('Название');
    await page.keyboard.type('x');
    await expect(title).toHaveValue('Названиеx');
    await page.getByRole('dialog').getByRole('button',{name:'Отмена',exact:true}).focus();
    await page.keyboard.press('x');
    await expect(input).not.toBeFocused();
    await expect(input).toHaveValue('Черновик');
    await page.getByRole('dialog').getByRole('button',{name:'Отмена',exact:true}).click();
    await page.locator(`[data-conversation-id="${id}"]`).locator('..').getByRole('button',{name:/Действия с диалогом/}).click();
    await page.keyboard.press('x');
    await expect(page.getByRole('menu')).toBeVisible();
    await expect(input).not.toBeFocused();
    await expect(input).toHaveValue('Черновик');
    await page.keyboard.press('Escape');
    await page.getByRole('button',{name:'Страницы',exact:true}).click();
    await expect(input).toBeHidden();
    await page.evaluate(()=>document.activeElement?.blur());
    await page.keyboard.press('x');
    await expect(input).not.toBeFocused();
    await page.getByRole('button',{name:'Чат',exact:true}).click();
    await expect(input).toHaveValue('Черновик');
  } finally {await browser.close();}
});

test('home composer starts one conversation and moves down without replacing the input', {timeout:60000},async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser),created=[],sent=[];
    page.on('request',request=>{
      if(request.method()==='POST' && new URL(request.url()).pathname.endsWith('/api/chat/sessions')) created.push(request);
      if(request.method()==='POST' && new URL(request.url()).pathname.endsWith('/api/chat/send')) sent.push(request);
    });
    await page.goto(new URL('chat/',base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toBeFocused();
    await expect(input).toHaveJSProperty('readOnly',false);
    assert.equal(created.length,0);
    const view=await page.getByRole('region',{name:'Разговор с Кодриком'}).boundingBox();
    const centered=await page.getByTestId('chat-composer').boundingBox();
    assert.ok(Math.abs(centered.y+centered.height/2-view.y-view.height/2)<30,'Home composer is centered');
    await input.fill('Первый вопрос с главной');
    await page.reload();
    await expect(input).toHaveValue('Первый вопрос с главной');
    await input.evaluate(element=>element.dataset.homeInput='preserved');
    await page.screenshot({path:'/tmp/kodrik-home-composer-desktop.png',fullPage:true,animations:'disabled'});
    await input.press('Enter');await input.press('Enter');
    await expect(page).toHaveURL(/conversation=/);
    await expect(page.locator('.message').getByText('Первый вопрос с главной',{exact:true})).toHaveCount(1);
    await expect(page.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(1);
    assert.equal(created.length,1);assert.equal(sent.length,1);
    await expect(input).toHaveAttribute('data-home-input','preserved');
    await expect(input).toBeFocused();
    await expect(input).toHaveValue('');
    await expect.poll(async()=> (await input.boundingBox()).y).toBeGreaterThan(centered.y+100);
    await page.screenshot({path:'/tmp/kodrik-home-composer-started.png',fullPage:true,animations:'disabled'});
  } finally {await browser.close();}
});

test('mobile app keeps its shell still during swipes while content and normal pages remain scrollable', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'ui'});
  await expect.poll(async()=>(await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{viewport:{width:390,height:844},isMobile:true,hasTouch:true});
    const input=await page.context().newCDPSession(page);
    const swipe=(x,y,distance)=>input.send('Input.synthesizeScrollGesture',{x,y,yDistance:distance,gestureSourceType:'touch',preventFling:true});
    // Emulate the extra document scroll range exposed by iOS standalone mode.
    const addRootOverflow=()=>page.evaluate(()=>{
      const spacer=document.createElement('div');spacer.style.height='200vh';document.body.append(spacer);
    });
    await page.goto(base);
    await expect(page.getByRole('link',{name:'Открыть чат',exact:true})).toBeVisible();
    await addRootOverflow();
    await swipe(200,600,-300);
    await expect.poll(()=>page.evaluate(()=>scrollY)).toBeGreaterThan(0);
    await page.goto(new URL('chat/',base).href);
    const composer=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(composer).toBeVisible();
    await composer.fill('Черновик после свайпа');
    await addRootOverflow();
    await swipe(200,600,-300);
    assert.equal(await page.evaluate(()=>scrollY),0);
    await swipe(200,200,300);
    assert.equal(await page.evaluate(()=>scrollY),0);
    await expect(composer).toHaveValue('Черновик после свайпа');
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const transcript=page.getByTestId('chat-transcript');
    await expect(page.getByRole('heading',{name:'Дневник питания',exact:true})).toBeAttached();
    await transcript.evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new Event('scroll'));});
    await swipe(200,500,-250);
    await expect.poll(()=>transcript.evaluate(el=>el.scrollTop)).toBeGreaterThan(0);
    assert.equal(await page.evaluate(()=>scrollY),0);
  } finally {await browser.close();}
});

test('home composer keeps failed drafts and starts mobile conversations with attachments only', {timeout:60000},async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{viewport:{width:390,height:844},isMobile:true,hasTouch:true});
    await page.goto(new URL('chat/',base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveJSProperty('readOnly',false);
    const composer=page.getByTestId('chat-composer');
    await expect.poll(()=>composer.evaluate(el=>Math.round(el.parentElement.getBoundingClientRect().bottom-el.getBoundingClientRect().bottom))).toBe(0);
    await page.screenshot({path:'/tmp/kodrik-home-composer-mobile.png',fullPage:true,animations:'disabled'});
    await input.fill('Сохранить при ошибке');
    await page.route('**/api/chat/sessions',route=>route.request().method()==='POST'?route.fulfill({status:503,json:{error:'Unavailable'}}):route.continue());
    await input.press('Enter');
    await expect(page.getByRole('alert')).toBeVisible();
    await expect(input).toHaveValue('Сохранить при ошибке');
    await expect(input).toHaveJSProperty('readOnly',false);
    assert.equal(new URL(page.url()).searchParams.has('conversation'),false);
    await page.unroute('**/api/chat/sessions');
    await input.fill('');
    await page.getByRole('button',{name:'Прикрепить файлы и фото',exact:true}).click();
    await page.locator('input[type=file]').setInputFiles({name:'first.txt',mimeType:'text/plain',buffer:Buffer.from('first attachment')});
    await expect(page.getByText('first.txt',{exact:true})).toBeVisible();
    await expect(page.getByText('first.txt',{exact:true})).toBeInViewport();
    await page.screenshot({path:'/tmp/kodrik-home-composer-attachment-mobile.png',fullPage:true,animations:'disabled'});
    await input.press('Enter');
    await expect(page).toHaveURL(/conversation=/);
    await expect(page.locator('.message').getByRole('link',{name:'first.txt',exact:true})).toBeVisible();
    await expect(page.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(1);
    await expect(input).toHaveValue('');
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
  } finally {await browser.close();}
});

test('conversation menus appear on hover or focus and pins survive reload', {timeout:60000},async()=>{
  const older=await(await request('sessions',{})).json();
  const newer=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.goto(new URL('chat/',base).href);
    const rows=page.locator('nav[aria-label="История диалогов"] [data-conversation-id]');
    const row=page.locator(`[data-conversation-id="${older.id}"]`).locator('..');
    const actions=row.locator('button[aria-label^="Действия с диалогом"]');
    await actions.waitFor();
    await page.mouse.move(900,500);
    await expect.poll(()=>actions.evaluate(el=>getComputedStyle(el).opacity)).toBe('0');
    await row.hover();
    await expect.poll(()=>actions.evaluate(el=>getComputedStyle(el).opacity)).toBe('1');
    await page.mouse.move(900,500);
    await actions.focus();
    await expect.poll(()=>actions.evaluate(el=>getComputedStyle(el).opacity)).toBe('1');
    await actions.click();
    await page.getByRole('menuitem',{name:'Закрепить диалог',exact:true}).click();
    await expect(rows.first()).toHaveAttribute('data-conversation-id',older.id);
    await expect(page.locator('[role="menu"]')).toHaveCount(0);
    await page.mouse.move(900,500);
    await page.screenshot({path:'/tmp/kodrik-chat-pins-desktop.png',fullPage:true});
    await page.reload();
    await expect(rows.first()).toHaveAttribute('data-conversation-id',older.id);
    await row.hover();await actions.click();
    await expect(page.getByRole('menuitem',{name:'Открепить диалог',exact:true})).toBeVisible();
    await page.mouse.move(900,500);
    await expect.poll(()=>actions.evaluate(el=>getComputedStyle(el).opacity)).toBe('1');
    await page.getByRole('menuitem',{name:'Открепить диалог',exact:true}).click();
    await expect(rows.first()).toHaveAttribute('data-conversation-id',newer.id);
    const context=await browser.newContext({locale:'ru-RU',viewport:{width:390,height:844},isMobile:true,hasTouch:true});
    const [name,...value]=cookie.split('=');await context.addCookies([{name,value:value.join('='),url:base}]);
    const mobile=await context.newPage();await mobile.goto(new URL('chat/',base).href);
    await mobile.getByRole('button',{name:'Показать диалоги',exact:true}).click();
    const mobileActions=mobile.locator(`[data-conversation-id="${older.id}"]`).locator('..').getByRole('button',{name:/Действия с диалогом/});
    await expect.poll(()=>mobileActions.evaluate(el=>getComputedStyle(el).opacity)).toBe('1');
    await mobileActions.click();await mobile.getByRole('menuitem',{name:'Закрепить диалог',exact:true}).click();
    await expect(mobile.locator('nav[aria-label="История диалогов"] [data-conversation-id]').first()).toHaveAttribute('data-conversation-id',older.id);
    await expect(mobile.locator('[role="menu"]')).toHaveCount(0);
    await mobile.screenshot({path:'/tmp/kodrik-chat-pins-mobile.png',fullPage:true});
    await mobileActions.click();await mobile.getByRole('menuitem',{name:'Открепить диалог',exact:true}).click();
  } finally {await browser.close();}
});

test('message timestamps use the browser timezone and survive reload', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'Проверка времени сообщений'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const snapshot=await(await request(`sessions/${id}/run`)).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'ru-RU',timezoneId:'Europe/Moscow'});
    await page.context().addCookies([{name:cookie.split('=')[0],value:cookie.split('=')[1],url:base}]);
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    for(const message of snapshot.messages) {
      const item=page.locator('.message').filter({has:page.getByText(message.parts[0].text,{exact:true})});
      const time=item.locator('time');
      await time.waitFor({state:'attached'});
      await page.mouse.move(0,0);
      await expect(time).toBeHidden();
      await item.hover();
      await expect(time).toBeVisible();
      await expect(time).toHaveAttribute('datetime',message.created_at);
      await expect(time).toHaveText(`Сегодня, ${new Intl.DateTimeFormat('ru-RU',{
        timeStyle:'short',timeZone:'Europe/Moscow',
      }).format(new Date(message.created_at))}`);
      if(message.role==='user') {
        const stamp=await time.boundingBox();
        const edit=await item.getByRole('button',{name:'Редактировать сообщение',exact:true}).boundingBox();
        assert.ok(stamp.x+stamp.width<=edit.x,'Timestamp sits to the left of edit');
        assert.ok(Math.abs(stamp.y+stamp.height/2-edit.y-edit.height/2)<2,'Timestamp aligns with edit');
      }
      await page.mouse.move(0,0);
      await expect(time).toBeHidden();
    }
    const timestamps=await page.locator('.message time').allTextContents();
    await page.reload();
    await expect(page.locator('.message time')).toHaveText(timestamps);
    await page.locator('.message[data-role="user"]').hover();
    await page.screenshot({path:'/tmp/kodrik-chat-timestamps-desktop.png',fullPage:true});
    await page.setViewportSize({width:390,height:844});
    await page.mouse.move(0,0);
    await expect(page.locator('.message time').first()).toBeHidden();
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-chat-timestamps-mobile.png',fullPage:true});
  } finally {await browser.close();}
});

test('conversation search scrolls to matches and navigates in both directions', {timeout:30000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    const snapshot={type:'snapshot',session_id:id,revision:1,busy:false,cancel_requested:false,phase:'Completed',blocked_reason:null,live:{current:null,interrupted:null},queued:[],
      messages:Array.from({length:40},(_,index)=>({id:`${id}:${index}`,role:index%2?'assistant':'user',parts:[{type:'text',text:`Сообщение ${index}: ${index===2 || index===21?'Поисковый маяк':'Обычный текст'}`}]}))};
    await page.route('**/events',r=>r.fulfill({contentType:'text/event-stream',body:`data: ${JSON.stringify(snapshot)}\n\n`}));
    await page.route('**/run',r=>r.fulfill({json:snapshot}));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const transcript=page.getByTestId('chat-transcript');
    await expect(page.locator('.message')).toHaveCount(40);
    await expect.poll(()=>transcript.evaluate(el=>el.scrollHeight-el.scrollTop-el.clientHeight)).toBeLessThan(2);
    await page.getByRole('button',{name:'Найти в диалоге',exact:true}).click();
    const search=page.getByRole('textbox',{name:'Поиск в диалоге',exact:true});
    await search.fill('Поисковый маяк');
    const selected=page.locator('.message.search-match');
    const expectMatch=async text=>{
      await expect(selected).toContainText(text);
      await expect.poll(()=>selected.evaluate(el=>{
        const item=el.getBoundingClientRect(),view=el.closest('[data-testid="chat-transcript"]').getBoundingClientRect();
        return item.top>=view.top && item.bottom<=view.bottom;
      })).toBe(true);
    };
    await expectMatch('Сообщение 2:');
    await expect(selected).not.toHaveClass(/\bring-2\b/);
    await expect(selected.locator('[data-slot="content"]').first()).toHaveClass(/\bring-2\b/);
    await page.getByRole('button',{name:'Следующее совпадение',exact:true}).click();
    await expectMatch('Сообщение 21:');
    await page.getByRole('button',{name:'Предыдущее совпадение',exact:true}).click();
    await expectMatch('Сообщение 2:');
    await search.press('Enter');
    await expectMatch('Сообщение 21:');
    await search.press('Enter');
    await expectMatch('Сообщение 2:');
  } finally {await browser.close();}
});

test('conversation search highlights literal text in Markdown and clears it when closed', {timeout:30000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    const snapshot={type:'snapshot',session_id:id,revision:1,busy:false,cancel_requested:false,phase:'Completed',blocked_reason:null,live:{current:null,interrupted:null},queued:[],messages:[
      {id:`${id}:0`,role:'user',parts:[{type:'text',text:'Поиск **МАЯК**, ещё маяк. Литерал a+b.'}]},
      {id:`${id}:1`,role:'assistant',parts:[{type:'text',text:'Текст с [маяк](https://example.com) и `маяк`.'}]},
      {id:`${id}:2`,role:'assistant',parts:[{type:'dynamic-tool',toolCallId:'hidden',toolName:'exec_command',state:'output-available',input:{cmd:'маяк'},output:'маяк'}]},
    ]};
    await page.route('**/events',r=>r.fulfill({contentType:'text/event-stream',body:`data: ${JSON.stringify(snapshot)}\n\n`}));
    await page.route('**/run',r=>r.fulfill({json:snapshot}));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await expect(page.locator('.message strong')).toHaveText('МАЯК');
    await expect(page.locator('.message code')).toHaveText('маяк');
    await page.getByRole('button',{name:'Найти в диалоге',exact:true}).click();
    const search=page.getByRole('textbox',{name:'Поиск в диалоге',exact:true});
    const highlights=()=>page.evaluate(()=>Array.from(CSS.highlights.get('conversation-search') ?? [],range=>range.toString()));
    await search.fill(' МаЯк ');
    await expect.poll(highlights).toEqual(['МАЯК','маяк','маяк','маяк']);
    assert.equal(await page.locator('.message strong').evaluate(el=>getComputedStyle(el,'::highlight(conversation-search)').backgroundColor),'rgb(249, 226, 175)');
    await expect(page.locator('.message strong')).toHaveText('МАЯК');
    await expect(page.getByRole('link',{name:'маяк',exact:true})).toHaveAttribute('href','https://example.com');
    await page.screenshot({path:'/tmp/kodrik-chat-search-highlight-desktop.png',fullPage:true});
    await page.emulateMedia({colorScheme:'dark'});
    await page.setViewportSize({width:390,height:844});
    await expect(page.locator('html')).toHaveClass(/dark/);
    await page.screenshot({path:'/tmp/kodrik-chat-search-highlight-mobile-dark.png',fullPage:true});
    snapshot.messages[0].parts[0].text+=' Новый маяк.';
    snapshot.revision++;
    await expect.poll(highlights).toEqual(['МАЯК','маяк','маяк','маяк','маяк']);
    await search.fill('a+b');
    await expect.poll(highlights).toEqual(['a+b']);
    await search.fill('нет совпадений');
    await expect.poll(highlights).toEqual([]);
    await search.fill('маяк');
    await expect.poll(highlights).toEqual(['МАЯК','маяк','маяк','маяк','маяк']);
    await page.getByRole('button',{name:'Закрыть поиск',exact:true}).click();
    await expect.poll(highlights).toEqual([]);
  } finally {await browser.close();}
});

test('chat logo navigates in the current tab', {timeout:30000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const logo=page.getByRole('link',{name:'kodrik',exact:true});
    await expect(logo).toHaveAttribute('target','_self');
    await logo.click();
    await expect(page).toHaveURL(new URL('chat/',base).href);
    assert.equal(page.context().pages().length,1);
  } finally {await browser.close();}
});

test('home Open Chat and sign-in links stay in the current tab', {timeout:30000},async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{locale:'en-US'});
    await page.goto(base);
    await page.emulateMedia({colorScheme:'light'});
    const open=page.getByRole('link',{name:'Open chat',exact:true});
    await expect(open).not.toHaveAttribute('target','_blank');
    await page.screenshot({path:'/tmp/kodrik-home-light.png',fullPage:true});
    await open.click();
    await expect(page).toHaveURL(new URL('chat/',base).href);
    assert.equal(page.context().pages().length,1);
    const anonymous=await browser.newPage({locale:'en-US'});
    await anonymous.goto(base);
    const login=anonymous.getByRole('link',{name:'Sign in',exact:true});
    await expect(login).not.toHaveAttribute('target','_blank');
    await login.click();
    await expect(anonymous).toHaveURL(new URL('auth/login',base).href);
    assert.equal(anonymous.context().pages().length,1);
  } finally {await browser.close();}
});

test('Markdown and document links use the current tab only for local URLs', {timeout:30000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    const local=new URL('chat/',base),external='https://example.com/docs';
    const links=[['Relative',local.pathname,'_self'],['Absolute',local.href,'_self'],['Same host',`//${local.host}${local.pathname}`,'_self'],['Fragment','#inside','_self'],['Query',`?conversation=${id}`,'_self'],['Outside',external,'_blank'],['Other host','//example.com/docs','_blank'],['Other scheme',local.href.replace('http:','https:'),'_blank'],['Other port',`http://${local.hostname}:1/docs`,'_blank']];
    const snapshot={type:'snapshot',session_id:id,revision:1,busy:false,cancel_requested:false,phase:'Completed',blocked_reason:null,live:{current:null,interrupted:null},queued:[],messages:[{id:`${id}:0`,role:'assistant',parts:[
      {type:'text',text:links.map(([name,url])=>`[${name}](${url})`).join('\n\n')},
      {type:'dynamic-tool',toolCallId:'links',toolName:'show_ui',state:'output-available',input:{},output:{url:new URL('pages/preview/',base).href,document:{version:1,title:'Links',sources:[],body:`[Local document](${local.pathname})\n\n[External document](${external})\n\n::u-button{to="${local.pathname}" target="_blank" label="Local button"}\n::\n\n::u-button{to="${external}" target="_self" label="External button"}\n::\n\n::u-link{href="${local.pathname}"}\nLocal UI link\n::`}}},
    ]}]};
    await page.route('**/events',r=>r.fulfill({contentType:'text/event-stream',body:`data: ${JSON.stringify(snapshot)}\n\n`}));
    await page.route('**/run',r=>r.fulfill({json:snapshot}));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    for(const [name,,target] of [...links,['Local document',local.pathname,'_self'],['External document',external,'_blank'],['Local button',local.pathname,'_self'],['External button',external,'_blank'],['Local UI link',local.pathname,'_self']]) {
      const link=page.getByRole('link',{name,exact:true});
      await expect(link).toHaveAttribute('target',target);
    }
    const button=page.getByRole('link',{name:'Local button',exact:true});
    await expect(button).toHaveAttribute('href',local.pathname);
    await button.click();
    await expect(page).toHaveURL(local.href);
    assert.equal(page.context().pages().length,1);
  } finally {await browser.close();}
});

test('conversation content search, rename, inline edits, versions and deletion work together', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'Уникальный маяк для поиска'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser),errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const editButton=page.getByRole('button',{name:'Редактировать сообщение',exact:true});
    await editButton.waitFor();
    await page.mouse.move(0,0);
    const actionsOpacity=()=>editButton.evaluate(el=>getComputedStyle(el.closest('[data-slot="actions"]') ?? el).opacity);
    await expect.poll(actionsOpacity).toBe('0');
    await editButton.focus();
    await expect.poll(actionsOpacity).toBe('1');
    await page.locator('.message[data-role="user"]').hover();
    await editButton.hover();
    const actionLayout=await editButton.evaluate(el=>({
      top:el.getBoundingClientRect().top,
      bubbleBottom:el.closest('[data-slot="body"]').querySelector('[data-slot="content"]').getBoundingClientRect().bottom,
      background:getComputedStyle(el).backgroundColor,
    }));
    assert.ok(actionLayout.top>=actionLayout.bubbleBottom,'Message actions sit below the bubble');
    assert.equal(actionLayout.background,'rgba(0, 0, 0, 0)');
    await editButton.click();
    await page.getByRole('textbox',{name:'Редактирование сообщения',exact:true}).fill('Исправленный вопрос');
    await page.getByRole('button',{name:'Сохранить и отправить',exact:true}).click();
    await expect(page.locator('.message').getByText('Исправленный вопрос',{exact:true})).toHaveCount(1);
    await page.getByRole('button',{name:'Редактировать сообщение',exact:true}).waitFor();
    const branch=new URL(page.url()).searchParams.get('conversation');
    assert.notEqual(branch,id);
    await page.getByRole('button',{name:'Предыдущая версия сообщения',exact:true}).click();
    await expect(page.locator('.message').getByText('Уникальный маяк для поиска',{exact:true})).toHaveCount(1);
    await page.getByRole('button',{name:'Следующая версия сообщения',exact:true}).click();
    await expect(page.locator('.message').getByText('Исправленный вопрос',{exact:true})).toHaveCount(1);
    await page.reload();
    await expect(page.locator('.message').getByText('Исправленный вопрос',{exact:true})).toHaveCount(1);
    await page.locator(`[data-conversation-id="${branch}"]`).locator('..').getByRole('button',{name:/Действия с диалогом/}).click();
    await page.getByRole('menuitem',{name:'Переименовать',exact:true}).click();
    await page.getByRole('textbox',{name:'Название диалога',exact:true}).fill('Поиск и версии');
    await page.getByRole('button',{name:'Сохранить название',exact:true}).click();
    await expect(page.getByRole('button',{name:'Поиск и версии',exact:true})).toHaveCount(1);
    await page.getByRole('button',{name:'Поиск диалогов',exact:true}).click();
    await page.getByRole('textbox',{name:'Поиск диалогов',exact:true}).fill('УНИКАЛЬНЫЙ МАЯК');
    await expect(page.locator('nav[aria-label="История диалогов"] [data-conversation-id]')).toHaveCount(1);
    await page.getByRole('button',{name:'Поиск и версии',exact:true}).click();
    await expect(page.locator('.message').getByText('Уникальный маяк для поиска',{exact:true})).toHaveCount(1);
    await expect(page.getByRole('textbox',{name:'Поиск в диалоге',exact:true})).toHaveValue('УНИКАЛЬНЫЙ МАЯК');
    await page.getByRole('textbox',{name:'Поиск в диалоге',exact:true}).fill('Привет');
    await expect(page.locator('.message.search-match')).toContainText('Привет!');
    await page.screenshot({path:'/tmp/kodrik-chat-management-desktop.png',fullPage:true});
    await page.setViewportSize({width:390,height:844});
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-chat-management-mobile.png',fullPage:true});
    await page.getByRole('button',{name:'Показать диалоги',exact:true}).click();
    await page.getByRole('textbox',{name:'Поиск диалогов',exact:true}).fill('');
    await page.getByRole('button',{name:'Действия с диалогом «Поиск и версии»',exact:true}).click();
    await page.getByRole('menuitem',{name:'Удалить диалог',exact:true}).click();
    await page.getByRole('button',{name:'Удалить навсегда',exact:true}).click();
    await expect(page.getByRole('button',{name:'Поиск и версии',exact:true})).toHaveCount(0);
    assert.equal((await fetch(new URL(`api/chat/sessions/${branch}/run`,base),{headers:{Cookie:cookie}})).status,404);
    assert.deepEqual(errors,[]);
  } finally {await browser.close();}
});

test('native MDC UI persists in chat and uses live paginated sources on its page', {timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'ui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const context=await browser.newContext({locale:'ru-RU',viewport:{width:390,height:844},colorScheme:'light'});
    const [name,...value]=cookie.split('=');await context.addCookies([{name,value:value.join('='),url:base}]);
    const page=await context.newPage(),errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await page.getByRole('heading',{name:'Дневник питания',exact:true}).waitFor();
    await page.getByRole('img',{name:'Калории по дням',exact:true}).waitFor();
    await page.getByRole('img',{name:'Калории по дням',exact:true}).focus();
    await page.keyboard.press('ArrowRight');
    await expect(page.getByRole('tooltip')).toContainText('ккал');
    await page.keyboard.press('Escape');
    await expect(page.getByTestId('ui-document')).toContainText(new Intl.NumberFormat('ru-RU').format(1400));
    await page.getByRole('button',{name:'Добавить приём пищи',exact:true}).click();
    await expect(page.getByRole('textbox',{name:'Сообщение',exact:true})).toHaveValue('Помоги добавить приём пищи');
    assert.equal(await page.getByRole('button',{name:'Далее',exact:true}).count(),0);
    await page.getByRole('textbox',{name:'Фильтр таблицы',exact:true}).fill('Breakfast');
    await expect(page.getByRole('table')).toContainText('Breakfast');
    await expect(page.getByRole('table')).not.toContainText('Lunch');
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-mdc-chat-mobile.png',fullPage:true});
    const url=await page.getByRole('link',{name:'Открыть страницу',exact:true}).getAttribute('href');
    await page.goto(url);
    await page.getByRole('heading',{name:'Дневник питания',exact:true}).waitFor();
    assert.equal(page.url(),url);
    await page.getByRole('button',{name:'Далее',exact:true}).click();
    await expect(page.getByRole('table')).toContainText('Dinner');
    await expect(page.getByRole('table')).not.toContainText('Breakfast');
    await page.getByRole('button',{name:'Назад',exact:true}).click();
    await expect(page.getByRole('table')).toContainText('Breakfast');
    await page.emulateMedia({colorScheme:'dark'});await page.setViewportSize({width:1280,height:900});
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-mdc-page-desktop.png',fullPage:true});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await page.getByRole('heading',{name:'Дневник питания',exact:true}).waitFor();
    await expect(page.getByRole('table')).toContainText('Breakfast');
    assert.equal(await page.getByRole('button',{name:'Далее',exact:true}).count(),0);
    assert.deepEqual(errors,[]);
  } finally {await browser.close();}
});

test('unknown MDC components show an error without mounting app components',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();await request('send',{id,text:'unsafeui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const context=await browser.newContext({locale:'ru-RU'});const [name,...value]=cookie.split('=');await context.addCookies([{name,value:value.join('='),url:base}]);
    const page=await context.newPage();await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await page.getByTestId('ui-document').getByText('Не удалось отобразить документ',{exact:true}).waitFor();
    await expect(page.getByTestId('ui-document')).toContainText('nuxt-link');
    await expect(page.getByTestId('ui-document').getByRole('alert')).toHaveClass(/text-error/);
    assert.equal(await page.locator('[href^="javascript:"]').count(),0);
  } finally {await browser.close();}
});

test('Nuxt UI components retain local state, named slots, scoped cells and native charts',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();await request('send',{id,text:'componentsui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try{
    const context=await browser.newContext({locale:'ru-RU',viewport:{width:390,height:844}});
    const [name,...value]=cookie.split('=');await context.addCookies([{name,value:value.join('='),url:base}]);
    const page=await context.newPage(),errors=[];page.on('pageerror',error=>errors.push(error.message));
    page.on('console',message=>{if(message.type()==='error') console.error(message.text())});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const document=page.getByTestId('ui-document');
    await expect(document).toContainText('Заголовок слота');
    await expect(document.locator('code')).toHaveText('value');
    await expect(document.locator('#styled-card')).toHaveCSS('text-align','center');
    await expect(page.getByRole('tab',{name:'Второй',exact:true}),await document.innerText()).toBeVisible({timeout:5000});
    await page.getByRole('tab',{name:'Второй',exact:true}).click();
    await expect(page.getByRole('tabpanel')).toContainText('Вторая вкладка');
    await expect(document).toContainText('Первый слайд');
    await expect(document).toContainText('Второй слайд');
    await page.getByRole('checkbox',{name:'Проверенный флажок',exact:true}).check();
    await expect(page.getByRole('checkbox',{name:'Проверенный флажок',exact:true})).toBeChecked();
    await page.getByPlaceholder('Локальное поле').fill('Изменённое');
    await expect(page.getByPlaceholder('Локальное поле')).toHaveValue('Изменённое');
    await page.getByRole('combobox').filter({hasText:'Выберите вариант'}).click();
    await page.getByRole('option',{name:'Бета',exact:true}).click();
    await expect(page.getByRole('combobox').filter({hasText:'Бета'})).toBeVisible();
    await page.getByRole('button',{name:'Открыть окно',exact:true}).click();
    await expect(page.getByRole('dialog')).toContainText('Содержимое окна');
    await page.keyboard.press('Escape');
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await expect(page.getByRole('table').getByText('Альфа',{exact:true})).toBeVisible();
    await expect(page.getByRole('table').getByText('Бета',{exact:true})).toBeVisible();
    await expect(document.locator('svg')).not.toHaveCount(0);
    const single=page.getByRole('img',{name:'Одна точка',exact:true});
    await expect(single).toBeVisible();
    for(const path of await single.locator('path').evaluateAll(paths=>paths.map(path=>path.getAttribute('d')))) assert.doesNotMatch(path||'',/NaN|Infinity/);
    await expect(document).toContainText('Нет данных для графика.');
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-ui-components-mobile.png',fullPage:true});
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test('computed views render three live series and refresh progress without republishing',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();await request('send',{id,text:'viewsui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser),errors=[];page.on('pageerror',error=>errors.push(error.message));
    await page.setViewportSize({width:390,height:844});
    const url=new URL('pages/data-views/document.json',base).href;
    const response=await page.request.get(url);
    assert.equal(response.status(),200);
    const before=await response.json();
    assert.equal(before.views.week.length,7);
    await page.goto(new URL('pages/data-views/',base).href);
    const chart=page.getByRole('img',{name:'КБЖУ за неделю',exact:true});
    await expect(chart).toBeVisible();
    for(const label of ['Белки','Жиры','Углеводы']) await expect(chart.getByText(label,{exact:true})).toBeVisible();
    const progress=page.locator('[aria-label="Калории"]').getByRole('progressbar');
    await expect(progress).toHaveAttribute('aria-valuenow','1900');
    await expect(progress).toHaveAttribute('aria-valuemax','2200');
    const composite=page.getByRole('progressbar',{name:'Составной белок',exact:true}),segments=composite.locator('[data-slot="segment"]');
    await expect(composite).toHaveAttribute('aria-valuenow','100');
    await expect(composite).toHaveAttribute('aria-valuemax','100');
    await expect(composite).toHaveAttribute('aria-valuetext','Основное: 80; Превышение: 20');
    await expect(page.locator('[data-slot="status"]')).toHaveText('100%');
    await expect(segments).toHaveCount(2);
    const track=await composite.boundingBox(),first=await segments.nth(0).boundingBox(),second=await segments.nth(1).boundingBox();
    assert.ok(Math.abs(first.width/track.width-.8)<.01);
    assert.ok(Math.abs(second.width/track.width-.2)<.01);
    assert.ok(Math.abs(first.x+first.width-second.x)<1);
    await expect(segments.nth(1)).toHaveCSS('background-color','rgb(210, 15, 57)');
    await page.emulateMedia({colorScheme:'dark'});
    await expect(segments.nth(1)).toHaveCSS('background-color','rgb(243, 139, 168)');
    await page.emulateMedia({colorScheme:'light'});
    for(const row of before.views.week) await expect(page.getByRole('table').getByRole('cell',{name:row.date,exact:true})).toBeVisible();
    await page.getByLabel('Калории дня',{exact:true}).fill('2100');
    await page.getByLabel('Белки дня',{exact:true}).fill('90');
    await page.getByRole('button',{name:'Сохранить день',exact:true}).click();
    await expect(progress).toHaveAttribute('aria-valuenow','2100');
    await expect(composite).toHaveAttribute('aria-valuenow','90');
    await expect(page.locator('[data-slot="status"]')).toHaveText('90%');
    assert.ok(Math.abs((await segments.nth(0).boundingBox()).width/(await composite.boundingBox()).width-.7)<.01);
    assert.ok(Math.abs((await segments.nth(1).boundingBox()).width/(await composite.boundingBox()).width-.2)<.01);
    const after=await(await page.request.get(url)).json();
    assert.equal(after.views.latest.kcal,2100);
    assert.equal(after.sources[0].url,before.sources[0].url);
    for(const path of await chart.locator('path').evaluateAll(paths=>paths.map(path=>path.getAttribute('d')))) assert.doesNotMatch(path||'',/NaN|Infinity/);
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-ui-views.png',fullPage:true});
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test('range chart shades min/max and updates the midpoint with keyboard tooltips',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();await request('send',{id,text:'rangeui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser),errors=[];page.on('pageerror',error=>errors.push(error.message));
    await page.goto(new URL('pages/range-chart/',base).href);
    const chart=page.getByRole('img',{name:'Коридор калорий',exact:true}),band=chart.locator('path[fill-opacity="0.2"]');
    await expect(chart).toBeVisible();
    await expect(band).toHaveCount(1);
    await expect(band).toHaveAttribute('fill','var(--ts-chart-1)');
    assert.equal(await band.getAttribute('stroke'),null);
    await expect(chart.locator('path[data-ts-key^="range-lines:"]')).toHaveCount(2);
    await expect(chart.getByText('Среднее',{exact:true})).toBeVisible();
    await expect(chart.getByText('Ориентир',{exact:true})).toBeVisible();
    assert.equal(await chart.getByText('Минимум',{exact:true}).count(),0);
    assert.equal(await chart.getByText('Максимум',{exact:true}).count(),0);
    await chart.focus();await page.keyboard.press('ArrowRight');
    await expect(page.getByRole('tooltip')).toContainText('Коридор:');
    await expect(page.getByRole('tooltip')).toContainText('Среднее:');
    await page.keyboard.press('Escape');
    const before=await band.getAttribute('d');
    await page.getByLabel('Верхняя оценка',{exact:true}).fill('220');
    await page.getByRole('button',{name:'Сохранить оценку',exact:true}).click();
    await expect.poll(()=>band.getAttribute('d')).not.toBe(before);
    await chart.focus();await page.keyboard.press('End');
    await expect(page.getByRole('tooltip')).toContainText('120 — 220');
    await expect(page.getByRole('tooltip')).toContainText('Среднее: 170');
    await page.keyboard.press('Escape');
    await page.screenshot({path:'/tmp/kodrik-range-chart.png',fullPage:true});
    await page.emulateMedia({colorScheme:'dark'});
    await expect(band).toHaveAttribute('fill-opacity','0.2');
    for(const d of await chart.locator('path').evaluateAll(paths=>paths.map(path=>path.getAttribute('d')))) assert.doesNotMatch(d||'',/NaN|Infinity/);
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test('four-column MDC grids adapt to desktop, tablet and mobile',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();await request('send',{id,text:'gridui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser),errors=[];page.on('pageerror',error=>errors.push(error.message));
    await page.setViewportSize({width:1280,height:900});
    await page.goto(new URL('pages/kbju-grid/',base).href);
    const cards=['Калории','Белки','Жиры','Углеводы'].map(name=>page.getByRole('heading',{name,exact:true}));
    for(const card of cards) await expect(card).toBeVisible();
    const positions=async()=>Promise.all(cards.map(card=>card.boundingBox()));
    const desktop=await positions();
    assert.ok(desktop.every(point=>Math.abs(point.y-desktop[0].y)<1));
    assert.ok(desktop[0].x<desktop[1].x&&desktop[1].x<desktop[2].x&&desktop[2].x<desktop[3].x);
    await page.screenshot({path:'/tmp/kodrik-kbju-grid-desktop.png',fullPage:true});
    await page.setViewportSize({width:800,height:900});
    await expect.poll(async()=>{const p=await positions();return Math.abs(p[0].y-p[1].y)<1&&p[2].y>p[0].y&&Math.abs(p[2].y-p[3].y)<1}).toBe(true);
    await page.setViewportSize({width:390,height:844});
    await expect.poll(async()=>{const p=await positions();return p.every((point,index)=>index===0||point.y>p[index-1].y)}).toBe(true);
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test('table headers and date formats preserve raw sorting and pagination',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();await request('send',{id,text:'datesui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser),errors=[];page.on('pageerror',error=>errors.push(error.message));
    await page.goto(new URL('pages/date-formats/',base).href);
    const tables=page.getByRole('table'),history=tables.nth(0),native=tables.nth(1),paged=tables.nth(2);
    await expect(history.getByRole('columnheader')).toHaveText(['День','Энергия, ккал']);
    await expect(history.getByRole('cell')).toHaveText(['02.01.2026','10','31.12.2025','20','01.02.2026','30']);
    await history.getByRole('button',{name:'День',exact:true}).click();
    await expect(history.getByRole('cell')).toHaveText(['31.12.2025','20','02.01.2026','10','01.02.2026','30']);
    await expect(native.getByRole('columnheader')).toHaveText(['Дата записи','Калории']);
    await expect(native.getByRole('cell').nth(0)).toHaveText('31.12.2025');
    await expect(native.getByRole('cell').nth(1)).toHaveText('20 — 31 декабря');
    await expect(page.getByTestId('ui-document')).toContainText('Дата в тексте: 2 января 2026');
    await expect(page.getByTestId('ui-document')).toContainText('Дата с часовым поясом: 22.10.2026');
    await expect(page.getByTestId('ui-document')).toContainText('Пустая дата: Нет данных');
    await expect(paged.getByRole('columnheader')).toHaveText(['Блюдо','Когда']);
    await expect(paged).toContainText('Breakfast');
    await page.getByRole('button',{name:'Далее',exact:true}).click();
    await expect(paged).toContainText('Dinner');
    await expect(paged).not.toContainText('Breakfast');
    await expect(paged.getByRole('columnheader')).toHaveText(['Блюдо','Когда']);
    await expect(paged.getByRole('cell').nth(1)).toHaveText('21.10.2026');
    const snapshot=await(await page.request.get(new URL('pages/date-formats/document.json',base).href)).json();
    assert.equal(snapshot.sources[0].data[0].date,'2026-01-02');
    await page.screenshot({path:'/tmp/kodrik-date-formats.png',fullPage:true});
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test('private MDC forms insert and edit dataset records and refresh dependent views',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();await request('send',{id,text:'formsui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try{
    const context=await browser.newContext({locale:'ru-RU'});const [name,...value]=cookie.split('=');await context.addCookies([{name,value:value.join('='),url:base}]);
    const page=await context.newPage(),errors=[];page.on('pageerror',error=>errors.push(error.message));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await expect(page.getByRole('button',{name:'Добавить запись',exact:true})).toBeDisabled();
    const url=await page.getByRole('link',{name:'Открыть страницу',exact:true}).getAttribute('href');await page.goto(url);
    await page.getByLabel('Дата нового приёма',{exact:true}).fill('2026-10-22');
    await page.getByLabel('Новое блюдо',{exact:true}).fill('Snack');
    await page.getByLabel('Новые калории',{exact:true}).fill('100');
    await page.getByRole('button',{name:'Добавить запись',exact:true}).click();
    await expect(page.getByRole('table')).toContainText('Snack');
    await expect(page.getByTestId('ui-document')).toContainText('450');
    await expect(page.getByRole('status')).toContainText('Запись сохранена');
    await page.getByLabel('Изменить блюдо',{exact:true}).fill('Updated breakfast');
    await page.getByLabel('Изменить калории',{exact:true}).fill('400');
    await page.getByRole('button',{name:'Сохранить изменения',exact:true}).click();
    await expect(page.getByRole('table')).toContainText('Updated breakfast');
    await expect(page.getByTestId('ui-document')).toContainText('500');
    await page.reload();
    await expect(page.getByRole('table')).toContainText('Snack');
    await expect(page.getByLabel('Изменить калории',{exact:true})).toHaveValue('400');
    await page.getByRole('button',{name:'Сохранить значения по умолчанию',exact:true}).click();
    await expect(page.getByRole('table')).toContainText('Untouched defaults');
    await expect(page.getByTestId('ui-document')).toContainText('550');
    await page.getByLabel('Дополнительное число',{exact:true}).fill('');
    await page.getByLabel('Необязательная дата',{exact:true}).fill('');
    await page.getByRole('button',{name:'Сохранить изменения',exact:true}).click();
    await expect.poll(async()=>{
      const snapshot=await(await context.request.get(new URL('document.json',page.url()).href)).json();
      return snapshot.actions.find(action=>action.id==='edit').record.values;
    }).toEqual({meal:'Updated breakfast',kcal:400});
    await page.reload();
    await expect(page.getByLabel('Дополнительное число',{exact:true})).toHaveValue('');
    await expect(page.getByLabel('Необязательная дата',{exact:true})).toHaveValue('');
    await page.screenshot({path:'/tmp/kodrik-ui-forms-desktop.png',fullPage:true});
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test('JSON admission and snapshots preserve tools and canonical text', async () => {
  for (const text of ['hello','tools','retry']) {
    const {id}=await (await request('sessions',{})).json();
    const accepted=await (await request('send',{id,text})).json();assert.ok(accepted.submission_id);
    let view;
    await expect.poll(async()=>{view=await (await request(`sessions/${id}/run`)).json();return view.busy;}).toBe(false);
    assert.equal(view.messages.at(-1).parts.at(-1).text,'Привет!');
    assert.equal(view.live.current,null);
    if(text==='tools') {assert.equal(view.messages.at(-1).parts[0].state,'output-available');assert.equal(view.plan.plan[0].status,'completed');}
    assert.deepEqual((await (await request(`sessions/${id}/run`)).json()).messages,view.messages);
  }
});

test('LLM titles update the sidebar and navbar without reload and survive refresh',{timeout:30000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const entry=page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${id}"]`);
    await expect(entry).toHaveAttribute('title','Новый диалог');
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveJSProperty('readOnly',false);
    await expect(input).toBeFocused();
    await input.fill('Меню на неделю');await input.press('Enter');
    await expect(entry).toHaveAttribute('title','Обсуждение: Меню на неделю');
    await expect(page.getByRole('heading',{name:'Обсуждение: Меню на неделю',exact:true})).toBeVisible();
    await page.reload();
    await expect(entry).toHaveAttribute('title','Обсуждение: Меню на неделю');
    await expect(input).toHaveJSProperty('readOnly',false);
    await expect(input).toBeFocused();
    await input.fill('Другой вопрос');await input.press('Enter');
    await expect(page.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(2);
    await expect(entry).toHaveAttribute('title','Обсуждение: Меню на неделю');
    await page.getByRole('button',{name:'Новый диалог',exact:true}).first().click();
    await expect.poll(()=>new URL(page.url()).searchParams.get('conversation')).not.toBe(id);
    const otherId=new URL(page.url()).searchParams.get('conversation');
    await input.fill('delayed title');await input.press('Enter');
    await expect(page.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(1);
    const otherEntry=page.locator(`[data-conversation-id="${otherId}"]`);
    await expect(otherEntry.getByTestId('title-shimmer')).toBeVisible();
    await expect(page.getByTestId('navbar-title-shimmer')).toBeVisible();
    await entry.click();
    await expect(entry).toHaveAttribute('aria-current','page');
    await expect(page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${otherId}"]`)).toHaveAttribute('title','Обсуждение: delayed title');
    await expect(otherEntry.getByTestId('title-shimmer')).toHaveCount(0);
  } finally {await browser.close();}
});

test('active conversation shows activity before its title', {timeout:30000}, async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.route('**/api/chat/sessions?*',async route=>{
      const response=await route.fetch();
      const sessions=await response.json();
      await route.fulfill({response,json:sessions.map(session=>session.id===id?{...session,busy:true}:session)});
    });
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const entry=page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${id}"]`);
    await expect(entry.getByLabel('Выполняется')).toBeVisible();
    assert.ok(await entry.evaluate(el=>el.querySelector('[aria-label="Выполняется"]').compareDocumentPosition(el.querySelector('span.text-left')) & Node.DOCUMENT_POSITION_FOLLOWING));
  } finally {await browser.close();}
});

test('mobile chat fits narrow screens and the visual keyboard viewport without losing the draft',{timeout:30000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'tools'});
  await expect.poll(async()=>(await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const context=await browser.newContext({locale:'ru-RU',viewport:{width:320,height:568},isMobile:true,hasTouch:true,deviceScaleFactor:2});
    const [name,...value]=cookie.split('=');await context.addCookies([{name,value:value.join('='),url:base}]);
    const page=await context.newPage(),errors=[];page.on('pageerror',error=>errors.push(error.message));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toBeFocused();
    const editButton=page.getByRole('button',{name:'Редактировать сообщение',exact:true});
    await expect.poll(()=>editButton.evaluate(el=>getComputedStyle(el.closest('[data-slot="actions"]') ?? el).opacity)).toBe('1');
    const actionFits=await editButton.evaluate(el=>({
      top:el.getBoundingClientRect().top,
      bubbleBottom:el.closest('[data-slot="body"]').querySelector('[data-slot="content"]').getBoundingClientRect().bottom,
    }));
    assert.ok(actionFits.top>=actionFits.bubbleBottom,'Touch actions sit below the bubble');
    assert.equal(await input.evaluate(el=>getComputedStyle(el).fontSize),'16px');
    await expect(page.getByText('Enter — отправить · Shift + Enter — новая строка',{exact:true})).toBeHidden();
    const checkIcon=async name=>{
      const geometry=await page.getByRole('button',{name,exact:true}).evaluate(el=>{
        const button=el.getBoundingClientRect(),icon=el.querySelector('[data-slot="leadingIcon"]').getBoundingClientRect();
        const x=button.x+button.width/2,y=button.y+button.height/2;
        return {dx:icon.x+icon.width/2-x,dy:icon.y+icon.height/2-y,
          touchable:[[-21,0],[21,0],[0,-21],[0,21]].every(([dx,dy])=>el.contains(document.elementFromPoint(x+dx,y+dy)))};
      });
      assert.ok(Math.abs(geometry.dx)<1 && Math.abs(geometry.dy)<1,`${name}: centered icon ${JSON.stringify(geometry)}`);
      assert.ok(geometry.touchable,`${name}: 44px touch area`);
    };
    for(const name of ['Прикрепить файлы и фото','Отправить']) await checkIcon(name);
    for(const button of ['Показать диалоги','Переименовать диалог']) {
      const bounds=await page.getByRole('button',{name:button,exact:true}).boundingBox();
      assert.ok(bounds.height>=44 && bounds.width>=44,`${button}: touch target`);
    }
    await expect(page.getByRole('button',{name:'Переименовать диалог',exact:true})).toHaveCSS('opacity','1');
    const checkFits=async(height,top=0)=>{
      await expect(async()=>{
        const {shell,composer,transcript,send,overflow}=await page.evaluate(()=>{
          const rect=selector=>document.querySelector(selector).getBoundingClientRect().toJSON();
          return {shell:rect('.chat-viewport'),composer:rect('[data-testid="chat-composer"]'),transcript:rect('[data-testid="chat-transcript"]'),
            send:rect('.chat-prompt button:last-child'),overflow:document.documentElement.scrollWidth>innerWidth || document.documentElement.scrollHeight>innerHeight+1};
        });
        assert.equal(Math.round(shell.height),height);
        assert.ok(composer.y>=top && composer.bottom<=height+top+1,JSON.stringify(composer));
        assert.ok(send.y>=composer.y && send.bottom<=composer.bottom,JSON.stringify({composer,send}));
        assert.ok(transcript.height>30,JSON.stringify(transcript));
        assert.equal(overflow,false);
      }).toPass({timeout:5000});
    };
    await checkFits(568);
    await input.fill('Мобильный черновик');
    await page.evaluate(()=>{
      Object.defineProperties(visualViewport,{height:{configurable:true,value:330},offsetTop:{configurable:true,value:38}});
      visualViewport.dispatchEvent(new Event('resize'));visualViewport.dispatchEvent(new Event('scroll'));
    });
    await expect.poll(()=>page.locator('.chat-viewport').first().evaluate(el=>Math.round(el.getBoundingClientRect().height))).toBe(330);
    await checkFits(330,38);
    await page.getByTestId('composer-plan').getByRole('button').click();
    await checkFits(330,38);
    await input.fill(Array(20).fill('Длинный черновик').join('\n'));await checkFits(330,38);
    await input.fill('Мобильный черновик');
    await expect(input).toHaveValue('Мобильный черновик');
    await page.screenshot({path:'/tmp/kodrik-chat-mobile-keyboard.png'});
    await page.evaluate(()=>{delete visualViewport.height;delete visualViewport.offsetTop;visualViewport.dispatchEvent(new Event('resize'));});
    await page.getByRole('button',{name:'Показать диалоги',exact:true}).click();
    const dialog=page.getByRole('dialog');await expect(dialog).toBeVisible();
    await expect.poll(()=>dialog.evaluate(el=>Math.round(el.getBoundingClientRect().x))).toBe(0);
    const logo=await dialog.getByRole('link',{name:'kodrik',exact:true}).boundingBox();
    for(const name of ['Язык','Переключить тему','Выйти']) {
      const bounds=await dialog.getByRole('button',{name,exact:true}).boundingBox();
      assert.ok(bounds.x>=logo.x+logo.width,`${name}: to the right of the logo`);
      assert.ok(Math.abs(bounds.y+bounds.height/2-logo.y-logo.height/2)<1,`${name}: same header row`);
      assert.ok(bounds.x+bounds.width<=320,`${name}: fits the narrow screen`);
      await checkIcon(name);
    }
    await expect(dialog.getByRole('button',{name:'Выйти',exact:true})).toBeVisible();
    const dialogSearch=dialog.getByRole('textbox',{name:'Поиск диалогов',exact:true});
    const dialogSearchToggle=dialog.getByRole('button',{name:'Поиск диалогов',exact:true});
    await expect(dialogSearch).toHaveCount(0);
    await dialogSearchToggle.click();
    await expect(dialogSearch).toBeFocused();
    await page.screenshot({path:'/tmp/kodrik-chat-mobile-menu.png'});
    await dialogSearch.fill('нет такого диалога');
    await expect(dialog.getByText('Диалоги не найдены.',{exact:true})).toBeVisible();
    await dialogSearch.press('Escape');
    await expect(dialogSearch).toHaveCount(0);
    await expect(dialogSearchToggle).toBeFocused();
    await expect(dialog.getByRole('navigation',{name:'История диалогов'}).locator('[data-conversation-id]').first()).toBeVisible();
    await page.keyboard.press('Escape');await expect(dialog).toHaveCount(0);
    await page.setViewportSize({width:844,height:390});await checkFits(390);
    await page.setViewportSize({width:390,height:844});await checkFits(844);
    await page.screenshot({path:'/tmp/kodrik-chat-mobile-app.png'});
    await page.reload();await expect(input).toBeFocused();await expect(input).toHaveValue('Мобильный черновик');
    const manifest=await page.locator('link[rel="manifest"]').getAttribute('href');
    assert.equal(manifest,new URL('chat/manifest.webmanifest',base).pathname);
    await input.fill('wait');await input.press('Enter');
    await page.getByText('Жду остановки',{exact:true}).waitFor();
    await checkIcon('Остановить');
    await page.getByRole('button',{name:'Остановить',exact:true}).click();
    await expect(page.getByRole('button',{name:'Отправить',exact:true})).toBeVisible();
    assert.deepEqual(errors,[]);
  } finally {await browser.close();}
});

test('chat UX preserves drafts and attachments across conversations and permits composing while busy',{timeout:60000},async()=>{
  const first=await(await request('sessions',{})).json(),second=await(await request('sessions',{})).json();
  await request('send',{id:first.id,text:'draft conversation'});
  await expect.poll(async()=>(await(await request(`sessions/${first.id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try{
    const page=await signedPage(browser);
    await page.goto(new URL(`chat/?conversation=${first.id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('Сохранённый черновик');
    await input.evaluate(el=>{const data=new DataTransfer();data.items.add(new File(['draft'],'draft.txt',{type:'text/plain'}));el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));});
    await page.getByRole('button',{name:'Новый диалог',exact:true}).first().click();
    await expect(input).toHaveValue('');
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${first.id}"]`).click();
    await expect(input).toHaveValue('Сохранённый черновик');
    await expect(page.getByText('draft.txt',{exact:true})).toBeVisible();
    await page.reload();
    await expect(input).toHaveValue('Сохранённый черновик');
    await expect(page.getByText('Повторно выберите вложения: draft.txt',{exact:false})).toBeVisible();
    await input.fill('wait');await input.press('Enter');
    await page.getByText('Жду остановки',{exact:true}).waitFor();
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('Следующий вопрос');
    await expect(input).toHaveValue('Следующий вопрос');
    await expect(page.getByRole('button',{name:'Остановить',exact:true})).toHaveCount(0);
    await request(`sessions/${first.id}/stop`,{});
    await expect(page.getByRole('button',{name:'Отправить',exact:true})).toBeEnabled();
    await page.reload();await expect(input).toHaveValue('Следующий вопрос');
    await page.getByRole('button',{name:'Поиск диалогов',exact:true}).click();
    await page.getByRole('textbox',{name:'Поиск диалогов',exact:true}).fill('нет такого названия');
    await expect(page.getByText('Диалоги не найдены.',{exact:true})).toBeVisible();
    await expect(input).toHaveValue('Следующий вопрос');
    await page.getByRole('textbox',{name:'Поиск диалогов',exact:true}).fill('draft conversation');
    await expect(page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${first.id}"]`)).toBeVisible();
    await page.getByRole('button',{name:'Поиск диалогов',exact:true}).click();
    await expect(page.getByRole('navigation',{name:'История диалогов'}).locator('time').first()).toBeVisible();
    const listed=await(await request('sessions')).json();
    assert.ok(listed.find(session=>session.id===first.id).updated_at);
    assert.equal(listed.find(session=>session.id===second.id).updated_at,null);
  }finally{await browser.close();}
});

test('web chat steers the current answer after a tool with text, files and photos',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try{
    const page=await signedPage(browser);await page.setViewportSize({width:390,height:844});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveJSProperty('readOnly',false);
    const photo=await page.screenshot({clip:{x:0,y:0,width:2,height:2}});
    await input.fill('slowtool');await input.press('Enter');
    await expect(page.getByRole('button',{name:'Выполняются шаги: 1',exact:true})).toBeVisible();
    await expect(page.getByRole('button',{name:'Выполняются шаги: 1',exact:true})).toHaveAttribute('data-slot','trigger');
    await expect(page.locator('.message [data-slot="leadingAvatar"]')).toHaveCount(0);
    await input.fill('queued browser text');
    await expect(page.getByRole('button',{name:'Отправить',exact:true})).toBeEnabled();
    await expect(page.getByRole('button',{name:'Остановить',exact:true})).toHaveCount(0);
    await page.getByRole('button',{name:'Отправить',exact:true}).click();
    await expect(page.locator('.message').getByText('queued browser text',{exact:true})).toHaveCount(1);
    await expect(page.getByRole('button',{name:'Остановить',exact:true})).toBeVisible();
    await expect(input).toHaveValue('');await expect(input).toHaveJSProperty('readOnly',false);
    await input.evaluate((el,bytes)=>{const data=new DataTransfer();data.items.add(new File(['queued browser file'],'queued-browser.txt',{type:'text/plain'}));data.items.add(new File([new Uint8Array(bytes)],'queued-photo.png',{type:'image/png'}));el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));},[...photo]);
    await input.press('Enter');
    await expect(page.locator('.message').getByText('queued-browser.txt',{exact:true})).toHaveCount(1);
    await page.reload();
    await expect(page.locator('.message').getByText('queued browser text',{exact:true})).toHaveCount(1);
    await expect(page.locator('.message').getByText('queued-browser.txt',{exact:true})).toHaveCount(1);
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-chat-queue-mobile.png',fullPage:true});
    await expect(page.locator('.message').getByRole('link',{name:'queued-browser.txt',exact:true})).toHaveCount(1);
    await expect(page.locator('.message img')).toHaveJSProperty('naturalWidth',2);
    await expect(page.locator('.message').getByText('Ожидает передачи агенту',{exact:true})).toHaveCount(0);
    await expect(page.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(1);
    const snapshot=await(await request(`sessions/${id}/run`)).json();
    assert.deepEqual(snapshot.messages.filter(m=>m.role==='user').map(m=>m.parts.find(p=>p.type==='text')?.text||m.parts.find(p=>p.filename==='queued-browser.txt')?.filename),['slowtool','queued browser text','queued-browser.txt']);
  }finally{await browser.close();}
});

test('web chat steers a changed draft after uncertain admission and Stop cancels pending steering',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json(),requestId=crypto.randomUUID();
  await request('send',{id,request_id:requestId,text:'wait'});
  const browser=await chromium.launch({headless:true});
  try{
    const page=await signedPage(browser);
    await page.addInitScript(({id,requestId})=>{
      sessionStorage.setItem(`kodrik.pending.${id}`,JSON.stringify({id:requestId,text:'wait',files:[]}));
      sessionStorage.setItem(`kodrik.draft.${id}`,JSON.stringify({text:'queued after uncertain response',files:[]}));
    },{id,requestId});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(page.getByRole('button',{name:'Отправить',exact:true})).toBeEnabled();
    await input.press('Enter');
    await expect(page.locator('.message').getByText('queued after uncertain response',{exact:true})).toHaveCount(1);
    await expect(input).toHaveValue('');
    await page.getByRole('button',{name:'Остановить',exact:true}).click();
    await expect(page.locator('.message').getByText('queued after uncertain response',{exact:true})).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Остановить',exact:true})).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Продолжить очередь',exact:true})).toHaveCount(0);
    await input.fill('new message after stop');await input.press('Enter');
    await expect(page.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(1);
    const snapshot=await(await request(`sessions/${id}/run`)).json();
    assert.deepEqual(snapshot.messages.filter(m=>m.role==='user').map(m=>m.parts[0].text),['wait','new message after stop']);
  }finally{await browser.close();}
});

test('composer switches one action and keeps its plan and hints attached on mobile',{timeout:30000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);await page.setViewportSize({width:390,height:844});
    const snapshot={type:'snapshot',session_id:id,run_id:'active',revision:1,busy:true,cancel_requested:false,phase:'NeedModel',blocked_reason:null,live:{current:null,interrupted:null},queued:[],
      messages:[{id:'initial',role:'user',parts:[{type:'text',text:'Сделай страницу'}]}],
      plan:{explanation:'Сначала данные, затем проверка результата.',plan:[{step:'Подготовить данные',status:'completed'},{step:'Создать страницу и проверить длинный заголовок текущего шага на телефоне',status:'in_progress'},{step:'Проверить результат',status:'pending'}]}};
    await page.route('**/events',r=>r.fulfill({contentType:'text/event-stream',body:`data: ${JSON.stringify(snapshot)}\n\n`}));
    await page.route('**/run',r=>r.fulfill({json:snapshot}));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    const form=page.locator('form').filter({has:input});
    const working=page.getByTestId('chat-transcript').getByText('Кодрик работает…',{exact:true});
    await expect(working).toBeVisible();
    await expect(working).toHaveAttribute('data-slot','base');
    await expect(page.getByText('Кодрик работает…',{exact:true})).toHaveCount(1);
    await expect(form.getByRole('button',{name:'Остановить',exact:true})).toBeVisible();
    const stop=form.getByRole('button',{name:'Остановить',exact:true});
    assert.equal((await stop.innerText()).trim(),'');
    const initialHeight=await input.evaluate(el=>el.getBoundingClientRect().height);
    for(let rows=2;rows<=4;rows++) {
      await input.fill(Array(rows).fill('Строка').join('\n'));
      await expect.poll(()=>input.evaluate(el=>el.rows)).toBe(rows);
    }
    const maxHeight=await input.evaluate(el=>el.getBoundingClientRect().height);
    assert.ok(maxHeight>initialHeight);
    await input.fill(Array(20).fill('Длинная строка черновика').join('\n'));
    await expect.poll(()=>input.evaluate(el=>el.getBoundingClientRect().height)).toBe(maxHeight);
    assert.ok(await input.evaluate(el=>el.scrollHeight>el.clientHeight));
    const send=form.getByRole('button',{name:'Отправить',exact:true});
    assert.equal((await send.innerText()).trim(),'');
    const layout=await form.evaluate(el=>{
      const rect=selector=>el.querySelector(selector).getBoundingClientRect().toJSON();
      return {upload:rect('[aria-label="Прикрепить файлы и фото"]'),input:rect('textarea'),send:rect('[aria-label="Отправить"]')};
    });
    assert.ok(layout.upload.right<=layout.input.x && layout.input.right<=layout.send.x,JSON.stringify(layout));
    assert.ok(Math.abs(layout.upload.y-layout.send.y)<2,JSON.stringify(layout));
    await input.fill('');
    await expect.poll(()=>input.evaluate(el=>el.getBoundingClientRect().height)).toBe(initialHeight);
    await expect(form.getByRole('button',{name:'Отправить',exact:true})).toHaveCount(0);
    await input.fill('Уточнение');
    await expect(form.getByRole('button',{name:'Отправить',exact:true})).toBeEnabled();
    await expect(form.getByRole('button',{name:'Остановить',exact:true})).toHaveCount(0);
    await input.fill('');
    await input.evaluate(el=>{const data=new DataTransfer();data.items.add(new File(['attachment'],'draft.txt',{type:'text/plain'}));el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));});
    await expect(form.getByRole('button',{name:'Отправить',exact:true})).toBeEnabled();
    await expect(form.getByRole('button',{name:'Остановить',exact:true})).toHaveCount(0);
    await expect(form.getByText('Enter — отправить',{exact:false})).toHaveCount(0);
    await expect(page.getByText('Enter — отправить · Shift + Enter — новая строка',{exact:true})).toBeHidden();
    const planBlock=page.getByTestId('composer-plan');
    const plan=planBlock.getByRole('button');
    await expect(form.getByTestId('composer-plan')).toHaveCount(0);
    await expect(plan).toHaveText('План · 2 из 3 · Создать страницу и проверить длинный заголовок текущего шага на телефоне');
    await expect(page.getByTestId('chat-transcript').getByTestId('composer-plan')).toHaveCount(0);
    assert.ok(!['transparent','rgba(0, 0, 0, 0)'].includes(await planBlock.evaluate(el=>getComputedStyle(el).backgroundColor)));
    const chevron=plan.locator('[data-slot="trailingIcon"]');
    const collapsedChevron=await chevron.evaluate(el=>getComputedStyle(el).rotate);
    await plan.click();
    await expect(plan).toHaveAttribute('aria-expanded','true');
    await expect(chevron).toHaveCSS('rotate','180deg');
    await expect(planBlock.getByRole('list',{name:'Шаги плана'})).toBeVisible();
    await expect(planBlock.getByRole('list',{name:'Шаги плана'}).locator('li').nth(1).locator('span')).toHaveCSS('animation-name','spin');
    await expect(planBlock.getByRole('list',{name:'Шаги плана'})).toContainText('Проверить результат');
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-composer-plan-mobile.png',fullPage:true,animations:'disabled'});
    await page.setViewportSize({width:1280,height:900});
    await expect(page.getByText('Enter — отправить · Shift + Enter — новая строка',{exact:true})).toBeVisible();
    await page.screenshot({path:'/tmp/kodrik-composer-plan-desktop.png',fullPage:true,animations:'disabled'});
    await plan.click();await expect(planBlock.getByRole('list',{name:'Шаги плана'})).toBeHidden();
    await expect(chevron).toHaveCSS('rotate',collapsedChevron);
    snapshot.busy=false;snapshot.phase='Completed';snapshot.revision++;
    await expect(working).toHaveCount(0);
  } finally {await browser.close();}
});

test('conversation history loads on scroll and the header pencil renames an unloaded dialog',{timeout:60000},async()=>{
  const ids=[];
  for(let i=0;i<65;i++) ids.push((await(await request('sessions',{})).json()).id);
  const browser=await chromium.launch({headless:true});let releasePage;
  try {
    const page=await signedPage(browser),lists=[];
    page.on('request',r=>{const url=new URL(r.url());if(url.pathname.endsWith('/api/chat/sessions')&&r.method()==='GET') lists.push(url);});
    await page.goto(new URL(`chat/?conversation=${ids[0]}`,base).href);
    const nav=page.getByRole('navigation',{name:'История диалогов'}),rows=nav.locator('[data-conversation-id]');
    await expect(rows).toHaveCount(30);
    assert.equal(lists.length,1,'initial navigation must fetch just one page');
    assert.equal(await nav.locator(`[data-conversation-id="${ids[0]}"]`).count(),0);
    const pencil=page.getByRole('button',{name:'Переименовать диалог',exact:true});
    await pencil.waitFor();await page.mouse.move(900,500);
    await expect.poll(()=>pencil.evaluate(el=>getComputedStyle(el).opacity)).toBe('0');
    await pencil.locator('..').hover();
    await expect.poll(()=>pencil.evaluate(el=>getComputedStyle(el).opacity)).toBe('1');
    await page.mouse.move(900,500);
    await expect.poll(()=>pencil.evaluate(el=>getComputedStyle(el).opacity)).toBe('0');
    await pencil.focus();
    await expect.poll(()=>pencil.evaluate(el=>getComputedStyle(el).opacity)).toBe('1');
    await pencil.click();
    await page.getByRole('textbox',{name:'Название диалога',exact:true}).fill('Старый диалог из заголовка');
    await page.getByRole('button',{name:'Сохранить название',exact:true}).click();
    await expect(page.getByText('Старый диалог из заголовка',{exact:true})).toBeVisible();
    await expect(rows).toHaveCount(30);
    let failAppend=true;
    await page.route('**/api/chat/sessions?*',async route=>{
      if(new URL(route.request().url()).searchParams.get('offset')==='30' && failAppend) {
        failAppend=false;await route.fulfill({status:503,json:{error:'Temporary failure'}});
      } else await route.continue();
    });
    await nav.evaluate(el=>{el.scrollTop=el.scrollHeight;});
    await nav.getByRole('button',{name:'Повторить загрузку',exact:true}).click();
    await expect(rows).toHaveCount(60);
    const count=lists.length;
    await nav.evaluate(el=>{el.dispatchEvent(new Event('scroll'));});
    assert.equal(lists.length,count,'appending a page must not eagerly request the remainder');
    await page.setViewportSize({width:390,height:844});
    await page.getByRole('button',{name:'Показать диалоги',exact:true}).click();
    const mobile=page.getByRole('dialog').getByRole('navigation',{name:'История диалогов'});
    let pageStarted;
    const started=new Promise(resolve=>{pageStarted=resolve}),gate=new Promise(resolve=>{releasePage=resolve});
    await page.route('**/api/chat/sessions?*',async route=>{
      if(new URL(route.request().url()).searchParams.get('offset')!=='60') {await route.fallback();return;}
      const response=await route.fetch();pageStarted();await gate;await route.fulfill({response});
    });
    await mobile.evaluate(el=>{el.scrollTop=el.scrollHeight;});
    await started;
    await page.getByRole('dialog').getByRole('button',{name:'Поиск диалогов',exact:true}).click();
    await page.getByRole('dialog').getByRole('textbox',{name:'Поиск диалогов',exact:true}).fill('Старый диалог из заголовка');
    await expect(mobile.locator('[data-conversation-id]')).toHaveCount(1);
    const appended=page.waitForResponse(r=>new URL(r.url()).searchParams.get('offset')==='60');
    releasePage();await(await appended).finished();
    await expect(mobile.locator('[data-conversation-id]')).toHaveCount(1);
    await mobile.locator('[data-conversation-id]').click();
    await expect(page.getByRole('dialog')).toHaveCount(0);
  }finally{releasePage?.();await browser.close();}
});

test('conversation list retry restores a direct link and refreshes an exhausted list',{timeout:30000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);let failLists=true;
    await page.addInitScript(id=>sessionStorage.setItem(`kodrik.draft.${id}`,JSON.stringify({text:'Черновик существующего диалога',files:[]})),id);
    await page.route('**/api/chat/sessions?*',async route=>{
      if(failLists) {await route.fulfill({status:503,json:{error:'Temporary failure'}});return;}
      const response=await route.fetch();
      // Exercise an exhausted archive using real conversation metadata.
      await route.fulfill({response,json:(await response.json()).slice(0,5)});
    });
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await expect(page.getByRole('button',{name:'Повторить загрузку',exact:true})).toBeVisible();
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveValue('');
    failLists=false;
    await page.getByRole('button',{name:'Повторить загрузку',exact:true}).click();
    await expect(input).toHaveValue('Черновик существующего диалога');
    await input.fill('Продолжение сохранённого черновика');
    assert.equal(await page.evaluate(id=>JSON.parse(sessionStorage.getItem(`kodrik.draft.${id}`)).text,id),'Продолжение сохранённого черновика');
    await page.getByRole('button',{name:'Переименовать диалог',exact:true}).click();
    await page.getByRole('textbox',{name:'Название диалога',exact:true}).fill('Восстановлено после ошибки');
    failLists=true;
    await page.getByRole('button',{name:'Сохранить название',exact:true}).click();
    await expect(page.getByRole('button',{name:'Повторить загрузку',exact:true})).toBeVisible();
    failLists=false;
    await page.getByRole('button',{name:'Повторить загрузку',exact:true}).click();
    await expect(page.locator('.conversation-heading')).toContainText('Восстановлено после ошибки');
    await expect(page.getByRole('button',{name:'Повторить загрузку',exact:true})).toHaveCount(0);
  }finally{await browser.close();}
});

test('chat UX groups technical steps, folds old documents and expands the latest snapshot without losing local state',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try{
    const page=await signedPage(browser),errors=[];page.on('pageerror',e=>errors.push(e.message));
    const url=new URL('pages/preview/',base).href;
    const doc=(call,body)=>({type:'dynamic-tool',toolCallId:call,toolName:'show_ui',state:'output-available',input:{},output:{url,document:{version:1,title:'Проверка документа',body,sources:[]}}});
    const snapshot={type:'snapshot',session_id:id,run_id:null,revision:1,busy:false,cancel_requested:false,phase:'Completed',blocked_reason:null,live:{current:null,interrupted:null},plan:null,queued:[],messages:[{id:'answer',role:'assistant',parts:[
      {type:'dynamic-tool',toolCallId:'a',toolName:'exec_command',state:'output-available',input:{cmd:'test'},output:'ok'},
      doc('old','::unknown-component\n::'),
      doc('new','# Актуальный документ\n\n[Справка](https://example.com/docs)\n\n::u-button{to="https://example.com/action" target="_self" label="Открыть справку"}\n::\n\n::u-input{placeholder="Черновик внутри документа"}\n::'),
      {type:'text',text:'Готово'},
    ]}]};
    await page.route('**/events',r=>r.fulfill({contentType:'text/event-stream',body:`data: ${JSON.stringify(snapshot)}\n\n`}));
    await page.route('**/run',r=>r.fulfill({json:snapshot}));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await expect(page.getByRole('button',{name:'Выполнено шагов: 3',exact:true})).toBeVisible();
    await expect(page.getByRole('button',{name:/^exec_command\b/})).toHaveCount(0);
    await expect(page.getByText('Не удалось отобразить документ',{exact:true})).toHaveCount(0);
    await expect(page.getByText('Снимок на момент создания. Данные на странице могут измениться.',{exact:true})).toBeVisible();
    await expect(page.getByRole('link',{name:'Открыть страницу',exact:true})).toHaveAttribute('href',url);
    await expect(page.getByRole('link',{name:'Открыть страницу',exact:true})).not.toHaveAttribute('target','_blank');
    for(const name of ['Справка','Открыть справку']) {
      await expect(page.getByRole('link',{name,exact:true})).toHaveAttribute('target','_blank');
    }
    await page.getByPlaceholder('Черновик внутри документа').fill('Остаётся при разворачивании');
    await page.getByRole('button',{name:'Развернуть документ',exact:true}).click();
    await expect(page.getByRole('dialog')).toBeVisible();
    await expect(page.getByRole('dialog').getByPlaceholder('Черновик внутри документа')).toHaveValue('Остаётся при разворачивании');
    await page.keyboard.press('Escape');await expect(page.getByRole('dialog')).toHaveCount(0);
    await expect(page.getByPlaceholder('Черновик внутри документа')).toHaveValue('Остаётся при разворачивании');
    await page.screenshot({path:'/tmp/kodrik-chat-ux-document.png',fullPage:true});
    await page.setViewportSize({width:390,height:844});
    await page.getByRole('button',{name:'Развернуть документ',exact:true}).click();
    await expect(page.getByRole('dialog').getByPlaceholder('Черновик внутри документа')).toHaveValue('Остаётся при разворачивании');
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await expect(page.getByRole('dialog')).toHaveCSS('opacity','1');
    await page.screenshot({path:'/tmp/kodrik-chat-ux-expanded-mobile.png',fullPage:true,animations:'disabled'});
    await page.getByRole('button',{name:'Закрыть документ',exact:true}).click();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await page.getByRole('button',{name:'Предыдущая версия документа',exact:true}).click();
    await expect(page.getByText('Не удалось отобразить документ',{exact:true})).toBeVisible();
    await page.getByRole('button',{name:'Выполнено шагов: 3',exact:true}).click();
    await expect(page.getByRole('button',{name:'exec_command · test',exact:true})).toHaveAttribute('data-slot','trigger');
    await page.getByRole('button',{name:'exec_command · test',exact:true}).click();
    await expect(page.locator('pre').filter({hasText:'cmd'})).toContainText('test');
    await page.getByRole('button',{name:'Свернуть шаги',exact:true}).click();
    await expect(page.getByRole('button',{name:'Выполнено шагов: 3',exact:true})).toBeFocused();
    await expect(page.getByRole('button',{name:/^exec_command\b/})).toHaveCount(0);
    assert.deepEqual(errors,[]);
  }finally{await browser.close();}
});

test('long tool lists keep compact summaries and collapse back to their header',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    const tools=Array.from({length:20},(_,i)=>({type:'dynamic-tool',toolCallId:`step-${i}`,toolName:'exec_command',state:'output-available',input:{cmd:`cargo test --package crate-${i}`},output:'ok'}));
    tools[19]={...tools[19],state:'output-error',errorText:'Command failed'};
    const snapshot={type:'snapshot',session_id:id,run_id:null,revision:1,busy:false,cancel_requested:false,phase:'Completed',blocked_reason:null,live:{current:null,interrupted:null},plan:null,queued:[],messages:[{id:'steps',role:'assistant',parts:tools}]};
    await page.route('**/events',r=>r.fulfill({contentType:'text/event-stream',body:`data: ${JSON.stringify(snapshot)}\n\n`}));
    await page.route('**/run',r=>r.fulfill({json:snapshot}));
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const header=page.getByRole('button',{name:'Выполнено шагов: 20',exact:true});
    for(const viewport of [{width:1280,height:900},{width:390,height:844}]) {
      await page.setViewportSize(viewport);
      await header.click();
      await expect(page.getByText('Шагов с ошибкой: 1. Подробности доступны ниже.',{exact:true})).toBeAttached();
      const first=page.getByRole('button',{name:'exec_command · cargo test --package crate-0',exact:true});
      await first.click();
      await expect(page.locator('pre').first()).toContainText('cargo test --package crate-0');
      const collapse=page.getByRole('button',{name:'Свернуть шаги',exact:true});
      await expect(collapse).toBeInViewport();
      const last=page.getByRole('button',{name:'exec_command · cargo test --package crate-19',exact:true});
      await last.click();
      await expect(page.getByRole('alert')).toHaveText('Command failed');
      await collapse.click();
      await expect(header).toBeFocused();
      await expect(header).toBeInViewport();
      await expect(header).toHaveAttribute('aria-expanded','false');
      await expect(first).toHaveCount(0);
      assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
      await header.click();
      await expect(first).toHaveAttribute('aria-expanded','false');
      await page.screenshot({path:`/tmp/kodrik-tool-steps-${viewport.width}.png`,fullPage:true,animations:'disabled'});
      await collapse.click();
    }
  } finally {await browser.close();}
});

test('chat UX returns to latest message without forcing readers down on updates',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();await request('send',{id,text:'ui'});
  await expect.poll(async()=>(await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try{
    const page=await signedPage(browser);await page.setViewportSize({width:390,height:844});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await page.getByRole('heading',{name:'Дневник питания',exact:true}).waitFor();
    const transcript=page.getByRole('region',{name:'Разговор с Кодриком'}).locator('.overflow-y-auto').first();
    await transcript.evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new Event('scroll'));});
    await expect(page.getByRole('button',{name:'К последнему сообщению',exact:true})).toBeVisible();
    const overlay=page.getByRole('button',{name:'К последнему сообщению',exact:true}).locator('..');
    await expect(overlay).toHaveCSS('position','absolute');
    await expect(overlay).toHaveCSS('background-color','rgba(0, 0, 0, 0)');
    await page.screenshot({path:'/tmp/kodrik-chat-scroll-overlay-mobile.png',fullPage:true});
    await request('send',{id,text:'hello'});
    await expect(page.locator('.message').getByText('hello',{exact:true})).toHaveCount(1);
    assert.equal(await transcript.evaluate(el=>el.scrollTop),0);
    await page.getByRole('button',{name:'К последнему сообщению',exact:true}).click();
    await expect(page.getByRole('button',{name:'К последнему сообщению',exact:true})).toHaveCount(0);
    await expect.poll(()=>transcript.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop)).toBeLessThan(100);
    // Late image/tool layout changes can arrive without another snapshot.
    await transcript.evaluate(el=>{
      const output=document.createElement('div');output.dataset.testLayout='true';output.style.height='700px';output.textContent='Поздний результат инструмента';
      el.querySelector('.message:last-of-type').append(output);
    });
    await expect.poll(()=>transcript.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop)).toBeLessThan(5);
    await page.setViewportSize({width:390,height:680});
    await expect.poll(()=>transcript.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop)).toBeLessThan(5);
    await transcript.evaluate(el=>{el.scrollTop=0;el.dispatchEvent(new Event('scroll'));});
    await expect(page.getByRole('button',{name:'К последнему сообщению',exact:true})).toBeVisible();
    await transcript.evaluate(el=>{el.querySelector('[data-test-layout]').style.height='1000px';});
    await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve))));
    await expect.poll(()=>transcript.evaluate(el=>el.scrollTop)).toBe(0);
    await page.getByRole('button',{name:'К последнему сообщению',exact:true}).click();
    await expect.poll(()=>transcript.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop)).toBeLessThan(5);
    await transcript.evaluate(el=>el.querySelector('[data-test-layout]').remove());
    await expect.poll(()=>transcript.evaluate(el=>el.scrollHeight-el.clientHeight-el.scrollTop)).toBeLessThan(5);
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-chat-ux-mobile.png',fullPage:true});
    await page.setViewportSize({width:1440,height:1000});
    await page.screenshot({path:'/tmp/kodrik-chat-ux-desktop.png',fullPage:true});
  }finally{await browser.close();}
});

for(const returnBeforeReply of [false,true]) test(`chat UX clears submitted attachments when returning ${returnBeforeReply?'before':'after'} the admission reply`,{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});let release;
  try{
    const page=await signedPage(browser),gate=new Promise(resolve=>{release=resolve});
    let admitted;const accepted=new Promise(resolve=>{admitted=resolve});
    const types=[];
    await page.route('**/api/chat/send',async route=>{
      types.push(route.request().headers()['content-type']);
      const response=await route.fetch();admitted();await gate;await route.fulfill({response});
    });
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('attachment first');
    await input.evaluate(el=>{const data=new DataTransfer();data.items.add(new File(['one upload'],'once.txt',{type:'text/plain'}));el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));});
    await page.getByRole('button',{name:'Отправить',exact:true}).click();await accepted;
    await page.getByRole('button',{name:'Новый диалог',exact:true}).first().click();
    await expect(page.getByRole('heading',{name:'Есть мысль?',exact:true})).toBeVisible();
    if(!returnBeforeReply) {
      release();
      await expect.poll(()=>page.evaluate(id=>sessionStorage.getItem(`kodrik.pending.${id}`),id)).toBe(null);
    }
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${id}"]`).click();
    await expect(page.getByRole('button',{name:/once\.txt/})).toHaveCount(0);
    if(!returnBeforeReply) await page.reload();
    await expect(page.getByText('Повторно выберите вложения: once.txt',{exact:false})).toHaveCount(0);
    await input.fill('next without attachment');await input.press('Enter');
    await expect(page.locator('.message').getByText('next without attachment',{exact:true})).toHaveCount(1);
    release();
    assert.equal(types.length,2);assert.ok(types[0].startsWith('multipart/form-data'));assert.equal(types[1],'application/json');
  }finally{release?.();await browser.close();}
});

test('chat UX sends a different saved draft after reconciling a completed pending request',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json(),requestId=crypto.randomUUID();
  await request('send',{id,text:'original accepted',request_id:requestId});
  await expect.poll(async()=>(await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try{
    const page=await signedPage(browser),sent=[];
    page.on('request',request=>{if(request.url().endsWith('/api/chat/send')) sent.push(request.postDataJSON());});
    await page.addInitScript(({id,requestId})=>{
      sessionStorage.setItem(`kodrik.pending.${id}`,JSON.stringify({id:requestId,text:'original accepted',files:[]}));
      sessionStorage.setItem(`kodrik.draft.${id}`,JSON.stringify({text:'Different next question',files:[]}));
    },{id,requestId});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveValue('Different next question');
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.press('Enter');
    await expect(page.locator('.message').getByText('Different next question',{exact:true})).toHaveCount(1);
    assert.equal(sent.length,1);assert.notEqual(sent[0].request_id,requestId);
  }finally{await browser.close();}
});

test('chat UX does not resend an accepted attachment with a different draft after a lost reply',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try{
    const page=await signedPage(browser),types=[];
    await page.route('**/api/chat/send',async route=>{
      types.push(route.request().headers()['content-type']);
      if(types.length===1) {await route.fetch();await route.abort('failed');}
      else await route.continue();
    });
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('accepted attachment');
    await input.evaluate(el=>{const data=new DataTransfer();data.items.add(new File(['only once'],'accepted.txt',{type:'text/plain'}));el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));});
    await input.press('Enter');
    await expect(input).toHaveValue('accepted attachment');
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('different draft without the old file');await input.press('Enter');
    await expect(page.locator('.message').getByText('different draft without the old file',{exact:true})).toHaveCount(1);
    assert.equal(types.length,2);assert.equal(types[1],'application/json');
    await expect(page.locator('.message').getByRole('link',{name:'accepted.txt',exact:true})).toHaveCount(1);
  }finally{await browser.close();}
});

test('chat UX retains bytes for an unaccepted attachment after switching conversations',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});let release;
  try{
    const page=await signedPage(browser),gate=new Promise(resolve=>{release=resolve});
    let intercepted;const started=new Promise(resolve=>{intercepted=resolve});let sends=0;
    await page.route('**/api/chat/send',async route=>{
      if(++sends===1) {intercepted();await gate;await route.abort('failed');}
      else await route.continue();
    });
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('retry retained bytes');
    await input.evaluate(el=>{const data=new DataTransfer();data.items.add(new File(['retained bytes'],'retained.txt',{type:'text/plain'}));el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));});
    await input.press('Enter');await started;
    await page.getByRole('button',{name:'Новый диалог',exact:true}).first().click();
    await expect(page.getByRole('heading',{name:'Есть мысль?',exact:true})).toBeVisible();
    release();
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${id}"]`).click();
    await expect(input).toHaveValue('retry retained bytes');
    await expect(input).toHaveJSProperty('readOnly',false);await input.press('Enter');
    await expect(page.locator('.message').getByRole('link',{name:'retained.txt',exact:true})).toHaveCount(1);
    assert.equal(sends,2);
  }finally{release?.();await browser.close();}
});

test('chat UX keeps a newer same-text draft when an older send reply arrives late',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});let release;
  try{
    const page=await signedPage(browser),gate=new Promise(resolve=>{release=resolve});let sends=0;
    let admitted;const accepted=new Promise(resolve=>{admitted=resolve});
    await page.route('**/api/chat/send',async route=>{
      const response=await route.fetch();
      if(++sends===1) {admitted();await gate;}
      await route.fulfill({response});
    });
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const input=page.getByRole('textbox',{name:'Сообщение',exact:true});
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('continue');await input.press('Enter');await accepted;
    await page.getByRole('button',{name:'Новый диалог',exact:true}).first().click();
    await expect(page.getByRole('heading',{name:'Есть мысль?',exact:true})).toBeVisible();
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${id}"]`).click();
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('different question');await input.press('Enter');
    await expect(page.locator('.message').getByText('different question',{exact:true})).toHaveCount(1);
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.fill('continue');
    const finished=page.waitForResponse(r=>r.url().endsWith('/api/chat/send')&&r.request().postDataJSON().text==='continue');
    release();await finished;
    await expect(input).toHaveValue('continue');
    assert.equal(await page.evaluate(id=>JSON.parse(sessionStorage.getItem(`kodrik.draft.${id}`)).text,id),'continue');
  }finally{release?.();await browser.close();}
});

test('Nuxt UI login preserves errors and return path on mobile and desktop', { timeout: 60000 }, async () => {
  const {id}=await (await request('sessions',{})).json();
  const target=new URL(`chat/?conversation=${id}`,base);
  const url=new URL('auth/login',base);
  url.searchParams.set('return_to',target.pathname+target.search);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'ru-RU',viewport:{width:390,height:844},colorScheme:'light'});
    const errors=[];
    const apiRequests=[];
    page.on('pageerror',error=>errors.push(error.message));
    page.on('request',request=>{if(new URL(request.url()).pathname.includes('/api/chat/')) apiRequests.push(request.url());});
    await page.goto(url.href);
    await page.getByRole('heading',{name:'Вход в Кодрик'}).waitFor();
    assert.equal(page.url(),url.href);
    const password=page.getByLabel('Пароль',{exact:true});
    await expect(password).toHaveAttribute('autocomplete','current-password');
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.screenshot({path:'/tmp/kodrik-login-mobile.png',fullPage:true});
    assert.deepEqual(apiRequests,[]);
    await page.getByRole('button',{name:'Войти',exact:true}).click();
    await page.getByText('Введите пароль.',{exact:true}).waitFor();
    await password.fill('wrong-password');
    await page.getByRole('button',{name:'Войти',exact:true}).click();
    await page.getByRole('alert').getByText('Не удалось войти. Проверьте пароль.').waitFor();
    await expect(password).toHaveValue('');
    await expect(page.locator('input[name=return_to]')).toHaveValue(target.pathname+target.search);
    await page.emulateMedia({colorScheme:'dark'});
    await page.setViewportSize({width:1280,height:900});
    await page.getByRole('heading',{name:'Вход в Кодрик'}).waitFor();
    await expect.poll(()=>page.evaluate(()=>getComputedStyle(document.documentElement).getPropertyValue('--ui-primary').trim())).toBe('#89b4fa');
    await page.screenshot({path:'/tmp/kodrik-login-desktop.png',fullPage:true});
    await password.fill('preview-password');
    await password.press('Enter');
    await page.waitForURL(target.href);
    await page.getByRole('heading',{name:'Есть мысль?'}).waitFor();
    // Keep API checks on a recent session as later logins evict the oldest ones.
    const session=(await page.context().cookies()).find(cookie=>cookie.name==='kodrik_web_session');
    assert.ok(session);cookie=`${session.name}=${session.value}`;
    assert.deepEqual(errors,[]);
  } finally { await browser.close(); }
});

test('browser login, text, tools, stop, history and mobile layout', { timeout: 60000 }, async () => {
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({locale:'ru-RU', viewport: { width: 1280, height: 900 } });
    const errors = [];
    page.on('pageerror', error => errors.push(error.message));
    await page.goto(new URL('chat/', base).href);
    const loginResponse=page.waitForResponse(response=>response.request().method()==='POST' && new URL(response.url()).pathname.endsWith('/auth/login'));
    await page.locator('input[type=password]').fill('preview-password');
    await page.locator('button[type=submit]').click();
    assert.equal((await loginResponse).status(),303);
    await newConversation(page);
    const input = page.getByRole('textbox', { name: 'Сообщение' });
    await input.fill('tools');
    const toolsId=new URL(page.url()).searchParams.get('conversation');
    await page.getByRole('button', { name: 'Отправить' }).click();
    await page.getByText('Привет!', { exact: true }).waitFor();
    await page.getByRole('button',{name:'Выполнено шагов: 1',exact:true}).click();
    await page.getByText('update_plan', { exact: true }).click();
    await page.locator('pre').getByText('Проверить чат', { exact: false }).waitFor();
    await input.fill('wait');
    await page.getByRole('button', { name: 'Отправить' }).click();
    await page.getByText('Жду остановки', { exact: true }).waitFor();
    await page.getByRole('button', { name: 'Остановить', exact: false }).click();
    await page.getByRole('button', { name: 'Отправить' }).waitFor();
    await page.getByRole('button', { name: 'Новый диалог', exact: true }).first().click();
    await page.getByRole('heading', { name: 'Есть мысль?' }).waitFor();
    await input.fill('retry');
    const retryId=new URL(page.url()).searchParams.get('conversation');
    await page.getByRole('button', { name: 'Отправить' }).click();
    await page.getByText('Привет!', { exact: true }).waitFor();
    assert.equal(await page.getByText('Неверная попытка', { exact: true }).count(), 0);
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${toolsId}"]`).click();
    await page.getByText('wait', { exact: true }).waitFor();
    await expect(page.getByText('Привет!', { exact: true })).toHaveCount(1);
    await page.screenshot({ path: '/tmp/kodrik-chat-desktop.png', fullPage: true });
    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByRole('button', { name: 'Показать диалоги' }).click();
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${retryId}"]`).click();
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await page.getByText('Привет!', { exact: true }).waitFor();
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth));
    await page.screenshot({ path: '/tmp/kodrik-chat-mobile.png', fullPage: true });
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test('composer waits for authoritative snapshot after admission', { timeout: 30000 }, async () => {
  const browser = await chromium.launch({ headless: true });
  let release;
  try {
    const page = await browser.newPage({locale:'ru-RU'});
    await page.context().addCookies([{ name: cookie.split('=')[0], value: cookie.split('=')[1], url: base }]);
    await page.goto(new URL('chat/', base).href);
    await newConversation(page);
    await page.getByRole('heading', { name: 'Есть мысль?' }).waitFor();
    const held = new Promise(resolve => { release = resolve; });
    let observed;
    const historyRequested = new Promise(resolve => { observed = resolve; });
    await page.route('**/events', route => route.abort());
    await page.route('**/run', async route => {
      const response = await route.fetch();
      observed(); await held;
      await route.fulfill({ response });
    });
    const input = page.getByRole('textbox', { name: 'Сообщение' });
    await input.fill('first');
    await page.getByRole('button', { name: 'Отправить' }).click();
    await historyRequested;
    await page.getByRole('button', { name: 'Отправить' }).waitFor();
    await expect(input).toHaveJSProperty('readOnly', true);
    await expect(page.getByRole('button', { name: 'Отправить' })).toBeDisabled();
    await input.evaluate(el=>el.blur());
    await page.keyboard.press('x');
    await expect(input).not.toBeFocused();
    await expect(input).toHaveValue('');
    release();
    await page.unrouteAll({ behavior: 'wait' });
    await input.fill('second');
    await page.getByRole('button', { name: 'Отправить' }).click();
    await page.getByText('second', { exact: true }).waitFor();
  } finally { release?.(); await browser.close(); }
});

test('composer keeps focus after Enter and button submission without stealing it back', { timeout: 30000 }, async () => {
  const browser = await chromium.launch({ headless: true });
  try {
    const page = await browser.newPage({locale:'ru-RU'});
    await page.context().addCookies([{ name: cookie.split('=')[0], value: cookie.split('=')[1], url: base }]);
    await page.goto(new URL('chat/', base).href);
    await newConversation(page);
    await page.getByRole('heading', { name: 'Есть мысль?' }).waitFor();
    const input = page.getByRole('textbox', { name: 'Сообщение' });
    await expect(input).toBeFocused();
    const firstId=new URL(page.url()).searchParams.get('conversation');
    await input.fill('hello');
    await input.press('Enter');
    await page.getByText('Привет!', { exact: true }).waitFor();
    await expect(input).toBeFocused();
    await input.fill('wait');
    await page.getByRole('button', { name: 'Отправить' }).click();
    await page.getByText('Жду остановки', { exact: true }).waitFor();
    await expect(input).toBeFocused();
    await expect(input).toHaveJSProperty('readOnly', false);
    await input.press('Tab');
    await page.getByRole('button', { name: 'Остановить', exact: false }).click();
    await page.getByRole('button', { name: 'Отправить' }).waitFor();
    await expect(input).toHaveJSProperty('readOnly', false);
    await expect(input).not.toBeFocused();
    await page.getByRole('button',{name:'Новый диалог',exact:true}).first().click();
    await expect(input).toBeFocused();
    const secondId=new URL(page.url()).searchParams.get('conversation');
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${firstId}"]`).click();
    await expect(input).toBeFocused();
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${firstId}"]`).click();
    await expect(input).toBeFocused();
    const mobile=await browser.newPage({locale:'ru-RU',viewport:{width:390,height:844},isMobile:true,hasTouch:true});
    await mobile.context().addCookies([{name:cookie.split('=')[0],value:cookie.split('=')[1],url:base}]);
    await mobile.goto(new URL(`chat/?conversation=${secondId}`,base).href);
    await mobile.getByRole('button',{name:'Показать диалоги',exact:true}).click();
    await mobile.getByRole('dialog').locator(`[data-conversation-id="${firstId}"]`).click();
    await expect(mobile.getByRole('dialog')).toHaveCount(0);
    await expect(mobile.getByRole('textbox',{name:'Сообщение',exact:true})).toBeFocused();
    const toggle=mobile.getByRole('button',{name:'Показать диалоги',exact:true});
    await toggle.click();await expect(mobile.getByRole('dialog')).toBeVisible();
    await mobile.keyboard.press('Escape');await expect(mobile.getByRole('dialog')).toHaveCount(0);
    await expect(mobile.getByRole('textbox',{name:'Сообщение',exact:true})).not.toBeFocused();
    await toggle.click();
    await mobile.getByRole('dialog').locator(`[data-conversation-id="${firstId}"]`).click();
    await expect(mobile.getByRole('dialog')).toHaveCount(0);
    await expect(mobile.getByRole('textbox',{name:'Сообщение',exact:true})).toBeFocused();
  } finally { await browser.close(); }
});

test('lost send response checks stable request status before another submission', { timeout: 60000 }, async () => {
  const browser = await chromium.launch({ headless: true });
  try {
    const page=await browser.newPage({locale:'ru-RU'});
    const [name,...value]=cookie.split('=');
    await page.context().addCookies([{name,value:value.join('='),url:base}]);
    await page.goto(new URL('chat/',base).href);
    await newConversation(page);
    await page.getByRole('heading',{name:'Есть мысль?'}).waitFor();
    const composer=page.getByRole('textbox',{name:'Сообщение',exact:true});
    const bodies=[];let statusChecks=0;
    page.on('request',r=>{if(r.url().includes('/requests/'))statusChecks++;});
    await page.route('**/api/chat/send',async route=>{
      bodies.push(route.request().postDataJSON());
      await route.fetch();
      await route.abort('failed');
    });
    await composer.fill('lost response');await composer.press('Enter');
    await expect(composer).toHaveValue('lost response');
    await expect(page.getByRole('alert')).toHaveClass(/text-error/);
    await expect(page.getByRole('button',{name:'Обновить историю',exact:true})).toBeVisible();
    assert.ok(bodies[0].request_id,'UUID must exist before the first HTTP send');
    await expect(composer).toHaveJSProperty('readOnly',false);await composer.press('Enter');
    await expect.poll(()=>statusChecks).toBeGreaterThan(0);
    await expect(composer).toHaveValue('');
    assert.equal(bodies.length,1,'accepted request must not be submitted again under another UUID');
  } finally {await browser.close();}
});

test('blocked accepted request allows a new input to resume the queue', { timeout: 60000 }, async () => {
  const browser=await chromium.launch({headless:true});
  try {
    const {id}=await (await request('sessions',{})).json();
    await (await request('send',{id,text:'blocked queue test'})).text();
    const page=await browser.newPage({locale:'ru-RU'});const [name,...value]=cookie.split('=');
    await page.context().addCookies([{name,value:value.join('='),url:base}]);
    await page.addInitScript(({id})=>sessionStorage.setItem(`kodrik.pending.${id}`,JSON.stringify({id:'11111111-1111-4111-8111-111111111111',text:'old input'})),{id});
    await page.route('**/requests/*',r=>r.fulfill({json:{status:'Queued',blocked_reason:'Stopped by user'}}));
    let submissions=0;page.on('request',r=>{if(r.url().endsWith('/api/chat/send'))submissions++;});
    await page.goto(new URL('chat/',base).href);
    await page.getByRole('navigation',{name:'История диалогов'}).locator(`[data-conversation-id="${id}"]`).click();
    const input=page.getByRole('textbox',{name:'Сообщение'});
    await input.fill('continue please');await input.press('Enter');
    await expect.poll(()=>submissions).toBe(1);
    await expect(page.getByText('Привет!',{exact:true})).toHaveCount(2);
  } finally {await browser.close();}
});


test('refresh and two observers preserve execution; Stop is shared', {timeout:30000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const context=await browser.newContext({locale:'ru-RU'});
    await context.addCookies([{name:cookie.split('=')[0],value:cookie.split('=')[1],url:base}]);
    const page=await context.newPage();await page.goto(new URL('chat/',base).href);
    await newConversation(page);
    const input=page.getByRole('textbox',{name:'Сообщение'});
    await input.fill('wait');await input.press('Enter');
    await page.getByText('Жду остановки',{exact:true}).waitFor();
    const url=page.url();await page.reload();
    await page.getByText('Жду остановки',{exact:true}).waitFor();
    const second=await context.newPage();await second.goto(url);
    await second.getByText('Жду остановки',{exact:true}).waitFor();
    await expect(second.getByRole('button',{name:'Остановить',exact:false})).toBeEnabled();
    await second.getByRole('button',{name:'Остановить',exact:false}).click();
    await second.getByRole('button',{name:'Отправить'}).waitFor();
    await page.getByRole('button',{name:'Отправить'}).waitFor();
    await page.close();
    await expect(second.locator('.message').getByText('wait',{exact:true})).toHaveCount(1);
  }finally{await browser.close();}
});


async function signedPage(browser,options={}) {
  const page=await browser.newPage({locale:'ru-RU',...options});
  await page.context().addCookies([{name:cookie.split('=')[0],value:cookie.split('=')[1],url:base}]);
  return page;
}

test('owner manages webhooks, rotates one-time tokens and opens arbitrary event conversations', {timeout:60000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{viewport:{width:1280,height:900}});
    await page.goto(new URL('chat/',base).href);
    await page.getByRole('button',{name:'Вебхуки',exact:true}).click();
    await page.getByRole('button',{name:'Создать вебхук',exact:true}).click();
    await page.getByLabel('Название вебхука',{exact:true}).fill('Ошибки checkout');
    await page.getByLabel('Инструкция агенту',{exact:true}).fill('Найди причину ошибки и открой PR после проверки тестов.');
    await page.getByRole('button',{name:'Сохранить вебхук',exact:true}).click();
    const url=await page.getByLabel('URL вебхука',{exact:true}).inputValue();
    const token=await page.getByLabel('Bearer-токен',{exact:true}).inputValue();
    assert.ok(token.length>=32);
    assert.equal((await fetch(url,{method:'POST',body:'unauthorized'})).status,401);
    const event=await fetch(url,{method:'POST',headers:{Authorization:`Bearer ${token}`,'Idempotency-Key':'browser-event'},body:'checkout error\nraw stack trace'});
    assert.equal(event.status,202);const accepted=await event.json();
    await page.getByRole('button',{name:'Открыть обработку',exact:true}).first().click();
    await expect(page).toHaveURL(new RegExp(`conversation=${accepted.conversation_id}`));
    await expect(page.locator('.message[data-role="user"]').first()).toContainText('checkout error');
    await page.getByRole('button',{name:'Вебхуки',exact:true}).click();
    await expect(page.getByLabel('Bearer-токен',{exact:true})).toHaveCount(0);
    await page.getByLabel('Название вебхука',{exact:true}).fill('Ошибки оплаты');
    await page.getByLabel('Инструкция агенту',{exact:true}).fill('Проверь новую инструкцию.');
    await page.getByRole('button',{name:'Сохранить вебхук',exact:true}).click();
    await expect(page.getByRole('navigation',{name:'Список вебхуков'})).toContainText('Ошибки оплаты');
    await page.getByRole('button',{name:'Перевыпустить токен',exact:true}).click();
    await page.getByRole('button',{name:'Подтвердить перевыпуск',exact:true}).click();
    const rotated=await page.getByLabel('Bearer-токен',{exact:true}).inputValue();
    assert.notEqual(rotated,token);
    assert.equal((await fetch(url,{method:'POST',headers:{Authorization:`Bearer ${token}`},body:'old token'})).status,401);
    await page.getByRole('switch',{name:'Вебхук включён',exact:true}).click();
    await page.getByRole('button',{name:'Сохранить вебхук',exact:true}).click();
    assert.equal((await fetch(url,{method:'POST',headers:{Authorization:`Bearer ${rotated}`},body:'disabled'})).status,401);
    await page.getByRole('button',{name:'Удалить вебхук',exact:true}).click();
    await page.getByRole('button',{name:'Подтвердить удаление',exact:true}).click();
    await expect(page.getByText('Пока нет вебхуков.',{exact:true})).toBeVisible();
  } finally {await browser.close();}
});

test('webhook form works in the mobile sidebar and English interface', {timeout:60000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{locale:'en-US',viewport:{width:390,height:844},isMobile:true,hasTouch:true});
    await page.goto(new URL('chat/',base).href);
    await page.getByRole('button',{name:'Show conversations',exact:true}).click();
    await page.getByRole('button',{name:'Webhooks',exact:true}).click();
    await page.getByRole('button',{name:'Create webhook',exact:true}).click();
    await page.getByLabel('Webhook name',{exact:true}).fill('Mobile errors');
    await page.getByLabel('Agent instruction',{exact:true}).fill('Investigate the event.');
    await page.getByRole('button',{name:'Save webhook',exact:true}).click();
    await expect(page.getByLabel('Bearer token',{exact:true})).toHaveValue(/[0-9a-f]{64}/);
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=window.innerWidth),true);
    await page.getByRole('button',{name:'Delete webhook',exact:true}).click();
    await page.getByRole('button',{name:'Confirm deletion',exact:true}).click();
    await page.getByRole('button',{name:'Show webhooks',exact:true}).click();
    await expect(page.getByRole('navigation',{name:'Webhooks list'}).getByRole('button',{name:'Mobile errors',exact:true})).toHaveCount(0);
  } finally {await browser.close();}
});

async function newConversation(page) {
  await page.getByRole('textbox',{name:'Сообщение',exact:true}).waitFor();
  const button=page.getByRole('button',{name:'Новый диалог',exact:true}).first();
  if(!await button.isVisible()) await page.getByRole('button',{name:'Показать диалоги',exact:true}).click();
  await button.click();
}

async function chooseLanguage(page,value) {
  await page.getByRole('button',{name:/^(Language|Язык)$/}).click();
  await page.getByRole('menuitemcheckbox',{name:value==='system'?/^(System|Системный)$/:value==='en'?'English':'Русский',exact:true}).click();
}

test('language and theme icon menus preserve manual choices and follow system settings',{timeout:60000},async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{locale:'ru-RU',colorScheme:'light'});
    await page.goto(new URL('chat/',base).href);
    await expect(page.locator('html')).toHaveAttribute('lang','ru');
    await expect(page.locator('select')).toHaveCount(0);
    await page.getByRole('button',{name:'Язык',exact:true}).click();
    await expect(page.getByRole('menuitemcheckbox',{name:'Системный',exact:true})).toBeChecked();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('button',{name:'Язык',exact:true})).toBeFocused();
    await chooseLanguage(page,'en');
    await page.evaluate(()=>{Object.defineProperty(navigator,'language',{configurable:true,value:'ru-BY'});window.dispatchEvent(new Event('languagechange'))});
    await expect(page.locator('html')).toHaveAttribute('lang','en');
    await chooseLanguage(page,'system');
    await expect(page.locator('html')).toHaveAttribute('lang','ru');
    assert.equal(await page.evaluate(()=>localStorage.getItem('kodrik.language')),'system');
    await page.evaluate(()=>{Object.defineProperty(navigator,'language',{configurable:true,value:'en-GB'});window.dispatchEvent(new Event('languagechange'))});
    await expect(page.locator('html')).toHaveAttribute('lang','en');
    await page.reload();
    await expect(page.locator('html')).toHaveAttribute('lang','ru');
    await chooseLanguage(page,'en');

    await page.getByRole('button',{name:'Switch theme',exact:true}).click();
    await expect(page.getByRole('menuitemcheckbox',{name:'System',exact:true})).toBeChecked();
    await page.getByRole('menuitemcheckbox',{name:'Dark',exact:true}).click();
    await expect(page.locator('html')).toHaveClass(/dark/);
    await page.emulateMedia({colorScheme:'light'});
    await expect(page.locator('html')).toHaveClass(/dark/);
    await page.reload();
    await expect(page.locator('html')).toHaveClass(/dark/);
    await page.getByRole('button',{name:'Switch theme',exact:true}).click();
    await expect(page.getByRole('menuitemcheckbox',{name:'Dark',exact:true})).toBeChecked();
    await page.getByRole('menuitemcheckbox',{name:'System',exact:true}).click();
    await expect(page.locator('html')).not.toHaveClass(/dark/);
    await page.emulateMedia({colorScheme:'dark'});
    await expect(page.locator('html')).toHaveClass(/dark/);
    await page.reload();
    await expect(page.locator('html')).toHaveClass(/dark/);
    await page.emulateMedia({colorScheme:'light'});
    await expect(page.locator('html')).not.toHaveClass(/dark/);
    await page.getByRole('button',{name:'Switch theme',exact:true}).click();
    await page.getByRole('menuitemcheckbox',{name:'Light',exact:true}).click();
    await page.emulateMedia({colorScheme:'dark'});
    await expect(page.locator('html')).not.toHaveClass(/dark/);

    const logout=page.getByRole('button',{name:'Sign out',exact:true});
    await expect(logout).toHaveText('');
    assert.ok((await logout.boundingBox()).width<=36);
    await page.getByRole('button',{name:'Language',exact:true}).click();
    await expect(page.getByRole('menu')).toBeVisible();
    await expect(page.getByRole('menu')).toBeInViewport();
    await page.screenshot({path:'/tmp/kodrik-language-menu.png',fullPage:true,animations:'disabled'});
    await page.keyboard.press('Escape');
    await page.getByRole('button',{name:'Switch theme',exact:true}).click();
    await expect(page.getByRole('menu')).toBeVisible();
    await expect(page.getByRole('menu')).toBeInViewport();
    await page.screenshot({path:'/tmp/kodrik-theme-menu.png',fullPage:true,animations:'disabled'});
  } finally {await browser.close();}
});

test('English fallback, locale persistence, localized API errors and timestamps preserve conversation content',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const text='Пользовательский текст остаётся русским';
  await request('send',{id,text});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const snapshot=await(await request(`sessions/${id}/run`)).json();
  const browser=await chromium.launch({headless:true});
  try {
    const context=await browser.newContext({locale:'fr-FR',timezoneId:'America/New_York'});
    await context.addCookies([{name:cookie.split('=')[0],value:cookie.split('=')[1],url:base}]);
    const page=await context.newPage();
    await page.addInitScript(()=>{
      if(sessionStorage.getItem('locale-test-seeded')) return;
      localStorage.setItem('kodrik.language','unsupported');
      sessionStorage.setItem('locale-test-seeded','1');
    });
    const url=new URL(`chat/?conversation=${id}`,base).href;
    await page.goto(url);
    await expect(page.locator('html')).toHaveAttribute('lang','en');
    await expect(page.getByRole('textbox',{name:'Message',exact:true})).toBeVisible();
    await expect(page.locator('.message[data-role="user"]')).toContainText(text);
    const user=snapshot.messages.find(message=>message.role==='user');
    const item=page.locator('.message[data-role="user"]');
    await item.hover();
    await expect(item.locator('time')).toHaveText(`Today, ${new Intl.DateTimeFormat('en-US',{hour:'2-digit',minute:'2-digit',timeZone:'America/New_York'}).format(new Date(user.created_at))}`);
    await page.route('**/api/chat/send',route=>route.fulfill({status:400,contentType:'application/json',body:JSON.stringify({error:'Страница обновлена. Загрузите её заново.'})}));
    await page.getByRole('textbox',{name:'Message',exact:true}).fill('A draft survives a language switch');
    await page.getByRole('button',{name:'Send',exact:true}).click();
    await expect(page.getByRole('alert')).toContainText('The page has changed. Reload it.');
    await chooseLanguage(page,'ru');
    await expect(page.locator('html')).toHaveAttribute('lang','ru');
    await expect(page.getByRole('alert')).toContainText('Страница обновлена. Загрузите её заново.');
    await expect(page.getByRole('textbox',{name:'Сообщение',exact:true})).toHaveValue('A draft survives a language switch');
    assert.equal(await page.evaluate(()=>localStorage.getItem('kodrik.language')),'ru');
    await page.reload();
    await expect(page.locator('html')).toHaveAttribute('lang','ru');
    await expect(page.getByRole('textbox',{name:'Сообщение',exact:true})).toHaveValue('A draft survives a language switch');
    const other=await page.context().newPage();
    await other.goto(new URL('auth/login',base).href);
    await expect(other.locator('html')).toHaveAttribute('lang','ru');
    await expect(other.getByRole('heading',{name:'Вход в Кодрик',exact:true})).toBeVisible();
    await chooseLanguage(other,'en');
    await expect(other.getByRole('heading',{name:'Sign in to Kodrik',exact:true})).toBeVisible();
    await other.goto(base);
    await expect(other.getByRole('heading',{name:'For ideas, questions and everyday tasks.',exact:true})).toBeVisible();
    await other.getByRole('button',{name:'Language',exact:true}).click();
    await expect(other.getByRole('menuitemcheckbox',{name:'English',exact:true})).toBeChecked();
    await other.keyboard.press('Escape');
    await other.reload();
    await expect(other.locator('html')).toHaveAttribute('lang','en');
    await page.unroute('**/api/chat/send');
    await page.reload();
    await expect(page.getByRole('textbox',{name:'Message',exact:true})).toHaveValue('A draft survives a language switch');
  } finally {await browser.close();}
});

test('English document chrome switches language without changing document titles or saved data',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  await request('send',{id,text:'ui'});
  await expect.poll(async()=> (await(await request(`sessions/${id}/run`)).json()).busy).toBe(false);
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{locale:'en-US'});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    await expect(page.getByRole('heading',{name:'Дневник питания',exact:true})).toBeVisible();
    await expect(page.getByRole('textbox',{name:'Filter table',exact:true})).toBeVisible();
    const url=await page.getByRole('link',{name:'Open page',exact:true}).getAttribute('href');
    await page.goto(url);
    await expect(page.getByRole('button',{name:'Next',exact:true})).toBeVisible();
    await expect(page.getByRole('heading',{name:'Дневник питания',exact:true})).toBeVisible();
    await page.getByRole('button',{name:'Next',exact:true}).click();
    await expect(page.getByRole('table')).toContainText('Dinner');
    await chooseLanguage(page,'ru');
    await expect(page.getByRole('button',{name:'Назад',exact:true})).toBeVisible();
    await expect(page.getByRole('heading',{name:'Дневник питания',exact:true})).toBeVisible();
    await expect(page.getByRole('table')).toContainText('Dinner');
    await page.reload();
    await expect(page.locator('html')).toHaveAttribute('lang','ru');
    await expect(page.getByRole('button',{name:'Далее',exact:true})).toBeVisible();
  } finally {await browser.close();}
});

test('English mobile chat keeps keyboard sending, saved drafts and language selection usable',{timeout:60000},async()=>{
  const {id}=await(await request('sessions',{})).json();
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser,{locale:'en-US',viewport:{width:390,height:844},isMobile:true,hasTouch:true});
    await page.addInitScript(({id})=>{
      if(sessionStorage.getItem('draft-test-seeded')) return;
      sessionStorage.setItem(`kodrik.draft.${id}`,JSON.stringify({text:'Saved unsent draft',files:[]}));
      sessionStorage.setItem('draft-test-seeded','1');
    },{id});
    await page.goto(new URL(`chat/?conversation=${id}`,base).href);
    const editor=page.getByRole('textbox',{name:'Message',exact:true});
    await expect(editor).toHaveValue('Saved unsent draft');
    await editor.fill('English mobile question');
    await editor.press('Shift+Enter');
    await editor.press('End');
    await editor.press('a');
    await expect(editor).toHaveValue('English mobile question\na');
    await editor.press('Enter');
    await expect(page.locator('.message[data-role="user"]')).toContainText('English mobile question');
    await expect(editor).toHaveValue('');
    await page.getByRole('button',{name:'Show conversations',exact:true}).click();
    await page.getByRole('button',{name:'Language',exact:true}).focus();
    await page.keyboard.press('Enter');await page.keyboard.press('End');await page.keyboard.press('Enter');
    await expect(page.locator('html')).toHaveAttribute('lang','ru');
    await expect(page.getByRole('menu')).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Язык',exact:true})).toBeFocused();
    await page.keyboard.press('Escape');
    await expect(page.getByRole('textbox',{name:'Сообщение',exact:true})).toBeVisible();
    await page.reload();
    await expect(page.getByRole('textbox',{name:'Сообщение',exact:true})).toHaveValue('');
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
  } finally {await browser.close();}
});

test('files and photos send without text, survive reload and download; mobile themes stay usable', {timeout:60000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);
    await page.goto(new URL('chat/',base).href);
    await newConversation(page);
    await page.getByRole('heading',{name:'Есть мысль?'}).waitFor();
    const photo=await page.screenshot({clip:{x:0,y:0,width:2,height:3}});
    await page.getByRole('button',{name:'Прикрепить файлы и фото'}).click();
    await page.locator('input[type=file]').setInputFiles([
      {name:'notes.txt',mimeType:'text/plain',buffer:Buffer.from('private notes')},
      {name:'photo.png',mimeType:'image/png',buffer:photo},
      {name:'remove.txt',mimeType:'text/plain',buffer:Buffer.from('remove me')},
    ]);
    await page.getByRole('button',{name:/remove.txt/}).click();
    await page.locator('[data-slot="base"][role="button"]').evaluate(el=>{
      const data=new DataTransfer();data.items.add(new File(['one copy'],'dropzone.txt',{type:'text/plain'}));
      el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));
    });
    await expect(page.getByText('dropzone.txt',{exact:true})).toHaveCount(1);
    await page.getByRole('button',{name:'Отправить',exact:true}).click();
    await page.getByText('Привет!',{exact:true}).waitFor();
    await page.reload();
    const notes=page.locator('.message').getByRole('link',{name:'notes.txt',exact:true});
    await notes.waitFor();
    await expect(page.locator('.message img')).toHaveJSProperty('naturalWidth',2);
    assert.equal(await page.locator('.message').getByText('remove.txt',{exact:true}).count(),0);
    const response=await page.request.get(new URL(await notes.getAttribute('href'),page.url()).href);
    assert.equal(await response.text(),'private notes');
    const [download]=await Promise.all([page.waitForEvent('download'),notes.click()]);
    assert.equal(download.suggestedFilename(),'notes.txt');
    const input=page.getByRole('textbox',{name:'Сообщение'});
    await input.evaluate(el=>{
      const data=new DataTransfer();data.items.add(new File(['send with Enter'],'enter.txt',{type:'text/plain'}));
      el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));
    });
    await input.press('Enter');
    await expect(page.locator('.message').getByRole('link',{name:'enter.txt',exact:true})).toHaveCount(1);
    await expect(page.getByText('Привет!',{exact:true})).toHaveCount(2);
    await page.setViewportSize({width:390,height:844});
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    await page.getByRole('button',{name:'Показать диалоги'}).click();
    await page.getByRole('button',{name:'Переключить тему'}).click();
    await page.getByRole('menuitemcheckbox',{name:'Тёмная',exact:true}).click();
    const primary=await page.evaluate(()=>getComputedStyle(document.documentElement).getPropertyValue('--ui-primary').trim());
    assert.ok(['#89b4fa','#1e66f5'].includes(primary));
    await page.keyboard.press('Escape');
    await expect(page.getByRole('dialog')).toHaveCount(0);
    await page.screenshot({path:'/tmp/kodrik-nuxt-mobile.png',fullPage:true});
    await page.setViewportSize({width:1440,height:1000});
    await page.screenshot({path:'/tmp/kodrik-nuxt-desktop.png',fullPage:true});
  }finally{await browser.close();}
});

test('paste and drop attach files; lost multipart response reconciles after reload without resubmission', {timeout:60000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);await page.goto(new URL('chat/',base).href);
    await newConversation(page);
    const input=page.getByRole('textbox',{name:'Сообщение'});
    await expect(input).toHaveJSProperty('readOnly',false);
    const photo=await page.screenshot({clip:{x:0,y:0,width:2,height:3}});
    await input.evaluate((el,bytes)=>{
      const data=new DataTransfer();data.items.add(new File([new Uint8Array(bytes)],'paste.png',{type:'image/png'}));
      el.dispatchEvent(new ClipboardEvent('paste',{clipboardData:data,bubbles:true,cancelable:true}));
    },[...photo]);
    await page.getByText('paste.png',{exact:true}).waitFor();
    await input.evaluate(el=>{
      const data=new DataTransfer();data.items.add(new File(['dropped'],'drop.txt',{type:'text/plain'}));
      el.dispatchEvent(new DragEvent('drop',{dataTransfer:data,bubbles:true,cancelable:true}));
    });
    await page.getByText('drop.txt',{exact:true}).waitFor();
    let submissions=0,statusChecks=0;
    page.on('request',r=>{if(r.url().includes('/requests/'))statusChecks++;});
    await page.route('**/api/chat/send',async route=>{
      submissions++;await route.fetch();await route.abort('failed');
    });
    await input.fill('attachments with uncertain response');
    await input.press('Enter');
    await expect.poll(()=>submissions).toBe(1);
    await expect(input).toHaveValue('attachments with uncertain response');
    await expect(input).toHaveJSProperty('readOnly',false);
    await page.reload();
    await expect(input).toHaveValue('attachments with uncertain response');
    await expect(input).toHaveJSProperty('readOnly',false);
    await input.press('Enter');
    await expect.poll(()=>statusChecks).toBeGreaterThan(0);
    await expect(input).toHaveValue('');
    assert.equal(submissions,1);
    await expect(page.locator('.message img')).toHaveJSProperty('naturalWidth',2);
    await expect(page.locator('.message').getByRole('link',{name:'drop.txt',exact:true})).toHaveCount(1);
  }finally{await browser.close();}
});

test('Markdown renders formatting without executing HTML, Vue components or unsafe links', {timeout:30000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);await page.goto(new URL('chat/',base).href);
    await newConversation(page);
    const input=page.getByRole('textbox',{name:'Сообщение'});
    await input.fill('**Safe bold**\n\n[Safe link](https://example.com/)\n\n<script>window.chatInjection = true</script>\n\n<img src=x onerror="window.chatInjection=true">\n\n[unsafe](javascript:alert(1))\n\n::u-button\nInjected component\n::');
    await page.getByRole('button',{name:'Отправить',exact:true}).click();
    await page.getByText('Привет!',{exact:true}).waitFor();
    await page.locator('.message strong').getByText('Safe bold',{exact:true}).waitFor();
    assert.equal(await page.evaluate(()=>window.chatInjection),undefined);
    assert.equal(await page.locator('.message script, .message [onerror], .message a[href^="javascript:"]').count(),0);
    assert.equal(await page.locator('.message').getByRole('button',{name:'Injected component',exact:true}).count(),0);
    const link=page.getByRole('link',{name:'Safe link',exact:true});
    await expect(link).toHaveAttribute('target','_blank');
    await expect(link).toHaveAttribute('rel',/noopener/);
    await page.context().route('https://example.com/',route=>route.fulfill({contentType:'text/html',body:'New tab'}));
    const original=page.url();
    const [popup]=await Promise.all([page.waitForEvent('popup'),link.click()]);
    await popup.waitForLoadState('domcontentloaded');
    assert.equal(popup.url(),'https://example.com/');
    assert.equal(page.url(),original);
    await popup.close();
  }finally{await browser.close();}
});

test('closing tab during a tool preserves one effect and one answer', {timeout:30000}, async()=>{
  const browser=await chromium.launch({headless:true});
  const priorEffects=(await readFile(join(storage,'effects.txt'),'utf8').catch(()=> '')).trim().split('\n').filter(Boolean).length;
  try {
    const page=await signedPage(browser);await page.goto(new URL('chat/',base).href);
    await newConversation(page);
    await page.getByRole('textbox',{name:'Сообщение',exact:true}).fill('slowtool');await page.getByRole('textbox',{name:'Сообщение',exact:true}).press('Enter');
    await page.getByRole('button',{name:'Выполняются шаги: 1',exact:true}).click();
    await page.getByText('slow_tool',{exact:true}).waitFor();
    const url=page.url();await page.close();
    const reopened=await signedPage(browser);await reopened.goto(url);
    await expect(reopened.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(1);
    await reopened.getByRole('button',{name:'Выполнено шагов: 1',exact:true}).click();
    await expect(reopened.getByText('slow_tool',{exact:true})).toHaveCount(1);
    assert.equal((await readFile(join(storage,'effects.txt'),'utf8')).trim().split('\n').filter(Boolean).length,priorEffects+1);
    await reopened.reload();await expect(reopened.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(1);
  }finally{await browser.close();}
});

test('network loss reconnects without cancelling; stop interrupts persisted retry', {timeout:30000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);await page.goto(new URL('chat/',base).href);
    await newConversation(page);
    await page.getByRole('textbox',{name:'Сообщение',exact:true}).fill('backoff');await page.getByRole('textbox',{name:'Сообщение',exact:true}).press('Enter');
    await page.getByRole('button',{name:'Остановить',exact:false}).waitFor();
    // Abort the observer, then force its reconnect requests to fail.
    await page.route('**/events',route=>route.abort());
    await page.reload();
    await page.getByText('Соединение потеряно, работа продолжается',{exact:true}).waitFor();
    await page.unrouteAll();
    await expect(page.getByText('Соединение потеряно, работа продолжается',{exact:true})).toHaveCount(0);
    await page.getByRole('button',{name:'Остановить',exact:false}).click();
    await page.getByRole('button',{name:'Отправить'}).waitFor();
    assert.equal(await page.getByText('Привет!',{exact:true}).count(),0);
  }finally{await browser.close();}
});

test('backend process restart restores the same run and attached browser', {timeout:60000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await signedPage(browser);await page.goto(new URL('chat/',base).href);
    await newConversation(page);
    await page.getByRole('textbox',{name:'Сообщение',exact:true}).fill('restart');await page.getByRole('textbox',{name:'Сообщение',exact:true}).press('Enter');
    await page.getByText('Перед перезапуском',{exact:true}).waitFor();
    const id=new URL(page.url()).searchParams.get('conversation');
    const before=await (await request(`sessions/${id}/run`)).json();
    const listen=new URL(base).host;
    const exited=once(server,'exit');process.kill(-server.pid,'SIGKILL');await exited;
    await page.getByText('Соединение потеряно, работа продолжается',{exact:true}).waitFor();
    await start(listen);
    await expect(page.locator('.message').getByText('Привет!',{exact:true})).toHaveCount(1,{timeout:15000});
    const after=await (await request(`sessions/${id}/run`)).json();
    assert.equal(after.run_id,before.run_id);assert.equal(after.phase,'Completed');
  }finally{await browser.close();}
});

test('logout revokes an existing watch and 401 stops reconnect', {timeout:30000}, async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    // Use a separate login: revoking it must not invalidate the fixture cookie.
    const context=await browser.newContext({locale:'ru-RU'});const page=await context.newPage();
    await page.goto(new URL('chat/',base).href);
    await page.locator('input[type=password]').fill('preview-password');await page.locator('button[type=submit]').click();
    await newConversation(page);
    await page.getByRole('textbox',{name:'Сообщение',exact:true}).fill('wait');await page.getByRole('textbox',{name:'Сообщение',exact:true}).press('Enter');
    await page.getByText('Жду остановки',{exact:true}).waitFor();
    const url=page.url();const second=await context.newPage();await second.goto(url);
    await second.getByText('Жду остановки',{exact:true}).waitFor();
    let watches=0;page.on('request',r=>{if(r.url().endsWith('/events')||r.url().endsWith('/run'))watches++;});
    await second.getByRole('button',{name:'Выйти',exact:true}).click();
    await page.getByText('Сессия истекла. Войдите снова.',{exact:false}).waitFor();
    const count=watches;await new Promise(resolve=>setTimeout(resolve,1500));assert.equal(watches,count);
    assert.ok(await page.getByRole('link',{name:'Войти',exact:true}).count());
    const id=new URL(url).searchParams.get('conversation');
    assert.equal((await (await request(`sessions/${id}/run`)).json()).busy,true);
    await request(`sessions/${id}/stop`,{});
  }finally{await browser.close();}
});

// Keep approval scenarios after the fixture restart to isolate the login rate-limit window.
async function decideTelegram(id,approve,user=7) {
  const response=await fetch(new URL('__test/telegram-login',base),{
    method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({id,approve,user}),
  });
  assert.ok(response.ok);return response.json();
}
function loginId(link) {return new URL(link).searchParams.get('start').slice('login_'.length);}

for (const short of [true,false]) {
test(`${short?'short':'legacy'} invitation landing scrubs the secret and sends it only in Telegram start JSON`, {timeout:60000},async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'ru-RU'}),invite=short?'AbCdEfGhIjKlMnOpQrSt_w':'ab'.repeat(32),requests=[],bodies=[];
    const url=new URL(short?'i':'auth/login?return_to=/kodrik/chat/',base);url.hash=short?invite:`invite=${invite}`;
    const redirect=new URL('chat/?invitation-accepted',base).href;
    let approved=false;
    page.on('request',request=>requests.push({url:request.url(),referer:request.headers().referer || ''}));
    await page.route('**/auth/telegram/start',route=>{
      bodies.push(route.request().postDataJSON());
      return route.fulfill({json:{url:'https://t.me/TestBot?start=login_abcdef12345678901234567890123456',verification:'ABCDEF',expiresIn:300}});
    });
    await page.route('**/auth/telegram/poll',route=>route.fulfill({json:approved?{status:'approved',redirect}:{status:'pending'}}));
    await page.route('**/auth/telegram/cancel',route=>route.fulfill({status:204}));
    await page.route('**/chat/?invitation-accepted',route=>route.fulfill({contentType:'text/html',body:'<h1>Accepted</h1>'}));
    await page.goto(url.href);
    await expect(page.getByRole('heading',{name:'Приглашение в Кодрик',exact:true})).toBeVisible();
    assert.equal(new URL(page.url()).hash,'');
    assert.ok(!(await page.evaluate(()=>JSON.stringify(history.state))).includes(invite));
    await expect(page.getByLabel('Пароль',{exact:true})).toHaveCount(0);
    await expect(page.getByRole('button',{name:'Войти по коду из Telegram',exact:true})).toHaveCount(0);
    await page.getByRole('button',{name:'Войти через Telegram',exact:true}).click();
    await expect(page.getByRole('link',{name:'Открыть Telegram',exact:true})).toBeVisible();
    assert.deepEqual(bodies,[{return_to:'/kodrik/chat/',invite}]);
    assert.ok(!(await page.content()).includes(invite));
    assert.ok(!(await page.evaluate(()=>JSON.stringify({local:{...localStorage},session:{...sessionStorage}}))).includes(invite));
    await page.getByRole('button',{name:'Отменить',exact:true}).click();
    await expect(page.getByRole('heading',{name:'Приглашение в Кодрик',exact:true})).toBeVisible();
    await page.getByRole('button',{name:'Войти через Telegram',exact:true}).click();
    await expect(page.getByRole('link',{name:'Открыть Telegram',exact:true})).toBeVisible();
    assert.deepEqual(bodies[1],bodies[0],'cancel must retain the invitation in memory');
    approved=true;
    await expect(page).toHaveURL(redirect);
    assert.ok(requests.every(request=>!request.url.includes(invite)&&!request.referer.includes(invite)));
  } finally {await browser.close();}
});

}

test('invalid invitation stays in Telegram flow and displays the server rejection', {timeout:60000},async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'ru-RU'});
    await page.route('**/auth/telegram/start',route=>route.fulfill({status:400,json:{error:'Приглашение недействительно. Попросите владельца прислать новое.'}}));
    await page.goto(new URL('auth/login#invite=invalid',base).href);
    await page.getByRole('button',{name:'Войти через Telegram',exact:true}).click();
    await expect(page.getByRole('alert')).toContainText('Приглашение недействительно');
    assert.equal(new URL(page.url()).hash,'');
    await expect(page.getByLabel('Пароль',{exact:true})).toHaveCount(0);
    await page.getByRole('button',{name:'Отменить',exact:true}).click();
    await expect(page.getByRole('heading',{name:'Приглашение в Кодрик',exact:true})).toBeVisible();
  } finally {await browser.close();}
});

test('Telegram approval login uses local QR and enters only the initiating browser', {timeout:60000},async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'ru-RU',viewport:{width:390,height:844}}),errors=[];
    page.on('pageerror',error=>errors.push(error.message));
    await page.goto(new URL('auth/login?return_to=/kodrik/chat/',base).href);
    await page.getByRole('button',{name:'Войти через Telegram',exact:true}).click();
    const open=page.getByRole('link',{name:'Открыть Telegram',exact:true});
    await expect(open).toBeVisible();
    const link=await open.getAttribute('href'),id=loginId(link);
    await expect(page.locator('[data-login-verification]')).toHaveText(id.slice(0,6).toUpperCase());
    const qr=page.getByRole('img',{name:'QR-код для входа через Telegram'});
    await expect(qr).toBeVisible();
    assert.match(await qr.getAttribute('src'),/^data:image\/svg\+xml,/);
    assert.ok(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth));
    const stranger=await fetch(new URL('auth/telegram/poll',base),{method:'POST',headers:{Origin:new URL(base).origin}});
    assert.equal((await stranger.json()).status,'expired');
    assert.ok(!stranger.headers.get('set-cookie').includes('kodrik_web_session'));
    await page.screenshot({path:'/tmp/kodrik-telegram-login-mobile.png',fullPage:true});
    await page.setViewportSize({width:1280,height:900});
    await page.screenshot({path:'/tmp/kodrik-telegram-login-desktop.png',fullPage:true});
    assert.equal((await decideTelegram(id,true)).ok,true);
    await expect(page).toHaveURL(new URL('chat/',base).href);
    await expect(page.locator('nav[aria-label="История диалогов"] [data-conversation-id]')).toHaveCount(1);
    assert.deepEqual(errors,[]);
  } finally {await browser.close();}
});

test('Telegram denial allows retry and cancellation restores password login', {timeout:60000},async()=>{
  const browser=await chromium.launch({headless:true});
  try {
    const page=await browser.newPage({locale:'ru-RU'});
    await page.goto(new URL('auth/login?return_to=/kodrik/chat/',base).href);
    await page.getByRole('button',{name:'Войти через Telegram',exact:true}).click();
    const open=page.getByRole('link',{name:'Открыть Telegram',exact:true});
    await expect(open).toBeVisible();
    const first=loginId(await open.getAttribute('href'));
    assert.equal((await decideTelegram(first,false)).ok,true);
    await expect(page.getByRole('alert')).toContainText('Вход отклонён');
    await page.getByRole('button',{name:'Попробовать снова',exact:true}).click();
    await expect(open).toBeVisible();
    const second=loginId(await open.getAttribute('href'));assert.notEqual(second,first);
    await page.getByRole('button',{name:'Другой способ входа',exact:true}).click();
    await expect(page.getByLabel('Пароль',{exact:true})).toBeVisible();
    assert.equal((await decideTelegram(second,true)).ok,false);
  } finally {await browser.close();}
});

test('Telegram cancellation ignores an already in-flight approved poll', {timeout:60000},async()=>{
  const browser=await chromium.launch({headless:true});
  let finishPoll,finishCancel,pollStarted,cancelStarted;
  const pollReady=new Promise(resolve=>{pollStarted=resolve}),cancelReady=new Promise(resolve=>{cancelStarted=resolve});
  const releasePoll=new Promise(resolve=>{finishPoll=resolve}),releaseCancel=new Promise(resolve=>{finishCancel=resolve});
  try {
    const page=await browser.newPage({locale:'ru-RU'}),url=new URL('auth/login',base).href;
    await page.route('**/auth/telegram/start',route=>route.fulfill({json:{url:'https://t.me/TestBot?start=login_abcdef12345678901234567890123456',verification:'ABCDEF',expiresIn:300}}));
    await page.route('**/auth/telegram/poll',async route=>{pollStarted();await releasePoll;await route.fulfill({json:{status:'approved',redirect:new URL('chat/?unexpected-login',base).href}});});
    await page.route('**/auth/telegram/cancel',async route=>{cancelStarted();await releaseCancel;await route.fulfill({status:204});});
    await page.goto(url);
    await page.getByRole('button',{name:'Войти через Telegram',exact:true}).click();
    await pollReady;
    await page.getByRole('button',{name:'Другой способ входа',exact:true}).click();
    await cancelReady;
    const polled=page.waitForResponse(response=>response.url().endsWith('/auth/telegram/poll'));
    finishPoll();await(await polled).finished();
    await page.evaluate(()=>new Promise(resolve=>requestAnimationFrame(()=>resolve())));
    assert.equal(page.url(),url,'canceled approval must not redirect');
    finishCancel();
    await expect(page.getByLabel('Пароль',{exact:true})).toBeVisible();
  } finally {finishPoll?.();finishCancel?.();await browser.close();}
});
