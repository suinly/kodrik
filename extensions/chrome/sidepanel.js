import {serverURL, pageContext, extractPage} from './core.mjs';
const $ = id => document.getElementById(id);
const frame = $('chat');
let server, windowId, channel, ready = false;
let includeContext = true, tabMetadata = {title:'',url:''};
const connection = $('connection');
function openSettings() { if (!connection.open) connection.showModal(); }
const ownerId = Number(new URLSearchParams(location.search).get('tab'));
const bound = Number.isInteger(ownerId) && ownerId > 0;
const storageKey = `tab-chat:${ownerId}`;
const status = text => { $('status').textContent = text; };
function post(message) { if (server) frame.contentWindow.postMessage(message, server.origin); }
async function activeTab() {
  const tab = bound ? await chrome.tabs.get(ownerId) : (await chrome.tabs.query({active:true,windowId}))[0];
  if (!tab?.id) throw new Error('Нет активной вкладки.');
  return tab;
}
async function preview() {
  try {
    const tab = await activeTab();
    tabMetadata = {title:tab.title || '',url:tab.url || ''};
    if (ready) handshake();
  } catch (error) {status(error.message);}
}
function validChatPath(path) {
  if(typeof path !== 'string' || !path.startsWith('/')) return false;
  let url;
  try {url=new URL(path,server);} catch {return false;}
  const root=new URL('app/chat',server).pathname;
  return url.origin===server.origin && !url.search && !url.hash && (url.pathname===root || url.pathname.startsWith(root+'/'));
}
function handshake() {
  if (!server || frame.hidden) return;
  post({type:'kodrik-tab-init',channel,enabled:includeContext,tab:tabMetadata});
}
async function load(value) {
  server = serverURL(value);
  $('server').value = server.href;
  channel = crypto.randomUUID();
  ready = false;
  let url = new URL('app/chat', server);
  if(bound) {
    const saved = (await chrome.storage.session.get(storageKey))[storageKey];
    if(saved?.server === server.href && validChatPath(saved.path)) url = new URL(saved.path,server);
  }
  url.searchParams.set('browser_sidebar', '1');
  frame.src = url.href;
  frame.hidden = false;
  connection.close();
  status('Ожидаю чат. Если он не появился, войдите через «Войти ↗» и подключитесь снова.');
}
$('connect').addEventListener('submit', async event => {
  event.preventDefault();
  try {
    const url = serverURL($('server').value);
    if (!await chrome.permissions.request({origins:[`${url.origin}/*`]})) throw new Error('Для подключения нужен доступ к адресу Кодрика.');
    await chrome.storage.local.set({server:url.href});
    await load(url.href);
  } catch (error) {status(error.message);}
});
$('close-connection').onclick = () => connection.close();
connection.addEventListener('click', event => {
  if (event.target !== connection) return;
  const rect = connection.getBoundingClientRect();
  if (event.clientX < rect.left || event.clientX > rect.right || event.clientY < rect.top || event.clientY > rect.bottom) connection.close();
});
$('login').onclick = () => {if(server) void chrome.tabs.create({url:new URL('auth/login',server).href});else status('Сначала укажите адрес Кодрика.');};
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
  if(event.data?.type === 'kodrik-tab-location') {
    if(bound && validChatPath(event.data.path)) await chrome.storage.session.set({[storageKey]:{server:server.href,path:event.data.path}});
    return;
  }
  if(event.data?.channel !== channel) return;
  const message = event.data;
  if(message.type === 'kodrik-tab-ready') {ready=true;status('');return;}
  if(message.type === 'kodrik-tab-settings') {openSettings();return;}
  if(message.type === 'kodrik-tab-toggle') {
    if(typeof message.enabled === 'boolean') {includeContext=message.enabled;handshake();}
    return;
  }
  if(!['kodrik-tab-capture','kodrik-tab-preview'].includes(message.type) || typeof message.id !== 'string') return;
  try {
    if(message.type === 'kodrik-tab-capture' && !includeContext) {post({type:'kodrik-tab-result',channel,id:message.id,context:null});return;}
    const tab = await activeTab();
    if(!/^https?:/.test(tab.url || '') || (tab.url && new URL(tab.url).origin === server.origin)) throw new Error('Эта вкладка недоступна для чтения. Выберите обычную страницу или отключите контекст.');
    const results = await chrome.scripting.executeScript({target:{tabId:tab.id},func:extractPage});
    const page = results[0]?.result;
    if(!page) throw new Error('Не удалось прочитать текст вкладки.');
    const current = await activeTab();
    if(current.id !== tab.id || current.url !== page.url) throw new Error('Вкладка изменилась во время чтения. Повторите отправку.');
    await preview();
    status('');
    post({type:'kodrik-tab-result',channel,id:message.id,context:pageContext(page)});
  } catch(error) {
    const text = `${error.message} Если Chrome не дал доступ — нажмите «Разрешить чтение сайтов» или ещё раз значок расширения на нужной вкладке.`;
    status(text);$('access').hidden=false;
    post({type:'kodrik-tab-result',channel,id:message.id,error:text});
  }
});
chrome.tabs.onActivated.addListener(info=>{if(!bound && info.windowId===windowId) void preview();});
chrome.tabs.onUpdated.addListener((_id,change,tab)=>{if((bound ? _id===ownerId : tab.active && tab.windowId===windowId) && (change.title || change.url || change.status==='complete')) void preview();});
windowId = (await chrome.windows.getCurrent()).id;
await preview();
try {const saved=await chrome.storage.local.get('server');if(saved.server) await load(saved.server);else openSettings();} catch(error){status(error.message);openSettings();}
