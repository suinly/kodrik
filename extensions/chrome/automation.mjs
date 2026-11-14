import {extractPage} from './core.mjs';
// Tokens live only in the isolated world; Chrome documentIds prevent navigation races.
const snapshots = new Map();
const MAX_BYTES = 100000;
function bounded(value) {
  const json = JSON.stringify(value ?? null);
  if (new TextEncoder().encode(json).length > MAX_BYTES) throw new Error('Browser result exceeds 100000 bytes');
  return JSON.parse(json);
}
function isolated(command) {
  try {
  if(command.forbidden_origin && location.origin===command.forbidden_origin) throw new Error('Forbidden origin');
  const visible=el=>{
    if(!el.getClientRects().length) return false;
    for(let node=el;node;node=node.parentElement) {const style=getComputedStyle(node);if(node.hidden || style.display==='none' || style.visibility==='hidden' || style.visibility==='collapse' || Number(style.opacity)===0) return false;}
    return true;
  };
  const safeText=el=>{
    if(!visible(el) || el.matches('input,textarea,select,[contenteditable]')) return '';
    const clone=el.cloneNode(true);
    const originals=[...el.querySelectorAll('*')],copies=[...clone.querySelectorAll('*')];
    originals.forEach((node,index)=>{if(!visible(node) || node.matches('input,textarea,select,[contenteditable],script,style,noscript')) copies[index].remove();});
    return clone.textContent || '';
  };
  const nameOf=el=>(el.getAttribute('aria-label') || (el.labels?.[0] ? safeText(el.labels[0]) : '') || el.getAttribute('title') || safeText(el)).trim().slice(0,150);
  const fingerprint=el=>JSON.stringify({name:nameOf(el),href:el.href || '',type:el.getAttribute('type') || '',formAction:el.form?.action || '',overrideAction:el.hasAttribute('formaction') ? el.formAction : '',formTarget:el.form?.target || '',overrideTarget:el.getAttribute('formtarget') || ''});
  const key = '__kodrikBrowserSnapshot';
  if (command.action === 'snapshot') {
    const state = {id:crypto.randomUUID(), url:location.href, elements:new Map()};
    globalThis[key] = state;
    const elements=[];
    for (const el of document.querySelectorAll('a,button,input,textarea,select,[contenteditable],[role],[tabindex]')) {
      if (!visible(el) || elements.length >= 150) continue;
      const id=crypto.randomUUID();state.elements.set(id,{el,fingerprint:fingerprint(el)});
      const name=nameOf(el);
      elements.push({id,role:el.getAttribute('role') || ({A:'link',BUTTON:'button',INPUT:'textbox',TEXTAREA:'textbox',SELECT:'combobox'}[el.tagName] || 'generic'),name,href:el.href || undefined,form_action:(el.hasAttribute('formaction') ? el.formAction : el.form?.action) || undefined,type:el.getAttribute('type') || undefined,disabled:!!el.disabled});
    }
    const result={snapshot_id:state.id,url:location.href.slice(0,2000),title:document.title.slice(0,500),text:'',elements};
    return result;
  }
  const state=globalThis[key];
  if (!state || state.id!==command.snapshot_id || state.url!==location.href) throw new Error('Stale snapshot or document');
  const entry=command.element_id ? state.elements.get(command.element_id) : null;
  const el=entry?.el;
  if (command.element_id && (!el || !el.isConnected || el.ownerDocument!==document)) throw new Error('Stale or detached element');
  if(el && (!visible(el) || fingerprint(el)!==entry.fingerprint)) throw new Error('Element target changed; take a new snapshot');
  if (command.action==='check') return true;
  if (command.action==='click') {
    if (!el || el.disabled || !el.getClientRects().length) throw new Error('Element cannot be clicked');
    el.click();
  } else if (command.action==='fill') {
    if (!el || el.disabled || el.readOnly) throw new Error('Element cannot be filled');
    if (el.matches('input[type=password],input[type=file]')) throw new Error('Cannot fill password or file input');
    if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement) {
      const proto=el instanceof HTMLInputElement ? HTMLInputElement.prototype : el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLSelectElement.prototype;
      Object.getOwnPropertyDescriptor(proto,'value').set.call(el,command.value);
    } else if (el.isContentEditable) el.textContent=command.value;
    else throw new Error('Element is not editable');
    el.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
    el.dispatchEvent(new Event('change',{bubbles:true}));
  } else if (command.action==='scroll') (el || window).scrollBy({left:command.x || 0,top:command.y || 0,behavior:'instant'});
  return {ok:true};
  } catch(error) {return {browser_error:error.message};}
}
function checked(result) {
  if(!result?.result) throw new Error('Browser document result unavailable');
  if(result.result.browser_error) throw new Error(result.result.browser_error);
  return result.result;
}
async function evaluateMain(code, timeout, forbiddenOrigin) {
  let timer;
  try {
    // MAIN obeys the page CSP. Never fall back to an eval bypass.
    if(forbiddenOrigin && location.origin===forbiddenOrigin) throw new Error('Forbidden origin');
    const fn=new Function(code);
    const value=await Promise.race([Promise.resolve().then(()=>fn()),new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('JavaScript timed out; execution may still be running')),timeout);})]);
    const json=JSON.stringify(value ?? null);
    if(new TextEncoder().encode(json).length>100000) throw new Error('Browser result exceeds 100000 bytes');
    return {value:JSON.parse(json)};
  } catch(error) {return {error:`JavaScript execution failed (page CSP may prohibit evaluation): ${error.message}`};}
  finally {clearTimeout(timer);}
}
export async function executeBrowserCommand(tabId, command, confirm, options={}) {
  if (!Number.isInteger(tabId) || tabId<=0) throw new Error('A bound tab is required');
  if (!command || !['snapshot','click','fill','scroll','evaluate'].includes(command.action)) throw new Error('Unknown browser action');
  if(command.action==='fill' && (typeof command.value!=='string' || command.value.length>50000)) throw new Error('Invalid fill value');
  if(command.action==='evaluate' && (typeof command.code!=='string' || command.code.length>50000)) throw new Error('Invalid JavaScript code');
  if(command.action==='scroll' && ['x','y'].some(k=>command[k]!==undefined && (!Number.isFinite(command[k]) || Math.abs(command[k])>10000))) throw new Error('Scroll must be finite pixels within 10000');
  if(['click','fill'].includes(command.action) && typeof command.element_id!=='string') throw new Error('element_id required');
  if(options.signal?.aborted) throw new Error('Browser action cancelled');
  command={...command,forbidden_origin:options.forbiddenOrigin || null};
  const tab=await chrome.tabs.get(tabId);
  if(options.forbiddenOrigin && new URL(tab.url).origin===options.forbiddenOrigin) throw new Error('Forbidden origin');
  if(!/^https?:/.test(tab.url || '')) throw new Error('Only HTTP(S) tabs are supported');
  if(command.action==='snapshot') {
    const [result]=await chrome.scripting.executeScript({target:{tabId},world:'ISOLATED',func:isolated,args:[command]});
    if(!result?.documentId || !result.result) throw new Error('Snapshot document unavailable');
    const value=bounded(checked(result));
    if(options.forbiddenOrigin && new URL(value.url).origin===options.forbiddenOrigin) throw new Error('Forbidden origin');
    const [text]=await chrome.scripting.executeScript({target:{tabId,documentIds:[result.documentId]},world:'ISOLATED',func:extractPage,args:[{allowEmpty:!!value.elements.length}]});
    if(text?.documentId!==result.documentId) throw new Error('Snapshot document changed');
    if(typeof text.result?.text!=='string') throw new Error('Snapshot text unavailable');
    value.text=text.result.text.slice(0,5000);
    bounded(value);
    snapshots.set(tabId,{id:value.snapshot_id,documentId:result.documentId,elements:value.elements});
    return value;
  }
  const snapshot=snapshots.get(tabId);
  if(!snapshot || snapshot.id!==command.snapshot_id) throw new Error('Stale snapshot');
  const target={tabId,documentIds:[snapshot.documentId]};
  const check=async()=>{const [result]=await chrome.scripting.executeScript({target,world:'ISOLATED',func:isolated,args:[{...command,action:'check'}]});return checked(result);};
  await check();
  if(typeof confirm!=='function' || !await confirm({...command,target:snapshot.elements.find(el=>el.id===command.element_id)},tab)) throw new Error('Browser action cancelled');
  if(snapshots.get(tabId)!==snapshot) throw new Error('Stale snapshot');
  await check();
  if(options.signal?.aborted) throw new Error('Browser action cancelled');
  if(command.action==='evaluate') {
    const timeout=Number.isFinite(command.timeout_ms) ? Math.max(1,Math.min(10000,command.timeout_ms)) : 5000;
    let timer;
    try {
      const [result]=await Promise.race([
        chrome.scripting.executeScript({target,world:'MAIN',func:evaluateMain,args:[command.code,timeout,options.forbiddenOrigin || null]}),
        new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error('JavaScript execution uncertain after 15s; may still run. New snapshot required; do not retry')),15000);})
      ]);
      if(!result?.result) throw new Error('JavaScript result unavailable; execution uncertain');
      if(result.result.error) throw new Error(result.result.error);
      return bounded(result.result.value);
    } catch(error) {snapshots.delete(tabId);throw error;}
    finally {clearTimeout(timer);}

  }
  const [result]=await chrome.scripting.executeScript({target,world:'ISOLATED',func:isolated,args:[command]});
  return bounded(checked(result));
}

