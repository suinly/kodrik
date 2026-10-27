<script setup lang="ts">
import { parseMarkdown } from '@nuxtjs/mdc/runtime'
import type { MDCNode, MDCRoot } from '@nuxtjs/mdc'
import rehypeSanitize, { defaultSchema } from 'rehype-sanitize'
import MarkdownLink from './MarkdownLink.vue'

const props = defineProps<{ text: string }>()
const body = shallowRef<Awaited<ReturnType<typeof parseMarkdown>>['body']>()

async function highlightCode(node: MDCNode | MDCRoot): Promise<void> {
  if (node.type !== 'element' && node.type !== 'root') return
  if (node.type === 'element' && node.tag === 'pre') {
    const language = String(node.props?.language || 'text').toLowerCase()
    if (['text', 'txt', 'plaintext'].includes(language)) return
    const code = node.children.find(child => child.type === 'element' && child.tag === 'code')
    if (!code || code.type !== 'element') return
    const { bundledLanguages, codeToTokens, getSingletonHighlighter } = await import('shiki')
    if (!Object.hasOwn(bundledLanguages, language)) return
    const { createJavaScriptRegexEngine } = await import('shiki/engine/javascript')
    // The app CSP blocks WebAssembly compilation; keep highlighting in JavaScript.
    await getSingletonHighlighter({ engine: createJavaScriptRegexEngine({ forgiving: true }) })
    const source = code.children.map(child => child.type === 'text' ? child.value : '').join('')
    // Use tokens directly: MDC's highlighter trims code and interprets notation comments.
    const result = await codeToTokens(source, {
      lang: language as keyof typeof bundledLanguages,
      themes: { light: 'catppuccin-latte', dark: 'catppuccin-mocha' }, defaultColor: false,
    }).catch(() => null)
    if (!result) return
    code.props = { ...code.props, class: 'syntax-code' }
    code.children = result.tokens.flatMap<MDCNode>((line, index) => [
      ...line.map<MDCNode>(token => ({ type: 'element', tag: 'span', props: { style: token.htmlStyle }, children: [{ type: 'text', value: token.content }] })),
      ...(index < result.tokens.length - 1 ? [{ type: 'text' as const, value: '\n' }] : []),
    ])
    return
  }
  await Promise.all(node.children.map(highlightCode))
}

watch(() => props.text, async (text, _, onCleanup) => {
  let stale = false
  onCleanup(() => { stale = true })
  try {
    const parsed = await parseMarkdown(text, {
      toc: false, contentHeading: false, highlight: false,
      remark: { plugins: { 'remark-mdc': false } },
      rehype: {
        options: { allowDangerousHtml: false },
        plugins: { 'rehype-sanitize': {
          instance: rehypeSanitize,
          options: { ...defaultSchema, attributes: {
            ...defaultSchema.attributes,
            // Keep fenced code native; inline code still uses ProseCode.
            code: [...(defaultSchema.attributes?.code || []), '__ignoreMap'],
            pre: [...(defaultSchema.attributes?.pre || []), 'language'],
          } },
        } },
      },
    })
    await highlightCode(parsed.body)
    if (!stale) body.value = parsed.body
  } catch { if (!stale) body.value = undefined }
}, { immediate: true })
</script>

<template>
  <!-- Ignore Markdown frontmatter: chat text cannot configure Vue components. -->
  <MDCRenderer v-if="body" :body="body" :data="{}" :components="{a:MarkdownLink}" class="prose *:first:mt-0 *:last:mb-0" />
  <p v-else class="whitespace-pre-wrap">{{ text }}</p>
</template>
