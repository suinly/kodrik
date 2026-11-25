<script setup lang="ts">
import {base,messageOf} from '../utils/api'
import {settingsRequest} from '../utils/settings'
import {t} from '../utils/i18n'

type Profile = {
  id:string; name:string; instructions:string; model:string|null; skills:string[]; tools:string[]
  builtin?:boolean
}
type Catalog = {profiles:Profile[];drafts?:Profile[];skills:string[];tools:string[];models:string[]}
type Kind = 'new'|'saved'|'draft'|'builtin'

const emit=defineEmits<{dirty:[value:boolean]}>()
const path='chat/agent-profiles'
const controller=new AbortController()
const catalog=ref<Catalog|null>(null),editor=ref<Profile|null>(null),kind=ref<Kind>('new')
const busy=ref(false),error=ref(''),notice=ref(''),baseline=ref('')
const dirty=computed(()=>editor.value!==null && JSON.stringify(editor.value)!==baseline.value)
watch(dirty,value=>emit('dirty',value),{flush:'sync'})

const reserved=(id:string)=>id.toLowerCase().startsWith('builtin-')
const bundled=(profile:Pick<Profile,'builtin'|'id'>)=>profile.builtin===true || reserved(profile.id)
const builtinProfiles=computed(()=>catalog.value?.profiles.filter(bundled)??[])
const customProfiles=computed(()=>catalog.value?.profiles.filter(profile=>!bundled(profile))??[])
const groups=computed(()=>[
  {label:'Мои профили',profiles:customProfiles.value,kind:'saved' as const},
  {label:'Встроенные профили',profiles:builtinProfiles.value,kind:'builtin' as const},
  {label:'Черновики на проверку',profiles:catalog.value?.drafts??[],kind:'draft' as const},
])
const newIdError=computed(()=>{
  if(!editor.value || kind.value!=='new') return ''
  const id=editor.value.id.trim()
  if(!id) return ''
  if(reserved(id)) return 'Префикс builtin- зарезервирован для встроенных профилей.'
  if(!/^[a-z0-9][a-z0-9_-]{0,63}$/.test(id)) return 'ID: до 64 символов, строчные латинские буквы, цифры, дефисы и подчёркивания. Начните с буквы или цифры.'
  if(catalog.value?.profiles.some(profile=>profile.id===id) || catalog.value?.drafts?.some(profile=>profile.id===id)) return 'Этот ID уже используется.'
  return ''
})
const valid=computed(()=>{
  const form=editor.value
  return form && kind.value!=='builtin' && !bundled({...form,id:form.id.trim()}) &&
    !builtinProfiles.value.some(profile=>profile.id===form.id.trim()) && !!form.id.trim() && !newIdError.value && !!form.name.trim()
})
const modelChoices=computed(()=>{
  const choices=catalog.value?.models??[],model=editor.value?.model
  return model && !choices.includes(model)?[...choices,model]:choices
})
const capabilities=computed(()=>[
  {key:'skills' as const,label:'Навыки агента'},
  {key:'tools' as const,label:'Инструменты агента'},
].map(group=>({...group,options:[...new Set([...(catalog.value?.[group.key]??[]),...(editor.value?.[group.key]??[])])]})))

