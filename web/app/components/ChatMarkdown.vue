<script setup lang="ts">
import { parseMarkdown } from '@nuxtjs/mdc/runtime'
import rehypeSanitize from 'rehype-sanitize'

const props = defineProps<{ text: string }>()
const body = shallowRef<Awaited<ReturnType<typeof parseMarkdown>>['body']>()

watch(() => props.text, async (text, _, onCleanup) => {
  let stale = false
  onCleanup(() => { stale = true })
  try {
    const parsed = await parseMarkdown(text, {
      toc: false, contentHeading: false, highlight: false,
      remark: { plugins: { 'remark-mdc': false } },
      rehype: {
        options: { allowDangerousHtml: false },
        plugins: { 'rehype-sanitize': { instance: rehypeSanitize } },
      },
    })
    if (!stale) body.value = parsed.body
  } catch { if (!stale) body.value = undefined }
}, { immediate: true })
</script>

<template>
  <!-- Ignore Markdown frontmatter: chat text cannot configure Vue components. -->
  <MDCRenderer v-if="body" :body="body" :data="{}" class="prose *:first:mt-0 *:last:mb-0" />
  <p v-else class="whitespace-pre-wrap">{{ text }}</p>
</template>
