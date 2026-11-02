<script setup lang="ts">
import {t} from '../utils/i18n'
import {linkTarget} from '../utils/link-target'
import {endpoint} from '../utils/api'
import type {UiDocument} from '../utils/ui-document'
const props=defineProps<{output:unknown;actions?:boolean;sessionId?:string;callId?:string}>()
const emit=defineEmits<{prompt:[text:string]}>()
const document=computed(()=>{
  const document=(props.output as {document?:UiDocument})?.document
  if(!document || !props.sessionId || !props.callId) return document
  return {...document,actions:document.actions?.map(action=>({...action,url:endpoint(`sessions/${encodeURIComponent(props.sessionId!)}/forms/${encodeURIComponent(props.callId!)}/${encodeURIComponent(action.id)}`)}))}
})
const url=computed(()=>{
  const value=(props.output as {url?:unknown})?.url
  return typeof value==='string'?value:undefined
})
const title=computed(()=>{
  const value=(document.value as {title?:unknown})?.title
  return typeof value==='string'?value:t("Документ")
})
const expanded=ref(false)
const expandedBody=useTemplateRef('expandedBody')
const description=computed(() => t("Снимок на момент создания. Данные на странице могут измениться."))
</script>

<template>
  <div class="min-w-0 space-y-3">
    <div class="flex flex-wrap items-center gap-2">
      <UButton v-if="url" :to="url" :target="linkTarget(url)" external :label="t('Открыть страницу')" icon="i-lucide-external-link" />
      <UModal v-model:open="expanded" fullscreen :title="title" :description="description">
        <UButton :label="t('Развернуть документ')" icon="i-lucide-maximize" color="neutral" variant="outline" />
        <template #close><UButton :aria-label="t('Закрыть документ')" icon="i-lucide-x" color="neutral" variant="ghost" /></template>
        <template #body><div ref="expandedBody" class="mx-auto w-full max-w-7xl" /></template>
      </UModal>
    </div>
    <p class="text-xs text-muted">{{ description }}</p>
    <!-- Move the same instance so local input and selection survive expanding. -->
    <Teleport :to="expandedBody ?? 'body'" :disabled="!expanded || !expandedBody">
      <UiDocument :document="document" :actions="actions" @prompt="emit('prompt',$event)" />
    </Teleport>
  </div>
</template>