export function confirmBrowserCommand(command, tab, signal) {
  if(signal?.aborted) return Promise.resolve(false);
  return new Promise(resolve=>{
    const dialog=document.createElement('dialog');
    dialog.setAttribute('aria-label','Разрешить действие во вкладке?');
    const title=document.createElement('h2');title.textContent='Разрешить действие во вкладке?';
    const description=document.createElement('pre');description.style.cssText='white-space:pre-wrap;max-height:45vh;overflow:auto';
    description.textContent=`${tab.title || ''}\n${tab.url}\n${JSON.stringify(command,null,2)}`;
    const cancel=document.createElement('button');cancel.textContent='Отмена';
    const accept=document.createElement('button');accept.textContent='Разрешить';
    const deadline=Date.now()+55000;
    const abort=()=>finish(false);
    const finish=value=>{clearTimeout(timer);signal?.removeEventListener('abort',abort);dialog.remove();resolve(value);};
    const timer=setTimeout(()=>finish(false),55000);
    signal?.addEventListener('abort',abort,{once:true});
    cancel.onclick=()=>finish(false);accept.onclick=()=>finish(Date.now()<deadline && !signal?.aborted);
    dialog.addEventListener('cancel',event=>{event.preventDefault();finish(false);});
    dialog.append(title,description,cancel,accept);document.body.append(dialog);dialog.showModal();cancel.focus();
  });
}
