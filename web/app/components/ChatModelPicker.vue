<script lang="ts">
import {api,messageOf} from '../utils/api'

type ModelCatalog = {models:string[];default_model:string|null;model_names?:Record<string,string>}
const roles = {fast:'Быстрая',smart:'Умная',deep:'Глубокая',vision:'Зрение'} as const
type ModelRole = keyof typeof roles
const catalogTtlMs = 5 * 60_000
let catalog: {data:ModelCatalog;expiresAt:number}|undefined
let pendingCatalog:Promise<ModelCatalog>|undefined

function loadCatalog() {
  if (catalog && catalog.expiresAt > Date.now()) return Promise.resolve(catalog.data)
  return pendingCatalog ??= api<ModelCatalog>('models').then(data=>{
    catalog={data,expiresAt:Date.now()+catalogTtlMs}
    return data
  }).finally(()=>{pendingCatalog=undefined})
}
</script>

<script setup lang="ts">
import {t} from '../utils/i18n'

const props=defineProps<{sessionId?:string|null;model?:string|null;modelOverride?:string|null;disabled:boolean;refresh:()=>Promise<void>}>()
const emit=defineEmits<{saving:[value:boolean];'update:modelOverride':[value:string|null]}>()
const open=ref(false)
const models=ref<string[]|null>(null)
const modelNames=ref<ModelCatalog['model_names']>()
const defaultModel=ref<string|null>(null)
const loading=ref(false)
const saving=ref(false)
const problem=ref('')
const locked=computed(()=>props.disabled || saving.value)
const availableRoles=computed(()=>(Object.keys(roles) as ModelRole[]).filter(role=>models.value?.includes(role)))
const currentModel=computed(()=>props.model ?? defaultModel.value)
const label=computed(()=>props.modelOverride ? modelName(props.modelOverride) : t('Авто'))
const title=computed(()=>!props.modelOverride && currentModel.value ? t('Авто · {0}', modelName(currentModel.value)) : label.value)

function modelName(model:string) {
  return Object.hasOwn(roles,model) ? t(roles[model as ModelRole]) : model.replace(/^openai:/,'')
}
onMounted(()=>{if(!props.modelOverride && !currentModel.value) void load()})

async function load() {
  if(loading.value) return
  loading.value=true;problem.value=''
  try {
    const result=await loadCatalog()
    models.value=result.models;defaultModel.value=result.default_model
    modelNames.value=result.model_names
  } catch(error) {problem.value=messageOf(error)}
  finally {loading.value=false}
}
function opened(value:boolean) {
  open.value=value
  if(value && !models.value) void load()
}
async function select(model:ModelRole|null) {
  if(locked.value || loading.value || model===(props.modelOverride ?? null) || model!==null && !availableRoles.value.includes(model)) return
  if(!props.sessionId) {
    emit('update:modelOverride',model)
    open.value=false
    return
  }
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
      <UButton :label="label" :title="title" :aria-label="t('Модель диалога')" icon="i-lucide-cpu" trailing-icon="i-lucide-chevron-down"
        color="neutral" variant="ghost" size="xs" class="max-w-full" :ui="{label:'truncate'}" :loading="saving" :disabled="locked" />
      <template #content>
        <div class="w-52 max-w-[calc(100vw-2rem)] space-y-1 p-2" :aria-busy="loading || saving">
          <p v-if="loading" role="status" class="px-2 text-sm text-muted">{{ t('Загружаю модели…') }}</p>
          <div v-if="problem" class="space-y-1 px-2">
            <p role="alert" class="text-sm text-error break-words">{{ t(problem) }}</p>
            <UButton v-if="models===null" :label="t('Повторить')" color="neutral" variant="link" size="xs" :disabled="locked || loading" @click="load" />
          </div>
          <UButton :label="t('Авто')" color="neutral" variant="ghost" size="sm"
            class="w-full justify-start" :disabled="locked || loading" :aria-pressed="!modelOverride" @click="select(null)" />
          <UButton v-for="role in availableRoles" :key="role" color="neutral" variant="ghost" size="sm"
            class="w-full justify-start text-left" :disabled="locked || loading" :aria-pressed="modelOverride===role" @click="select(role)">
            <span class="min-w-0">
              {{ modelName(role) }}
              <span v-if="modelNames?.[role]" class="block break-all whitespace-normal text-xs text-muted">{{ modelNames[role] }}</span>
            </span>
          </UButton>
        </div>
      </template>
    </UPopover>
  </div>
</template>
