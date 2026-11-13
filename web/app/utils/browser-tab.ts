// The extension never submits messages: it can only supply page data to an explicit Send.
export function createTabBridge(win:Window, timeout=15000) {
  let origin='',channel='',enabled=true
  const pending=new Map<string,{resolve:(file:File|null)=>void;reject:(error:Error)=>void;timer:ReturnType<typeof setTimeout>}>()
  const framed=win.parent!==win && new URLSearchParams(win.location.search).get('browser_sidebar')==='1'
  function receive(event:MessageEvent) {
    if(!framed || event.source!==win.parent || event.origin!=='chrome-extension://iiajbdommloibgimhfmoojnhknbaklih') return
    const data=event.data
    if(!data || typeof data!=='object') return
    if(data.type==='kodrik-tab-init' && typeof data.channel==='string' && data.channel.length<=100 && typeof data.enabled==='boolean') {
      if(origin && (origin!==event.origin || channel!==data.channel)) return
      origin=event.origin;channel=data.channel;enabled=data.enabled
      win.parent.postMessage({type:'kodrik-tab-ready',channel},origin)
      return
    }
    if(event.origin!==origin || data.channel!==channel || data.type!=='kodrik-tab-result') return
    const request=pending.get(data.id)
    if(!request) return
    clearTimeout(request.timer);pending.delete(data.id)
    if(typeof data.error==='string') request.reject(new Error(data.error.slice(0,2000)))
    else if(data.context===null) request.resolve(null)
    else if(typeof data.context==='string' && data.context.length>0 && new TextEncoder().encode(data.context).length<=100000) {
      request.resolve(new File([data.context],'browser-tab.txt',{type:'text/plain'}))
    } else request.reject(new Error('Некорректный контекст вкладки. Повторите отправку.'))
  }
  win.addEventListener('message',receive)
  if(framed) win.parent.postMessage({type:'kodrik-tab-connect'},'*')
  return {
    async capture():Promise<File|null> {
      if(!framed) return null
      if(!origin) throw new Error('Расширение ещё не подключено к чату. Переподключите панель.')
      if(!enabled) return null
      const id=crypto.randomUUID()
      return new Promise((resolve,reject)=>{
        const timer=setTimeout(()=>{pending.delete(id);reject(new Error('Истекло время чтения вкладки. Повторите отправку или отключите контекст.'))},timeout)
        pending.set(id,{resolve,reject,timer})
        win.parent.postMessage({type:'kodrik-tab-capture',channel,id},origin)
      })
    },
    dispose() {
      win.removeEventListener('message',receive)
      for(const request of pending.values()) {clearTimeout(request.timer);request.reject(new Error('Чат закрыт.'))}
      pending.clear()
    },
  }
}
