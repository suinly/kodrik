<script setup lang="ts">
import {t} from '../utils/i18n'
import {toolSummary,type ToolPart} from '../utils/chat-presentation'
const props=defineProps<{tools:ToolPart[]}>()
const open=ref(false)
const steps=useTemplateRef('steps')
const running=computed(()=>props.tools.some(tool=>!['output-available','output-error'].includes(tool.state)))
const failed=computed(()=>props.tools.filter(tool=>tool.state==='output-error').length)
async function collapseSteps() {
  open.value=false
  await nextTick()
  const trigger=steps.value?.querySelector('button')
  trigger?.focus({preventScroll:true})
  trigger?.scrollIntoView({block:'nearest'})
}
</script>

<template>
  <div ref="steps" class="min-w-0">
  <UChatTool v-model:open="open" :text="running ? t('Выполняются шаги: {0}', tools.length) : t('Выполнено шагов: {0}', tools.length)"
    :loading="running" icon="i-lucide-list-checks" :ui="{body:'space-y-2 border-l border-default ml-2 pl-3'}" unmount-on-hide>
    <p v-if="failed" class="text-sm text-muted">{{ t('Шагов с ошибкой: {0}. Подробности доступны ниже.', failed) }}</p>
    <UChatTool v-for="tool in tools" :key="tool.toolCallId" :text="tool.toolName" :suffix="toolSummary(tool) && ` · ${toolSummary(tool)}`" unmount-on-hide
      :loading="!['output-available','output-error'].includes(tool.state)"
      :ui="{leadingIcon:tool.state==='output-error'?'text-error':undefined}"
      :icon="tool.state === 'output-error' ? 'i-lucide-circle-alert' : 'i-lucide-check'">
      <div class="space-y-2 text-sm">
        <p class="text-muted"> {{ t("Входные данные") }} </p>
        <pre class="max-h-64 overflow-auto whitespace-pre-wrap break-all">{{ JSON.stringify(tool.input,null,2) }}</pre>
        <template v-if="tool.state==='output-available'">
          <p class="text-muted"> {{ t("Результат") }} </p>
          <pre class="max-h-64 overflow-auto whitespace-pre-wrap break-all">{{ JSON.stringify(tool.output,null,2) }}</pre>
        </template>
        <p v-if="tool.state==='output-error'" role="alert" class="text-sm whitespace-pre-wrap break-words">{{ tool.errorText }}</p>
      </div>
    </UChatTool>
    <div class="sticky bottom-0 bg-default py-1">
      <UButton :label="t('Свернуть шаги')" icon="i-lucide-chevron-up" color="neutral" variant="link" size="sm" class="px-0" @click="collapseSteps" />
    </div>
  </UChatTool>
  </div>
</template>