async function load() {
  if(busy.value) return
  busy.value=true;error.value=''
  try {catalog.value=await settingsRequest<Catalog>(base,path,'GET',undefined,controller.signal)}
  catch(reason) {if(!controller.signal.aborted) error.value=messageOf(reason)}
  finally {busy.value=false}
}
function setEditor(profile:Profile|null,next:Kind,changed=false) {
  const value=profile?{...profile,skills:[...profile.skills],tools:[...profile.tools]}:null
  baseline.value=changed?'':JSON.stringify(value)
  editor.value=value;kind.value=next
}
function open(profile:Profile|null,next:Kind,changed=false) {
  if(busy.value || dirty.value && !window.confirm(t('Отбросить несохранённые изменения профиля?'))) return
  error.value='';notice.value=''
  setEditor(profile,next,changed)
}
function create() {
  open({id:'',name:'',instructions:'',model:null,skills:[],tools:[]},'new')
}
function copy() {
  if(kind.value!=='builtin' || !editor.value) return
  const {builtin,...profile}=editor.value
  open({...profile,id:''},'new',true)
}
async function save() {
  if(busy.value || !catalog.value || !editor.value || !valid.value || !(dirty.value || kind.value==='draft')) return
  const {id,name,instructions,model,skills,tools}=editor.value
  const profileId=id.trim()
  const body={name:name.trim(),instructions,model,skills,tools}
  busy.value=true;error.value='';notice.value=''
  try {
    const saved=await settingsRequest<Profile|undefined>(base,`${path}/${encodeURIComponent(profileId)}`,'PUT',body,controller.signal) ?? {id:profileId,...body}
    if(controller.signal.aborted) return
    const index=catalog.value.profiles.findIndex(profile=>profile.id===saved.id)
    if(index<0) catalog.value.profiles.push(saved)
    else catalog.value.profiles[index]=saved
    catalog.value.drafts=catalog.value.drafts?.filter(profile=>profile.id!==saved.id)
    setEditor(saved,'saved');notice.value=t('Профиль сохранён.')
  } catch(reason) {if(!controller.signal.aborted) error.value=messageOf(reason)}
  finally {busy.value=false}
}
async function remove() {
  const profile=editor.value
  if(busy.value || !catalog.value || !profile || !['saved','draft'].includes(kind.value) || bundled(profile)) return
  if(!window.confirm(t('Удалить профиль «{0}»?',profile.name))) return
  busy.value=true;error.value='';notice.value=''
  try {
    await settingsRequest<void>(base,`${path}/${encodeURIComponent(profile.id)}`,'DELETE',undefined,controller.signal)
    if(controller.signal.aborted) return
    catalog.value.profiles=catalog.value.profiles.filter(item=>item.id!==profile.id)
    catalog.value.drafts=catalog.value.drafts?.filter(item=>item.id!==profile.id)
    setEditor(null,'new');notice.value=t('Профиль удалён.')
  } catch(reason) {if(!controller.signal.aborted) error.value=messageOf(reason)}
  finally {busy.value=false}
}
onMounted(load)
onBeforeUnmount(()=>controller.abort())
</script>

