import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile,readdir} from 'node:fs/promises';
import {catalogs} from '../app/utils/translations.ts';
import {detectLanguage,initialLanguage,setLanguage,language,locale,t,formatUiValue} from '../app/utils/i18n.ts';
import {formatMessageTime,conversationTitle} from '../app/utils/chat-presentation.ts';
import {createUiDateFormatter} from '../app/utils/ui-date.ts';
import {prepareUiTableColumns} from '../app/utils/ui-table.ts';

test('browser language detection uses Russian only for Russian locales and honors a valid saved choice',()=>{
  for(const value of ['ru','ru-RU','ru-BY','RU-kz']) assert.equal(detectLanguage(value),'ru');
  for(const value of [undefined,'','en-US','fr-FR','uk-UA','russian']) assert.equal(detectLanguage(value),'en');
  assert.equal(initialLanguage('ru-RU','en'),'en');
  assert.equal(initialLanguage('fr-FR','ru'),'ru');
  assert.equal(initialLanguage('ru-RU','invalid'),'ru');
  assert.equal(initialLanguage('de-DE','invalid'),'en');
  assert.equal(initialLanguage('ru-RU','system'),'ru');
  assert.equal(initialLanguage('en-US','system'),'en');
});

test('catalogs have matching keys, nonempty translations and identical interpolation parameters',async()=>{
  assert.deepEqual(Object.keys(catalogs.en).sort(),Object.keys(catalogs.ru).sort());
  for(const [key,value] of Object.entries(catalogs.en)) {
    assert.ok(value.trim(),key);
    assert.deepEqual((key.match(/\{\d+\}/g)??[]).sort(),(value.match(/\{\d+\}/g)??[]).sort(),key);
  }
  for(const folder of ['components','utils']) for(const name of await readdir(new URL(`../app/${folder}/`,import.meta.url))) {
    if(!/\.(vue|ts)$/.test(name)||name==='translations.ts') continue;
    const source=await readFile(new URL(`../app/${folder}/${name}`,import.meta.url),'utf8');
    for(const match of source.matchAll(/\bt\((['"])([^'"\n]+)\1/g)) assert.ok(Object.hasOwn(catalogs.en,match[2]),`${name}: ${match[2]}`);
  }
});

test('switching language persists the choice and translates existing errors without altering unknown details',()=>{
  const old=Object.getOwnPropertyDescriptor(globalThis,'localStorage'),values=new Map();
  Object.defineProperty(globalThis,'localStorage',{configurable:true,value:{setItem:(key,value)=>values.set(key,value)}});
  try {
    setLanguage('en');
    const message=t('Не удалось загрузить данные ({0}).',503);
    assert.equal(message,'Could not load data (503).');
    assert.equal(values.get('kodrik.language'),'en');
    setLanguage('system');
    assert.equal(values.get('kodrik.language'),'system');
    assert.equal(language.value,initialLanguage(globalThis.navigator?.language));
    setLanguage('en');
    assert.equal(t('Недоступная пользовательская запись'),'Недоступная пользовательская запись');
    setLanguage('ru');
    assert.equal(t(message),'Не удалось загрузить данные (503).');
    assert.equal(locale.value,'ru-RU');
    Object.defineProperty(globalThis,'localStorage',{configurable:true,get(){throw new Error('Storage denied')}});
    assert.doesNotThrow(()=>setLanguage('en'));
    assert.equal(language.value,'en');
  } finally {if(old) Object.defineProperty(globalThis,'localStorage',old);else delete globalThis.localStorage;}
});

test('English dates, timestamps and numbers follow the selected locale while explicit document locales win',()=>{
  setLanguage('en');
  const now=new Date(2026,9,22,12),date=new Date(2026,9,22,9,5);
  assert.equal(formatMessageTime(date.toISOString(),now),`Today, ${date.toLocaleTimeString('en-US',{hour:'2-digit',minute:'2-digit'})}`);
  assert.match(formatMessageTime(new Date(2026,9,21,9,5).toISOString(),now),/^Yesterday,/);
  assert.match(formatMessageTime(new Date(2025,9,20,9,5).toISOString(),now),/^October 20, 2025,/);
  assert.equal(createUiDateFormatter('long')('2026-10-21'),'October 21, 2026');
  assert.equal(createUiDateFormatter({locale:'ru-RU',dateStyle:'long'})('2026-10-21'),'21 октября 2026 г.');
  assert.equal(formatUiValue(1234.5),'1,234.5');
  assert.equal(formatUiValue(null),'No data');
  const columns=prepareUiTableColumns([{accessorKey:'amount'}],true);
  assert.equal(columns[0].cell({getValue:()=>1234.5}),'1,234.5');
  setLanguage('ru');
  assert.equal(formatUiValue(1234.5),new Intl.NumberFormat('ru-RU').format(1234.5));
  assert.equal(columns[0].cell({getValue:()=>1234.5}),new Intl.NumberFormat('ru-RU').format(1234.5));
});

test('only explicitly marked default conversation titles translate',()=>{
  setLanguage('en');
  assert.equal(conversationTitle({title:'Новый диалог',title_default:true}),'New conversation');
  assert.equal(conversationTitle({title:'Новый диалог',title_default:false}),'Новый диалог');
  assert.equal(conversationTitle({title:'Мои заметки'}),'Мои заметки');
});

test('API errors send the selected language and localize known payloads while preserving unknown details',async()=>{
  globalThis.window={location:{href:'http://localhost/kodrik/chat/'}};
  const {checkedFetch,messageOf,AuthenticationError,ConversationNotFoundError}=await import('../app/utils/api.ts');
  const original=globalThis.fetch;
  try {
    setLanguage('en');
    for(const [status,error,expected,kind] of [
      [401,'','Your session has expired. Sign in again.',AuthenticationError],
      [404,'','Conversation was deleted or could not be found.',ConversationNotFoundError],
      [409,'Resume the queue in its original interface','Resume the pending queue in its original Telegram or CLI interface.',Error],
      [429,'Inbox queue is full','The message queue is full. Wait for earlier messages to finish.',Error],
      [413,'','File or message is too large.',Error],
      [400,'Страница обновлена. Загрузите её заново.','The page has changed. Reload it.',Error],
      [400,'Unknown technical detail','Unknown technical detail',Error],
      [503,'','Request failed (503).',Error],
    ]) {
      globalThis.fetch=async(_,init)=>{assert.equal(init.headers.get('Accept-Language'),'en-US');assert.equal(init.headers.get('X-Test'),'kept');return new Response(JSON.stringify({error}),{status})};
      await assert.rejects(checkedFetch('/api',{headers:{'X-Test':'kept'}}),cause=>cause instanceof kind&&cause.message.startsWith(expected));
    }
    assert.equal(messageOf(new TypeError('Failed to fetch')),'Could not connect to the server.');
    setLanguage('ru');
    globalThis.fetch=async(_,init)=>{assert.equal(init.headers.get('Accept-Language'),'ru-RU');return new Response('',{status:401})};
    await assert.rejects(checkedFetch('/api'),/Сессия истекла/);
  } finally {globalThis.fetch=original;delete globalThis.window;}
});
