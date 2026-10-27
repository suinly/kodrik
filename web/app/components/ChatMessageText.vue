<script setup lang="ts">
import {t} from '../utils/i18n'
import {formatWebhookBody} from '../utils/chat-presentation'

const props=defineProps<{text:string;code?:boolean;reveal?:boolean;initiallyCollapsed?:boolean}>()
const expanded=ref(!props.initiallyCollapsed)
const root=useTemplateRef('root')
const contentId=useId()
const content=computed(()=>props.code ? formatWebhookBody(props.text) : props.text)
const long=computed(()=>content.value.length>1000 || content.value.split('\n').length>12)
const open=computed(()=>!long.value || expanded.value || props.reveal)

async function setOpen(value:boolean) {
  expanded.value=value
  await nextTick()
  root.value?.scrollIntoView({block:value?'start':'nearest'})
}
</script>

<template>
  <div ref="root" class="min-w-0 space-y-2">
    <div :id="contentId" :class="{'max-h-48 overflow-hidden':!open}" :inert="!open" data-search-text>
      <pre v-if="code" class="max-w-full overflow-x-auto rounded-md bg-muted p-3 text-sm/6"><code>{{ content }}</code></pre>
      <ChatMarkdown v-else :text="text" />
    </div>
    <UButton v-if="long && !reveal" :label="t(open ? 'Свернуть сообщение' : 'Развернуть сообщение')" :trailing-icon="open ? 'i-lucide-chevron-up' : 'i-lucide-chevron-down'"
      :aria-expanded="open" :aria-controls="contentId" color="neutral" variant="link" size="sm" class="px-0" @click="setOpen(!open)" />
  </div>
</template>
