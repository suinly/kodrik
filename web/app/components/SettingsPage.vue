<script setup lang="ts">
import {base, messageOf} from '../utils/api'
import {settingsRequest, restartGateway, type ConfigDocument, type GatewayStatus} from '../utils/settings'
import {t} from '../utils/i18n'

const props=defineProps<{owner:boolean}>()
const emit=defineEmits<{dirty:[value:boolean]}>()
const document=ref<ConfigDocument|null>(null),text=ref(''),revealed=ref(false)
const loading=ref(false),saving=ref(false),restarting=ref(false),confirmRestart=ref(false)
const gateway=ref<GatewayStatus|null>(null)
const error=ref(''),gatewayError=ref(''),notice=ref(''),restartNotice=ref('')
const dirty=computed(()=>document.value!==null && text.value!==document.value.config)
const controller=new AbortController()
watch(dirty,value=>emit('dirty',value),{flush:'sync'})

async function load() {
  if(!props.owner || loading.value || saving.value || restarting.value) return
  if(dirty.value && !window.confirm(t('Отбросить несохранённые изменения конфигурации?'))) return
  loading.value=true;error.value='';notice.value=''
  try {
    document.value=await settingsRequest<ConfigDocument>(base,'config','GET',undefined,controller.signal)
    text.value=document.value.config;revealed.value=true
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
    document.value=await settingsRequest<ConfigDocument>(base,'config','PUT',{config:text.value,revision:document.value.revision},controller.signal)
    text.value=document.value.config;notice.value=t('Конфигурация сохранена. Изменения вступят в силу после перезапуска шлюза.')
  } catch(reason) {if(!controller.signal.aborted) error.value=messageOf(reason)}
  finally {saving.value=false}
}
async function restart() {
  if(!confirmRestart.value || dirty.value || saving.value || restarting.value || !gateway.value?.restart_available) return
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
  if(dirty.value) {event.preventDefault();event.returnValue=''}
}
watch(()=>props.owner,()=>{void loadGateway()},{immediate:true})
onMounted(()=>{window.addEventListener('beforeunload',beforeUnload)})
onBeforeUnmount(()=>{controller.abort();window.removeEventListener('beforeunload',beforeUnload);emit('dirty',false)})
</script>

<template>
  <section :aria-label="t('Настройки')" class="mx-auto w-full max-w-4xl space-y-8 p-4 sm:p-8">
    <p v-if="!owner" role="alert" class="text-sm text-error">{{ t('Настройки доступны только владельцу.') }}</p>
    <template v-else>
      <section class="space-y-4" :aria-label="t('Конфигурация шлюза')">
        <h2 class="text-lg font-semibold text-highlighted">{{ t('Конфигурация шлюза') }}</h2>
        <p class="text-sm text-muted">{{ t('Полная конфигурация редактируется как исходный TOML, а не набор отдельных полей. Все ключи и комментарии сохраняются. Изменения применяются после перезапуска.') }}</p>
        <p class="text-sm text-warning">{{ t('После раскрытия будут видны все ключи, включая пароли и токены. Не показывайте экран посторонним. Конфигурация не сохраняется в хранилище браузера.') }}</p>
        <div class="flex flex-wrap gap-2">
          <UButton v-if="!revealed" :label="t('Показать полную конфигурацию')" icon="i-lucide-eye" color="neutral" variant="soft" :loading="loading" :disabled="restarting" @click="document?revealed=true:load()" />
          <UButton v-else :label="t('Скрыть конфигурацию')" icon="i-lucide-eye-off" color="neutral" variant="soft" @click="revealed=false" />
          <UButton v-if="document" :label="t('Загрузить с сервера')" color="neutral" variant="ghost" :loading="loading" :disabled="saving || restarting" @click="load" />
        </div>
        <form v-if="revealed && document" class="space-y-3" @submit.prevent="save">
          <label for="settings-toml" class="block text-sm font-medium">{{ t('Полная конфигурация TOML') }}</label>
          <textarea id="settings-toml" v-model="text" :disabled="saving || restarting || loading" :spellcheck="false" autocomplete="off" autocorrect="off" autocapitalize="off" wrap="off" class="block min-h-80 w-full resize-y rounded-md border border-default bg-default p-3 font-mono text-sm focus:outline-2 focus:outline-primary" />
          <div class="flex flex-wrap items-center gap-3">
            <UButton type="submit" :label="t('Сохранить конфигурацию')" :loading="saving" :disabled="!dirty || restarting || loading" />
            <span v-if="dirty" class="text-sm text-warning">{{ t('Есть несохранённые изменения.') }}</span>
          </div>
        </form>
        <p v-if="error" role="alert" class="text-sm text-error">{{ error }}</p>
        <p v-if="notice" role="status" class="text-sm text-success">{{ notice }}</p>
      </section>
      <USeparator />
      <section :aria-label="t('Перезапуск шлюза')" class="space-y-4">
        <h2 class="text-lg font-semibold text-highlighted">{{ t('Перезапуск шлюза') }}</h2>
        <p v-if="gateway" class="text-sm text-muted">{{ t('Процесс: {0}',gateway.pid) }}</p>
        <p class="text-sm text-muted">{{ t('Активные сессии будут приостановлены. Изменение владельца или веб-настроек может лишить вас доступа к этому интерфейсу.') }}</p>
        <p v-if="gateway && !gateway.restart_available" class="text-sm text-warning">{{ t('Перезапуск недоступен для этого способа запуска. Перезапустите шлюз вручную.') }}</p>
        <p v-if="dirty" class="text-sm text-warning">{{ t('Сохраните или отбросьте изменения перед перезапуском.') }}</p>
        <UButton :label="t('Перезапустить шлюз')" icon="i-lucide-refresh-cw" color="warning" variant="soft" :loading="restarting" :disabled="!gateway?.restart_available || dirty || saving || loading" @click="confirmRestart=true" />
        <p v-if="restarting" role="status" class="text-sm text-muted">{{ t('Ожидаю новый экземпляр шлюза, до 60 секунд…') }}</p>
        <p v-if="restartNotice" role="status" class="text-sm text-muted">{{ restartNotice }}</p>
        <p v-if="gatewayError" role="alert" class="text-sm text-error">{{ gatewayError }}</p>
        <UButton v-if="gatewayError" :label="t('Обновить состояние')" color="neutral" variant="ghost" :disabled="restarting" @click="loadGateway" />
      </section>
      <UModal v-model:open="confirmRestart" :title="t('Перезапустить шлюз?')">
        <template #body><p class="text-sm text-muted">{{ t('Активные сессии будут приостановлены. Изменение владельца или веб-настроек может лишить вас доступа к этому интерфейсу.') }}</p></template>
        <template #footer>
          <UButton :label="t('Отмена')" color="neutral" variant="ghost" @click="confirmRestart=false" />
          <UButton :label="t('Подтвердить перезапуск')" color="warning" :disabled="dirty || saving" @click="restart" />
        </template>
      </UModal>
    </template>
  </section>
</template>
