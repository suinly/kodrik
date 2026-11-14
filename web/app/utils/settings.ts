import {locale, t} from './i18n.ts'

export type ConfigDocument = {config:string;revision:string}
export type GatewayStatus = {instance_id:string;pid:number;restart_available:boolean}

// Config errors deliberately do not use the chat-specific checkedFetch messages.
export async function settingsRequest<T>(base:URL,path:string,method='GET',body?:unknown,signal?:AbortSignal):Promise<T> {
  const response=await fetch(new URL(`api/${path}`,base),{
    method,cache:'no-store',signal,headers:{'Accept-Language':locale.value,...(body===undefined?{}:{'Content-Type':'application/json'})},
    ...(body===undefined?{}:{body:JSON.stringify(body)}),
  })
  if(!response.ok) {
    if(response.status===409 && path==='gateway/restart') return undefined as T
    if(response.status===409 && path==='config') throw new Error(t('Конфигурация изменилась на сервере. Скопируйте свои правки, затем загрузите актуальную версию и объедините изменения.'))
    if(response.status===400 || response.status===422) {
      const detail=await response.json().catch(()=>null)
      // The server sanitizes validation errors; render only as text, never HTML.
      throw new Error(typeof detail?.error==='string'?detail.error:t('Некорректная конфигурация.'))
    }
    if(response.status===401) throw new Error(t('Сессия истекла. Войдите снова.'))
    if(response.status===403) throw new Error(t('Настройки доступны только владельцу.'))
    throw new Error(t('Не удалось выполнить запрос настроек ({0}).',response.status))
  }
  if(response.status===204 || response.status===202 && path!=='gateway/restart') return undefined as T
  return response.json()
}

// One POST only: a lost response may mean the restart was already queued.
export async function restartGateway(
  instanceId:string,
  request:(method:'GET'|'POST',signal:AbortSignal)=>Promise<Pick<GatewayStatus,'instance_id'>|void>,
  {timeoutMs=60000,intervalMs=1000,signal}:{timeoutMs?:number;intervalMs?:number;signal?:AbortSignal}={},
):Promise<'confirmed'|'uncertain'> {
  const controller=new AbortController()
  let timer:ReturnType<typeof setTimeout>|undefined
  const cancel=()=>controller.abort()
  const deadline=new Promise<'uncertain'>(resolve=>{
    controller.signal.addEventListener('abort',()=>resolve('uncertain'),{once:true})
    timer=setTimeout(cancel,timeoutMs)
  })
  signal?.addEventListener('abort',cancel,{once:true})
  if(signal?.aborted) controller.abort()
  const track=async():Promise<'confirmed'|'uncertain'>=>{
    if(controller.signal.aborted) return 'uncertain'
    const current=await request('GET',controller.signal)
    if(!current || controller.signal.aborted) return 'uncertain'
    instanceId=current.instance_id
    try {
      const accepted=await request('POST',controller.signal)
      if(accepted) instanceId=accepted.instance_id
    }
    catch(error) {if(!(error instanceof TypeError) && !controller.signal.aborted) throw error}
    while(!controller.signal.aborted) {
      try {
        const status=await request('GET',controller.signal)
        if(status && status.instance_id!==instanceId) return 'confirmed'
      } catch { /* The old process can disappear before the new one starts. */ }
      if(!controller.signal.aborted) await new Promise(resolve=>setTimeout(resolve,intervalMs))
    }
    return 'uncertain'
  }
  try {return await Promise.race([deadline,track()])}
  finally {clearTimeout(timer);signal?.removeEventListener('abort',cancel);controller.abort()}
}
