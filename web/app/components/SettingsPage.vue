<script setup lang="ts">
import {base, messageOf} from '../utils/api'
import {settingsRequest, restartGateway, type ConfigDocument, type GatewayStatus} from '../utils/settings'
import {t} from '../utils/i18n'
import AgentProfilesSettings from './AgentProfilesSettings.vue'
import ConfigFieldEditor from './ConfigFieldEditor.vue'
import {configSchema,type ConfigValues} from '../utils/configForm'

const props=defineProps<{owner:boolean}>()
const emit=defineEmits<{dirty:[value:boolean]}>()
const document=ref<ConfigDocument|null>(null),values=ref<ConfigValues>({}),revealed=ref(false)
const loading=ref(false),saving=ref(false),restarting=ref(false),confirmRestart=ref(false)
const gateway=ref<GatewayStatus|null>(null)
const error=ref(''),gatewayError=ref(''),notice=ref(''),restartNotice=ref('')
const dirty=computed(()=>document.value!==null && JSON.stringify(values.value)!==JSON.stringify(document.value.values))
const profilesOpened=ref(false),profilesDirty=ref(false)
const settingsDirty=computed(()=>dirty.value || profilesDirty.value)
const selected=ref('provider'),showKeys=ref(false)
const sections=[
  {id:'provider',label:'Модели',description:'Профили моделей и подключения к провайдерам ИИ.',fields:['provider','model']},
  {id:'gateway',label:'Telegram',description:'Боты Telegram и владелец шлюза.',fields:['gateway','owner']},
  {id:'behavior',label:'Поведение',description:'Общение, инициативные сообщения и работа с контекстом.',fields:['companion','sessions','system_prompt']},
  {id:'research',label:'Поиск',description:'Поисковый сервис и чтение веб-страниц.',fields:['research']},
  {id:'web',label:'Веб-сервер',description:'Адрес сервера и подключения веб-чата.',fields:['web']},
  {id:'mcp',label:'Интеграции',description:'Внешние инструменты через серверы MCP.',fields:['mcp']},
]
const currentSection=computed(()=>sections.find(section=>section.id===selected.value)!)
const controller=new AbortController()
watch(settingsDirty,value=>emit('dirty',value),{flush:'sync'})

async function load() {
  if(!props.owner || loading.value || saving.value || restarting.value) return
  if(dirty.value && !window.confirm(t('Отбросить несохранённые изменения конфигурации?'))) return
  loading.value=true;error.value='';notice.value=''
  try {
    document.value=await settingsRequest<ConfigDocument>(base,'config/form','GET',undefined,controller.signal)
    values.value=JSON.parse(JSON.stringify(document.value.values));revealed.value=true
  } catch(reason) {if(!controller.signal.aborted) error.value=messageOf(reason)}
  finally {loading.value=false}
}
async function loadGateway() {
  if(!props.owner) return
  gatewayError.value=''
  try {gateway.value=await settingsRequest<GatewayStatus>(base,'gateway','GET',undefined,controller.signal)}
  catch(reason) {if(!controller.signal.aborted) gatewayError.value=messageOf(reason)}
}
async function save() {
  if(!dirty.value || !document.value || saving.value || restarting.value) return
  saving.value=true;error.value='';notice.value=''
  try {
    document.value=await settingsRequest<ConfigDocument>(base,'config/form','PUT',{values:values.value,revision:document.value.revision},controller.signal)
    values.value=JSON.parse(JSON.stringify(document.value.values));notice.value=t('Конфигурация сохранена. Изменения вступят в силу после перезапуска шлюза.')
  } catch(reason) {if(!controller.signal.aborted) error.value=messageOf(reason)}
  finally {saving.value=false}
}
async function restart() {
  if(!confirmRestart.value || settingsDirty.value || saving.value || restarting.value || !gateway.value?.restart_available) return
  confirmRestart.value=false;restarting.value=true;gatewayError.value='';restartNotice.value=''
  try {
    const result=await restartGateway(gateway.value.instance_id,(method,signal)=>settingsRequest<GatewayStatus>(base,method==='POST'?'gateway/restart':'gateway',method,undefined,signal),{signal:controller.signal})
    if(controller.signal.aborted) return
    restartNotice.value=t(result==='confirmed'?'Перезапуск подтверждён: запущен новый экземпляр шлюза.':'Не удалось подтвердить перезапуск за 60 секунд. Запрос не повторялся. Проверьте процесс и доступ к серверу перед новой попыткой.')
    await loadGateway()
  } catch(reason) {if(!controller.signal.aborted) gatewayError.value=messageOf(reason)}
  finally {restarting.value=false}
}
function beforeUnload(event:BeforeUnloadEvent) {
  if(settingsDirty.value) {event.preventDefault();event.returnValue=''}
}
watch(()=>props.owner,()=>{void loadGateway()},{immediate:true})
onMounted(()=>{window.addEventListener('beforeunload',beforeUnload)})
onBeforeUnmount(()=>{controller.abort();window.removeEventListener('beforeunload',beforeUnload);emit('dirty',false)})
</script>

