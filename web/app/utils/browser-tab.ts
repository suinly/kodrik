export type BrowserCapture=BrowserContext & {document?:File}
export type BrowserContext={url:string;title:string;text:string;source:'page'|'selection';captured_at:string;truncated:boolean}
function validContext(value:unknown):value is BrowserContext {
  if(!value || typeof value!=='object') return false
  const c=value as BrowserContext
  if(typeof c.url!=='string' || c.url.length>4096 || typeof c.title!=='string' || c.title.length>1024 || typeof c.text!=='string' || !['page','selection'].includes(c.source) || typeof c.truncated!=='boolean' || typeof c.captured_at!=='string' || !Number.isFinite(Date.parse(c.captured_at))) return false
  try {
    const url=new URL(c.url)
    return ['http:','https:'].includes(url.protocol) && !url.username && !url.password && new TextEncoder().encode(JSON.stringify(c)).length<=100000
  } catch {return false}
}
// The extension never submits messages: it can only supply page data to an explicit Send.
export function createTabBridge(win:Window, timeout=90000) {
  let origin='',channel='',enabled=true
  let state={enabled:true,title:'Текущая вкладка',url:''}
  const subscribers=new Set<(state:{enabled:boolean;title:string;url:string})=>void>()
  const pending=new Map<string,{resolve:(context:BrowserCapture|null)=>void;reject:(error:Error)=>void;timer:ReturnType<typeof setTimeout>}>()
  const framed=win.parent!==win && new URLSearchParams(win.location.search).get('browser_sidebar')==='1'
  function receive(event:MessageEvent) {
    if(!framed || event.source!==win.parent || event.origin!=='chrome-extension://iiajbdommloibgimhfmoojnhknbaklih') return
    const data=event.data
    if(!data || typeof data!=='object') return
    if(data.type==='kodrik-tab-init' && typeof data.channel==='string' && data.channel.length<=100 && typeof data.enabled==='boolean') {
      if(origin && (origin!==event.origin || channel!==data.channel)) return
      origin=event.origin;channel=data.channel;enabled=data.enabled
      state={enabled,title:typeof data.tab?.title==='string'?data.tab.title.slice(0,1024):'Текущая вкладка',url:typeof data.tab?.url==='string'?data.tab.url.slice(0,4096):''}
      for(const listener of subscribers) listener(state)
      win.parent.postMessage({type:'kodrik-tab-ready',channel},origin)
      return
    }
    if(event.origin!==origin || data.channel!==channel || data.type!=='kodrik-tab-result') return
    const request=pending.get(data.id)
    if(!request) return
    clearTimeout(request.timer);pending.delete(data.id)
    if(typeof data.error==='string') request.reject(new Error(data.error.slice(0,2000)))
    else if(data.context===null) request.resolve(null)
    else if(validContext(data.context)) {
      try {
        if(data.document===undefined) request.resolve(data.context)
        else {
          const doc=data.document
          if(!doc || typeof doc.name!=='string' || !doc.name || doc.name.length>200 || /[\\/\x00-\x1f]/.test(doc.name) || !['application/pdf','text/markdown','application/vnd.openxmlformats-officedocument.spreadsheetml.sheet'].includes(doc.mime) || typeof doc.base64!=='string' || doc.base64.length>26666668) throw new Error('Некорректное вложение вкладки.')
          const binary=atob(doc.base64)
          if(!binary.length || binary.length>20000000 || doc.mime==='application/pdf' && !binary.startsWith('%PDF-') || doc.mime.includes('spreadsheet') && !binary.startsWith('PK')) throw new Error('Некорректное вложение вкладки.')
          request.resolve({...data.context,document:new File([Uint8Array.from(binary,c=>c.charCodeAt(0))],doc.name,{type:doc.mime})})
        }
      } catch {request.reject(new Error('Некорректное вложение вкладки. Повторите отправку.'))}
    } else request.reject(new Error('Некорректный контекст вкладки. Повторите отправку.'))
  }
  win.addEventListener('message',receive)
  if(framed) win.parent.postMessage({type:'kodrik-tab-connect'},'*')
  async function request(preview=false):Promise<BrowserCapture|null> {
    if(!framed) return null
    if(!origin) throw new Error('Расширение ещё не подключено к чату. Переподключите панель.')
    if(!preview && !enabled) return null
    const id=crypto.randomUUID()
    return new Promise((resolve,reject)=>{
      const timer=setTimeout(()=>{pending.delete(id);reject(new Error('Истекло время чтения вкладки. Повторите чтение вкладки.'))},timeout)
      pending.set(id,{resolve,reject,timer})
      win.parent.postMessage({type:preview?'kodrik-tab-preview':'kodrik-tab-capture',channel,id},origin)
    })
  }
  return {
    capture:()=>request(),
    preview:()=>request(true),
    settings(){if(origin) win.parent.postMessage({type:'kodrik-tab-settings',channel},origin)},
    toggle(value:boolean){if(origin) win.parent.postMessage({type:'kodrik-tab-toggle',channel,enabled:value},origin)},
    subscribe(listener:(state:{enabled:boolean;title:string;url:string})=>void){subscribers.add(listener);listener(state);return ()=>subscribers.delete(listener)},
    dispose() {
      win.removeEventListener('message',receive)
      for(const request of pending.values()) {clearTimeout(request.timer);request.reject(new Error('Чат закрыт.'))}
      pending.clear();subscribers.clear()
    },
  }
}
