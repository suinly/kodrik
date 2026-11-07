<script setup lang="ts">
import {locale,t} from '../utils/i18n'
import type {TokenUsage} from '../utils/api'
import {tokenUsageSummary} from '../utils/token-usage'

const props=defineProps<{usage?:TokenUsage|null;context?:number|null;limit?:number|null;threshold?:number|null}>()
const summary=computed(()=>tokenUsageSummary(props.usage,props.context,props.limit))
const compact=(value:number|null|undefined)=>value==null?'—':new Intl.NumberFormat(locale.value,{notation:'compact',maximumFractionDigits:1}).format(value)
const exact=(value:number|null|undefined)=>value==null?t('Нет данных'):new Intl.NumberFormat(locale.value).format(value)
</script>

<template>
  <UPopover mode="hover" enable-touch :content="{align:'end'}">
    <UButton data-testid="token-usage" color="neutral" variant="ghost" size="sm" :aria-label="t('Статистика токенов')" class="gap-1 tabular-nums whitespace-nowrap">
      <UIcon name="i-lucide-chart-no-axes-column" class="size-4 shrink-0" />
      <span>{{ summary.partial && summary.total!=null?'≥':'' }}{{ compact(summary.total) }}</span>
      <span class="text-dimmed">·</span>
      <span class="text-muted">{{ summary.percent!=null?`${summary.percent}%`:compact(summary.context) }}</span>
    </UButton>
    <template #content>
      <div class="w-72 max-w-[calc(100vw-1rem)] space-y-3 p-3 text-sm" data-testid="token-usage-details">
        <div>
          <p class="mb-1 font-medium">{{ t('Токены за диалог') }}</p>
          <dl class="grid grid-cols-2 gap-x-3 gap-y-1 tabular-nums">
            <dt class="text-muted">{{ t('Входящие') }}</dt><dd class="text-right">{{ exact(usage?.input_tokens) }}</dd>
            <dt class="text-muted">{{ t('Исходящие') }}</dt><dd class="text-right">{{ exact(usage?.output_tokens) }}</dd>
            <dt class="text-muted">{{ t('Из кэша') }}</dt><dd class="text-right">{{ exact(usage?.cached_tokens) }}</dd>
            <dt class="text-muted">{{ t('Всего') }}</dt><dd class="text-right">{{ exact(summary.total) }}</dd>
            <dt class="text-muted">{{ t('Вызовы модели') }}</dt><dd class="text-right">{{ exact(usage?.requests) }}</dd>
          </dl>
        </div>
        <div>
          <p class="mb-1 font-medium">{{ t('Контекст последнего запроса') }}</p>
          <p class="tabular-nums">{{ exact(context) }}<span v-if="limit"> / {{ exact(limit) }} · {{ summary.percent }}%</span></p>
          <p v-if="!limit" class="mt-1 text-xs text-muted">{{ t('Лимит контекста модели неизвестен.') }}</p>
          <p v-if="threshold" class="mt-1 text-xs text-muted">{{ t('Порог сжатия: {0} токенов',exact(threshold)) }}</p>
        </div>
        <p class="text-xs text-muted">{{ t('Кэш входит во входящие токены, а не прибавляется к ним.') }}</p>
        <p class="text-xs text-muted">{{ t('Только текущая ветка, без сабагентов, названий и сжатия. Не включает прерванные запросы без usage.') }}</p>
        <p v-if="summary.partial" class="text-xs text-muted">{{ t('Учёт неполный: старые вызовы или данные провайдера могут отсутствовать.') }}</p>
      </div>
    </template>
  </UPopover>
</template>
