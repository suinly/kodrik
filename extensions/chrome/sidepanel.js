import {serverURL, contextText, extractPage} from './core.mjs';
const $ = id => document.getElementById(id);
const frame = $('chat');
let server, windowId, channel, ready = false;
const status = text => { $('status').textContent = text; };
function post(message) { if (server) frame.contentWindow.postMessage(message, server.origin); }
async function activeTab() {
  const [tab] = await chrome.tabs.query({active:true,windowId});
  if (!tab?.id) throw new Error('Нет активной вкладки.');
  return tab;
}
async function preview() {
  try {
    const tab = await activeTab();
    $('tab').textContent = tab.title || tab.url || 'Вкладка: доступ потребуется при отправке';
    $('tab').title = tab.url || '';
  } catch (error) {status(error.message);}
}
function handshake() {
  if (!server || frame.hidden) return;
  post({type:'kodrik-tab-init',channel,enabled:$('include').checked});
}
function load(value) {
  server = serverURL(value);
  $('server').value = server.href;
  channel = crypto.randomUUID();
  ready = false;
  const url = new URL('app/chat', server);
  url.searchParams.set('browser_sidebar', '1');
  frame.src = url.href;
  frame.hidden = false;
  $('connect').hidden = true;
  status('Ожидаю чат. Если он не появился, войдите через «Войти ↗» и подключитесь снова.');
}
$('connect').addEventListener('submit', async event => {
  event.preventDefault();
  try {
    const url = serverURL($('server').value);
    if (!await chrome.permissions.request({origins:[`${url.origin}/*`]})) throw new Error('Для подключения нужен доступ к адресу Кодрика.');
    await chrome.storage.local.set({server:url.href});
    load(url.href);
  } catch (error) {status(error.message);}
});
$('settings').onclick = () => { $('connect').hidden = !$('connect').hidden; };
$('login').onclick = () => {if(server) void chrome.tabs.create({url:new URL('auth/login',server).href});else status('Сначала укажите адрес Кодрика.');};
$('include').onchange = () => {handshake();};
$('access').onclick = async () => {
  try {
    if(await chrome.permissions.request({origins:['https://*/*','http://*/*']})) { $('access').hidden=true;status('Доступ разрешён. Повторите отправку вопроса.');await preview(); }
  } catch(error) {status(error.message);}
};
frame.addEventListener('load',()=>{ready=false;handshake();});
// A ready message can race iframe load while Vue is booting; retry the handshake only.
setInterval(()=>{if(!ready) handshake();},1000);
window.addEventListener('message', async event => {
  if (!server || event.source !== frame.contentWindow || event.origin !== server.origin) return;
  if(event.data?.type === 'kodrik-tab-connect') {handshake();return;}
  if(event.data?.channel !== channel) return;
  const message = event.data;
  if(message.type === 'kodrik-tab-ready') {ready=true;status('');return;}
  if(message.type !== 'kodrik-tab-capture' || typeof message.id !== 'string') return;
  try {
    if(!$('include').checked) {post({type:'kodrik-tab-result',channel,id:message.id,context:null});return;}
    const tab = await activeTab();
    if(!/^https?:/.test(tab.url || '') || (tab.url && new URL(tab.url).origin === server.origin)) throw new Error('Эта вкладка недоступна для чтения. Выберите обычную страницу или отключите контекст.');
    const results = await chrome.scripting.executeScript({target:{tabId:tab.id},func:extractPage});
    const page = results[0]?.result;
    if(!page) throw new Error('Не удалось прочитать текст вкладки.');
    const current = await activeTab();
    if(current.id !== tab.id || current.url !== page.url) throw new Error('Вкладка изменилась во время чтения. Повторите отправку.');
    await preview();
    post({type:'kodrik-tab-result',channel,id:message.id,context:contextText(page)});
  } catch(error) {
    const text = `${error.message} Если Chrome не дал доступ — нажмите «Разрешить чтение сайтов» или ещё раз значок расширения на нужной вкладке.`;
    status(text);$('access').hidden=false;
    post({type:'kodrik-tab-result',channel,id:message.id,error:text});
  }
});
chrome.tabs.onActivated.addListener(info=>{if(info.windowId===windowId) void preview();});
chrome.tabs.onUpdated.addListener((_id,change,tab)=>{if(tab.active && tab.windowId===windowId && (change.title || change.url || change.status==='complete')) void preview();});
windowId = (await chrome.windows.getCurrent()).id;
await preview();
try {const saved=await chrome.storage.local.get('server');if(saved.server) load(saved.server);} catch(error){status(error.message);}