<template>
  <section :aria-label="t('Настройки')" class="mx-auto w-full max-w-6xl space-y-8 p-4 sm:p-8">
    <p v-if="!owner" role="alert" class="text-sm text-error">{{ t('Настройки доступны только владельцу.') }}</p>
    <template v-else>
      <section class="space-y-3" :aria-label="t('Профили агентов')">
        <h2 class="text-lg font-semibold text-highlighted">{{ t('Профили агентов') }}</h2>
        <p class="text-sm text-muted">{{ t('Настройте инструкции, модель, навыки и инструменты для лёгких агентов.') }}</p>
        <details @toggle="profilesOpened ||= ($event.currentTarget as HTMLDetailsElement).open">
          <summary class="cursor-pointer text-sm font-medium text-primary">{{ t('Открыть каталог профилей') }}</summary>
          <AgentProfilesSettings v-if="profilesOpened" class="mt-4" @dirty="profilesDirty=$event" />
        </details>
      </section>
      <USeparator />
      <section class="space-y-4" :aria-label="t('Конфигурация шлюза')">
        <h2 class="text-lg font-semibold text-highlighted">{{ t('Конфигурация шлюза') }}</h2>
        <p class="text-sm text-muted">{{ t('Все настройки доступны в форме, включая необязательные поля. Значения по умолчанию загружаются с сервера. Изменения применяются после перезапуска.') }}</p>
        <details class="text-sm text-muted">
          <summary class="cursor-pointer">{{ t('Хранение и безопасность') }}</summary>
          <div class="mt-3 space-y-2">
            <p>{{ t('Конфигурация загружается только по запросу. Пароли, токены и заголовки скрыты. Значения не сохраняются в хранилище браузера.') }}</p>
            <p>{{ t('При сохранении TOML переформатируется, комментарии не сохраняются. Исходник остаётся в config.toml.bak.') }}</p>
          </div>
        </details>
        <div class="flex flex-wrap gap-2">
          <UButton v-if="!revealed" :label="t('Показать полную конфигурацию')" icon="i-lucide-eye" color="neutral" variant="soft" :loading="loading" :disabled="restarting" @click="document?revealed=true:load()" />
          <UButton v-else :label="t('Скрыть конфигурацию')" icon="i-lucide-eye-off" color="neutral" variant="soft" @click="revealed=false" />
          <UButton v-if="document" :label="t('Загрузить с сервера')" color="neutral" variant="ghost" :loading="loading" :disabled="saving || restarting" @click="load" />
        </div>
        <UCheckbox v-if="revealed" v-model="showKeys" :label="t('Показывать ключи конфигурации')" />
        <form v-if="revealed && document" class="space-y-6" novalidate @submit.prevent="save">
          <div class="grid items-start gap-6 md:grid-cols-[12rem_minmax(0,1fr)]">
            <nav :aria-label="t('Разделы настроек')" class="space-y-1 md:sticky md:top-4">
              <label class="block md:hidden">
                <span class="mb-2 block text-sm text-muted">{{ t('Разделы настроек') }}</span>
                <select v-model="selected" class="w-full rounded-md border border-default bg-default p-3">
                  <option v-for="section in sections" :key="section.id" :value="section.id">{{ t(section.label) }}</option>
                </select>
              </label>
              <button v-for="section in sections" :key="section.id" type="button" :aria-current="selected===section.id?'page':undefined" class="hidden w-full rounded-md px-3 py-2.5 text-left text-sm transition-colors md:block" :class="selected===section.id?'bg-primary/10 font-semibold text-primary':'text-muted hover:bg-elevated hover:text-highlighted'" @click="selected=section.id">{{ t(section.label) }}</button>
            </nav>
            <section class="min-w-0 space-y-6" :aria-label="t(currentSection.label)">
              <header class="space-y-2">
                <h3 class="text-xl font-semibold text-highlighted">{{ t(currentSection.label) }}</h3>
                <p class="text-sm leading-relaxed text-muted">{{ t(currentSection.description) }}</p>
              </header>
              <ConfigFieldEditor v-for="key in currentSection.fields" :key="key" :model-value="values[key]" :schema="configSchema.fields![key]!" :path="key" :show-keys="showKeys" :disabled="saving || restarting || loading" @update:model-value="values[key]=$event" />
            </section>
          </div>
          <footer class="sticky bottom-0 z-10 space-y-3 rounded-lg border border-default bg-default/95 p-4 shadow-sm backdrop-blur">
            <div class="flex flex-wrap items-center justify-between gap-3">
              <span class="text-sm" :class="dirty?'text-warning':'text-muted'">{{ t(dirty?'Есть несохранённые изменения.':'Нет несохранённых изменений.') }}</span>
              <UButton type="submit" :label="t('Сохранить конфигурацию')" :loading="saving" :disabled="!dirty || restarting || loading" />
            </div>
            <p v-if="error" role="alert" class="text-sm text-error">{{ error }}</p>
            <p v-if="notice" role="status" class="text-sm text-success">{{ notice }}</p>
          </footer>
        </form>
        <p v-if="error && !revealed" role="alert" class="text-sm text-error">{{ error }}</p>
      </section>
      <USeparator />
      <section :aria-label="t('Перезапуск шлюза')" class="space-y-4">
        <h2 class="text-lg font-semibold text-highlighted">{{ t('Перезапуск шлюза') }}</h2>
        <p v-if="gateway" class="text-sm text-muted">{{ t('Процесс: {0}',gateway.pid) }}</p>
        <p class="text-sm text-muted">{{ t('Активные сессии будут приостановлены. Изменение владельца или веб-настроек может лишить вас доступа к этому интерфейсу.') }}</p>
        <p v-if="gateway && !gateway.restart_available" class="text-sm text-warning">{{ t('Перезапуск недоступен для этого способа запуска. Перезапустите шлюз вручную.') }}</p>
        <p v-if="settingsDirty" class="text-sm text-warning">{{ t('Сохраните или отбросьте изменения перед перезапуском.') }}</p>
        <UButton :label="t('Перезапустить шлюз')" icon="i-lucide-refresh-cw" color="warning" variant="soft" :loading="restarting" :disabled="!gateway?.restart_available || settingsDirty || saving || loading" @click="confirmRestart=true" />
        <p v-if="restarting" role="status" class="text-sm text-muted">{{ t('Ожидаю новый экземпляр шлюза, до 60 секунд…') }}</p>
        <p v-if="restartNotice" role="status" class="text-sm text-muted">{{ restartNotice }}</p>
        <p v-if="gatewayError" role="alert" class="text-sm text-error">{{ gatewayError }}</p>
        <UButton v-if="gatewayError" :label="t('Обновить состояние')" color="neutral" variant="ghost" :disabled="restarting" @click="loadGateway" />
      </section>
      <UModal v-model:open="confirmRestart" :title="t('Перезапустить шлюз?')">
        <template #body><p class="text-sm text-muted">{{ t('Активные сессии будут приостановлены. Изменение владельца или веб-настроек может лишить вас доступа к этому интерфейсу.') }}</p></template>
        <template #footer>
          <UButton :label="t('Отмена')" color="neutral" variant="ghost" @click="confirmRestart=false" />
          <UButton :label="t('Подтвердить перезапуск')" color="warning" :disabled="settingsDirty || saving" @click="restart" />
        </template>
      </UModal>
    </template>
  </section>
</template>
