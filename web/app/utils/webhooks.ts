import {computed, onMounted, reactive, ref, watch, type Ref} from 'vue'
import {base, checkedFetch, messageOf} from './api'
import {locale, t} from './i18n'

type Webhook = {id:string; name:string; instruction:string; enabled:boolean; url:string}
type Delivery = {conversation_id:string|null; accepted_at:string; status:string}

export function useWebhooks(active:Ref<boolean>) {
  const allowed=ref(false), loading=ref(false), saving=ref(false), editing=ref(false)
  const error=ref(''), token=ref(''), copied=ref(false)
  const hooks=ref<Webhook[]>([]), selected=ref<string|null>(null), deliveries=ref<Delivery[]>([])
  const confirmation=ref<'rotate'|'delete'|null>(null)
  const draft=reactive({name:'',instruction:'',enabled:true})
  const current=computed(()=>hooks.value.find(hook=>hook.id===selected.value))
  const dateFormat=computed(()=>new Intl.DateTimeFormat(locale.value,{dateStyle:'short',timeStyle:'short'}))
  const curl=computed(()=>current.value ? `curl -X POST '${current.value.url}' \\\n  -H 'Authorization: Bearer ${token.value || '<TOKEN>'}' \\\n  -H 'Content-Type: text/plain' \\\n  --data-binary @event.txt` : '')
  const statusLabels:Record<string,string>={Queued:'В очереди',Running:'Выполняется',Done:'Завершено',Cancelled:'Остановлено',Unanswered:'Ошибка обработки',Deleted:'Диалог удалён'}
  let selectionRevision=0
  let pendingLoad:Promise<void>|null=null

  async function request<T>(path='',method='GET',body?:unknown):Promise<T> {
    const response=await checkedFetch(new URL(`api/webhooks${path}`,base),{method,...(body===undefined?{}:{headers:{'Content-Type':'application/json'},body:JSON.stringify(body)})})
    return response.status===204 ? undefined as T : response.json()
  }
  function load() {
    if(pendingLoad) return pendingLoad
    pendingLoad=(async()=>{
      loading.value=true;error.value=''
      try {hooks.value=await request<Webhook[]>();allowed.value=true}
      catch(reason) {error.value=messageOf(reason)}
      finally {loading.value=false;pendingLoad=null}
    })()
    return pendingLoad
  }
  function clear() {
    selectionRevision++
    selected.value=null;editing.value=false;token.value='';copied.value=false;error.value='';confirmation.value=null;deliveries.value=[]
  }
  function choose(hook?:Webhook) {
    selectionRevision++
    selected.value=hook?.id ?? null;editing.value=true;token.value='';copied.value=false;error.value='';deliveries.value=[]
    Object.assign(draft,{name:hook?.name ?? '',instruction:hook?.instruction ?? '',enabled:hook?.enabled ?? true})
  }
  async function save() {
    if(saving.value) return
    const revision=selectionRevision, id=selected.value, values={...draft}
    saving.value=true;error.value=''
    try {
      if(id) {
        await request(`/${id}`,'PATCH',values)
        hooks.value=hooks.value.map(hook=>hook.id===id?{...hook,...values}:hook)
      } else {
        const {token:createdToken,...created}=await request<Webhook & {token:string}>('','POST',values)
        hooks.value.unshift(created)
        if(revision===selectionRevision) {choose(created);token.value=createdToken}
      }
    } catch(reason) {if(revision===selectionRevision) error.value=messageOf(reason)}
    finally {saving.value=false}
  }
  async function confirm() {
    if(saving.value || !selected.value) return
    const revision=selectionRevision, id=selected.value, action=confirmation.value
    saving.value=true;error.value=''
    try {
      if(action==='rotate') {
        const rotated=await request<{token:string}>(`/${id}/token`,'POST')
        if(revision===selectionRevision) {token.value=rotated.token;copied.value=false}
      } else {
        await request(`/${id}`,'DELETE')
        hooks.value=hooks.value.filter(hook=>hook.id!==id)
        if(revision===selectionRevision) clear()
      }
      if(revision===selectionRevision) confirmation.value=null
    } catch(reason) {if(revision===selectionRevision) {error.value=messageOf(reason);confirmation.value=null}}
    finally {saving.value=false}
  }
  async function copyToken() {
    try {await navigator.clipboard.writeText(token.value);copied.value=true}
    catch {error.value=t('Не удалось скопировать. Выделите и скопируйте токен вручную.')}
  }
  watch(active,visible=>{
    token.value='';copied.value=false;confirmation.value=null
    if(!visible) selectionRevision++
  })
  watch([active,selected],([visible,id],_,onCleanup)=>{
    if(!visible || !id) return
    let disposed=false, pending=false
    async function refresh() {
      if(pending) return
      pending=true
      try {const found=await request<Delivery[]>(`/${id}/events`);if(!disposed) deliveries.value=found}
      catch(reason) {if(!disposed) error.value=messageOf(reason)}
      finally {pending=false}
    }
    void refresh()
    const timer=setInterval(()=>{void refresh()},3000)
    onCleanup(()=>{disposed=true;clearInterval(timer)})
  })
  onMounted(load)
  return reactive({allowed,loading,saving,editing,error,token,copied,hooks,selected,deliveries,confirmation,draft,current,dateFormat,curl,statusLabels,load,clear,choose,save,confirm,copyToken})
}

export type WebhooksState = ReturnType<typeof useWebhooks>