<template>
  <div class="space-y-4">
    <p v-if="error" role="alert" class="text-sm text-error">{{ error }}</p>
    <p v-if="notice" role="status" class="text-sm text-success">{{ notice }}</p>
    <template v-if="!catalog">
      <p v-if="busy" role="status" class="text-sm text-muted">{{ t('Загрузка профилей…') }}</p>
      <UButton v-else :label="t('Повторить загрузку')" color="neutral" variant="soft" @click="load" />
    </template>
    <div v-else class="grid items-start gap-6 md:grid-cols-[12rem_minmax(0,1fr)]">
      <div class="min-w-0 space-y-4">
        <UButton :label="t('Новый профиль агента')" icon="i-lucide-plus" color="neutral" variant="soft" :disabled="busy" @click="create" />
        <p v-if="!groups.some(group=>group.profiles.length)" class="text-sm text-muted">{{ t('Профилей пока нет.') }}</p>
        <div v-for="group in groups" :key="group.kind" class="space-y-1">
          <h3 v-if="group.profiles.length" class="text-sm font-semibold text-muted">{{ t(group.label) }}</h3>
          <button v-for="profile in group.profiles" :key="profile.id" type="button" :disabled="busy" :aria-pressed="kind===group.kind && editor?.id===profile.id" class="block w-full rounded-md px-3 py-2 text-left text-sm wrap-anywhere hover:bg-elevated aria-pressed:bg-primary/10 aria-pressed:text-primary" @click="open(profile,bundled(profile)?'builtin':group.kind)">
            <span class="block font-medium">{{ profile.name }}</span>
            <span class="block font-mono text-xs text-muted">{{ profile.id }}</span>
          </button>
        </div>
      </div>
      <form v-if="editor" class="min-w-0 space-y-4 rounded-lg border border-default p-4" @submit.prevent="save">
        <p v-if="kind==='builtin'" class="text-sm text-muted">{{ t('Этот профиль поставляется с Kodrik. Чтобы изменить его настройки, создайте копию.') }}</p>
        <p v-else-if="kind==='draft'" class="text-sm text-warning">{{ t('Проверьте инструкции и доступы черновика перед сохранением.') }}</p>
        <fieldset :disabled="busy || kind==='builtin'" class="min-w-0 space-y-4">
          <div>
            <label class="block space-y-1 text-sm">
              <span class="block font-medium">{{ t('ID профиля') }}</span>
              <UInput v-model="editor.id" class="w-full" :disabled="kind!=='new'" required :aria-invalid="!!newIdError" :aria-describedby="newIdError?'agent-profile-id-error':undefined" />
            </label>
            <p v-if="newIdError" id="agent-profile-id-error" class="mt-1 text-sm text-error">{{ t(newIdError) }}</p>
          </div>
          <label class="block space-y-1 text-sm">
            <span class="block font-medium">{{ t('Название профиля') }}</span>
            <UInput v-model="editor.name" class="w-full" required />
          </label>
          <label class="block space-y-1 text-sm">
            <span class="block font-medium">{{ t('Инструкции') }}</span>
            <UTextarea v-model="editor.instructions" :rows="6" class="w-full" />
          </label>
          <label class="block space-y-1 text-sm">
            <span id="agent-profile-model-label" class="block font-medium">{{ t('Роль модели') }}</span>
            <select v-model="editor.model" aria-labelledby="agent-profile-model-label" class="w-full rounded-md border border-default bg-default p-2">
              <option :value="null">{{ t('Модель по умолчанию') }}</option>
              <option v-for="choice in modelChoices" :key="choice" :value="choice">{{ choice }}</option>
            </select>
          </label>
          <div class="grid gap-4 sm:grid-cols-2">
            <fieldset v-for="group in capabilities" :key="group.key" class="min-w-0 rounded-md border border-default p-3">
              <legend class="px-1 text-sm font-medium">{{ t(group.label) }}</legend>
              <p v-if="!group.options.length" class="text-sm text-muted">{{ t('Нет доступных вариантов.') }}</p>
              <div v-else class="max-h-44 space-y-2 overflow-y-auto">
                <label v-for="option in group.options" :key="option" class="flex items-start gap-2 text-sm wrap-anywhere">
                  <input v-model="editor[group.key]" type="checkbox" :value="option" class="mt-1 shrink-0 accent-primary" />
                  <span>{{ option }}</span>
                </label>
              </div>
            </fieldset>
          </div>
        </fieldset>
        <div class="flex flex-wrap gap-2">
          <UButton v-if="kind==='builtin'" type="button" :label="t('Скопировать в новый профиль')" icon="i-lucide-copy" @click="copy" />
          <template v-else>
            <UButton type="submit" :label="t('Сохранить профиль')" :disabled="busy || !valid || !(dirty || kind==='draft')" />
            <UButton v-if="kind==='saved' || kind==='draft'" type="button" :label="t(kind==='draft'?'Удалить черновик':'Удалить профиль')" color="error" variant="soft" :disabled="busy" @click="remove" />
          </template>
          <UButton type="button" :label="t('Закрыть профиль')" color="neutral" variant="ghost" :disabled="busy" @click="open(null,'new')" />
        </div>
      </form>
      <p v-else class="text-sm text-muted">{{ t('Выберите профиль или создайте новый.') }}</p>
    </div>
  </div>
</template>
