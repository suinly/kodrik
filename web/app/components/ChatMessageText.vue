<script setup lang="ts">
import {t} from '../utils/i18n'
import {formatWebhookBody} from '../utils/chat-presentation'

const props=defineProps<{text:string;code?:boolean;reveal?:boolean}>()
const expanded=ref(false)
const root=useTemplateRef('root')
const content=computed(()=>props.code ? formatWebhookBody(props.text) : props.text)
const long=computed(()=>content.value.length>1000 || content.value.split('\n').length>12)
const open=computed(()=>!long.value || expanded.value || props.reveal)
const preview=computed(()=>content.value.split('\n').slice(0,4).join('\n').slice(0,240).trimEnd()+'…')

async function setOpen(value:boolean) {
  expanded.value=value
  await nextTick()
  root.value?.scrollIntoView({block:value?'start':'nearest'})
}
</script>

<template>
  <div ref="root" class="min-w-0 space-y-2">
    <p v-if="!open" class="whitespace-pre-wrap break-words" data-search-text>{{ preview }}</p>
    <UCollapsible :open="open" :ui="{content:'animate-none!'}" @update:open="setOpen">
      <template v-if="long && !reveal" #default>
        <UButton :label="t(open ? 'Свернуть сообщение' : 'Развернуть сообщение')" :trailing-icon="open ? 'i-lucide-chevron-up' : 'i-lucide-chevron-down'"
          color="neutral" variant="link" size="sm" class="px-0" />
      </template>
      <template #content>
        <pre v-if="code" class="max-w-full overflow-x-auto rounded-md bg-muted p-3 text-sm/6" data-search-text><code>{{ content }}</code></pre>
        <ChatMarkdown v-else :text="text" data-search-text />
      </template>
    </UCollapsible>
  </div>
</template>
