import { parseMarkdown } from '@nuxtjs/mdc/runtime'
import rehypeSanitize, { defaultSchema } from 'rehype-sanitize'
import remarkMath from 'remark-math'
import rehypeKatex from 'rehype-katex'
import { unified } from 'unified'
import remarkParse from 'remark-parse'
import type { RootContent, Root } from 'mdast'

// Normalize legacy TeX delimiters before Markdown consumes their backslashes.
function normalizeMath(text: string): string {
  const ranges: [number, number][] = []
  function protect(node: Root | RootContent) {
    if (['code', 'inlineCode', 'link', 'image', 'html', 'definition'].includes(node.type)) {
      if (node.position) ranges.push([node.position.start.offset!, node.position.end.offset!])
      return
    }
    if ('children' in node) node.children.forEach(protect)
  }
  protect(unified().use(remarkParse).parse(text))
  const convert = (source: string) => source
    .replace(/\\\[([\s\S]*?)\\\]/g, (_, math) => `\n$$\n${math.trim()}\n$$\n`)
    .replace(/\\\(([^\n]*?)\\\)/g, (_, math) => `$${math}$`)
    // A standalone bracketed TeX expression also covers older chat replies.
    .replace(/^[ \t]*\[([^\n]+)\][ \t]*$/gm, (source, math: string) =>
      /\\[a-zA-Z]+|[_^]/.test(math) ? `$$\n${math.trim()}\n$$` : source)
  let end = 0
  let result = ''
  for (const [start, stop] of ranges) {
    result += convert(text.slice(end, start)) + text.slice(start, stop)
    end = stop
  }
  return result + convert(text.slice(end))
}

export async function parseChatMarkdown(text: string) {
  return parseMarkdown(normalizeMath(text), {
      toc: false, contentHeading: false, highlight: false,
      remark: { plugins: { 'remark-mdc': false, 'remark-math': { instance: remarkMath } } },
      rehype: {
        options: { allowDangerousHtml: false },
        plugins: { 'rehype-sanitize': {
          instance: rehypeSanitize,
          options: { ...defaultSchema, attributes: {
            ...defaultSchema.attributes,
            // Keep fenced code native; inline code still uses ProseCode.
            code: [...(defaultSchema.attributes?.code || []), '__ignoreMap', ['className', 'language-math', 'math-inline', 'math-display']],
            pre: [...(defaultSchema.attributes?.pre || []), 'language'],
          } },
        }, 'rehype-katex': { instance: rehypeKatex, options: { trust: false, strict: 'ignore', throwOnError: false, maxSize: 20, maxExpand: 1000 } } },
      },
    })
}
