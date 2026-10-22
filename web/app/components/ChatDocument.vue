<script setup lang="ts">
import {t} from '../utils/i18n'
const props=defineProps<{output:unknown;actions?:boolean}>()
const emit=defineEmits<{prompt:[text:string]}>()
const document=computed(()=>(props.output as {document?:unknown})?.document)
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
      <UButton v-if="url" :to="url" external :label="t('Открыть страницу')" icon="i-lucide-external-link" />
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
