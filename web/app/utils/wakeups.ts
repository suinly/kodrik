import {computed, onBeforeUnmount, reactive, ref, watch, type Ref} from 'vue'
import {base, checkedFetch, messageOf, type Snapshot} from './api'
import {locale, t} from './i18n'

export type Wakeup = {id:string;instruction:string;due_at:string;status:string;retry_at?:string|null;last_error?:string|null;source_session?:string|null;message?:string|null}
export type WakeupEvents = Pick<Snapshot,'messages'|'busy'> & Partial<Pick<Snapshot,'blocked_reason'|'phase'|'live'|'plan'|'environment'>>
const statusLabels:Record<string,string>={pending:'Ожидает',running:'Выполняется',ready:'Готово к отправке',completed:'Завершено',sending:'Отправляется',sent:'Отправлено',cancelled:'Остановлено',uncertain:'Результат неизвестен',failed:'Ошибка обработки'}
export function wakeupStatus(status:string) {return t(statusLabels[status] ?? status)}
export function useWakeups(active:Ref<boolean>) {
  const items=ref<Wakeup[]>([]), selected=ref<string|null>(null), loading=ref(false), error=ref(''), eventsError=ref(''), eventsLoading=ref(false)
  const events=ref<WakeupEvents|null>(null)
  const current=computed(()=>items.value.find(item=>item.id===selected.value))
  const dateFormat=computed(()=>new Intl.DateTimeFormat(locale.value,{dateStyle:'short',timeStyle:'short'}))
  let disposed=false, generation=0, pending:Promise<void>|null=null
  let controller:AbortController|null=null
  function load() {
    if(disposed || !active.value) return Promise.resolve()
    if(pending) return pending
    const revision=generation, requestController=new AbortController()
    controller=requestController;loading.value=true
    const request=(async()=>{
      try {
        const response=await checkedFetch(new URL('api/wakeups',base),{signal:requestController.signal})
        const found:Wakeup[]=await response.json()
        if(!disposed && revision===generation) {items.value=found;error.value=''}
      } catch(reason) {if(!disposed && revision===generation) error.value=messageOf(reason)}
      finally {if(revision===generation) {loading.value=false;pending=null;controller=null}}
    })()
    pending=request
    return request
  }
  function choose(id:string|null) {selected.value=id}
  watch(active,(visible,_,onCleanup)=>{
    if(!visible) return
    void load()
    const timer=setInterval(()=>{void load()},3000)
    onCleanup(()=>{generation++;controller?.abort();controller=null;pending=null;loading.value=false;clearInterval(timer)})
  },{immediate:true,flush:'sync'})
  watch([active,selected],([visible,id],_,onCleanup)=>{
    events.value=null;eventsError.value='';eventsLoading.value=false
    if(!visible || !id) return
    let stale=false, busy=false
    const controller=new AbortController()
    async function refresh() {
      if(busy) return
      busy=true;eventsLoading.value=events.value===null
      try {
        const response=await checkedFetch(new URL(`api/wakeups/${encodeURIComponent(id!)}/events`,base),{signal:controller.signal})
        const found:WakeupEvents=await response.json()
        if(!stale) {events.value=found;eventsError.value=''}
      } catch(reason) {if(!stale) eventsError.value=messageOf(reason)}
      finally {busy=false;if(!stale) eventsLoading.value=false}
    }
    void refresh()
    const timer=setInterval(()=>{void refresh()},3000)
    onCleanup(()=>{stale=true;controller.abort();clearInterval(timer)})
  },{immediate:true,flush:'sync'})
  onBeforeUnmount(()=>{disposed=true;generation++;controller?.abort()})
  return reactive({items,selected,current,loading,error,events,eventsError,eventsLoading,dateFormat,load,choose})
}
export type WakeupsState = ReturnType<typeof useWakeups>
