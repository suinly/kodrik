<script setup lang="ts">
import {api,messageOf} from '../utils/api'
import {t} from '../utils/i18n'

const props=defineProps<{sessionId:string;model?:string|null;modelOverride?:string|null;disabled:boolean;refresh:()=>Promise<void>}>()
const emit=defineEmits<{saving:[value:boolean]}>()
const open=ref(false)
const models=ref<string[]|null>(null)
const defaultModel=ref<string|null>(null)
const search=ref('')
const loading=ref(false)
const saving=ref(false)
const problem=ref('')
const locked=computed(()=>props.disabled || saving.value)
const filtered=computed(()=>(models.value ?? []).filter(model=>model.toLowerCase().includes(search.value.trim().toLowerCase())))
const label=computed(()=>props.modelOverride ?? (props.model ? t('По умолчанию · {0}',props.model) : t('По умолчанию')))

async function load() {
  if(loading.value) return
  loading.value=true;problem.value=''
  try {
    const result=await api<{models:string[];default_model:string|null}>('models')
    models.value=result.models;defaultModel.value=result.default_model
  } catch(error) {problem.value=messageOf(error)}
  finally {loading.value=false}
}
function opened(value:boolean) {
  open.value=value
  if(value) {search.value='';if(!models.value) void load()}
}
async function select(model:string|null) {
  if(locked.value || loading.value || model===(props.modelOverride ?? null)) return
  saving.value=true;emit('saving',true);problem.value=''
  try {
    await api(`sessions/${props.sessionId}/model`,'POST',{model})
    await props.refresh()
    open.value=false
  } catch(error) {problem.value=messageOf(error)}
  finally {saving.value=false;emit('saving',false)}
}
</script>

<template>
  <div class="min-w-0">
    <UPopover :open="open" :content="{align:'start'}" @update:open="opened">
      <UButton :label="label" :title="label" :aria-label="t('Модель диалога')" icon="i-lucide-cpu" trailing-icon="i-lucide-chevron-down"
        color="neutral" variant="ghost" size="xs" class="max-w-full" :ui="{label:'truncate'}" :loading="saving" :disabled="locked" />
      <template #content>
        <div class="w-72 max-w-[calc(100vw-2rem)] space-y-2 p-2" :aria-busy="loading || saving">
          <UInput v-model="search" :aria-label="t('Поиск моделей')" :placeholder="t('Поиск моделей')" icon="i-lucide-search" class="w-full" />
          <p v-if="loading" role="status" class="px-2 text-sm text-muted">{{ t('Загружаю модели…') }}</p>
          <div v-if="problem" class="space-y-1 px-2">
            <p role="alert" class="text-sm text-error break-words">{{ t(problem) }}</p>
            <UButton v-if="models===null" :label="t('Повторить')" color="neutral" variant="link" size="xs" :disabled="locked || loading" @click="load" />
          </div>
            <UButton :label="defaultModel ? t('Использовать по умолчанию · {0}',defaultModel) : t('Использовать по умолчанию')" color="neutral" variant="ghost" size="sm"
              class="w-full justify-start" :disabled="locked || loading" :aria-pressed="!modelOverride" @click="select(null)" />
          <template v-if="models!==null">
            <div class="max-h-60 space-y-1 overflow-y-auto">
              <UButton v-for="model in filtered" :key="model" :label="model" :title="model" color="neutral" variant="ghost" size="sm"
                class="w-full justify-start" :ui="{label:'truncate'}" :disabled="locked || loading" :aria-pressed="modelOverride===model" @click="select(model)" />
              <p v-if="!filtered.length" role="status" class="px-2 text-sm text-muted">{{ t('Модели не найдены') }}</p>
            </div>
          </template>
        </div>
      </template>
    </UPopover>
  </div>
</template>
