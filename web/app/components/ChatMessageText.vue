<script setup lang="ts">
import {formatWebhookBody} from '../utils/chat-presentation'

const props=defineProps<{text:string;code?:boolean}>()
const content=computed(()=>props.code ? formatWebhookBody(props.text) : props.text)
</script>

<template>
  <div class="min-w-0" data-search-text>
    <pre v-if="code" class="max-w-full overflow-x-auto rounded-md bg-muted p-3 text-sm/6"><code>{{ content }}</code></pre>
    <ChatMarkdown v-else :text="text" />
  </div>
</template>
