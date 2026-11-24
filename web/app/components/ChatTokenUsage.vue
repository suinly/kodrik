<script setup lang="ts">
import {locale,t} from '../utils/i18n'
import type {SessionCost,TokenUsage} from '../utils/api'
import {tokenUsageSummary} from '../utils/token-usage'

const props=defineProps<{usage?:TokenUsage|null;cost?:SessionCost;runCost?:Pick<SessionCost,'total_usd'|'partial'>|null;context?:number|null;limit?:number|null;threshold?:number|null}>()
const summary=computed(()=>tokenUsageSummary(props.usage,props.context,props.limit))
const compact=(value:number|null|undefined)=>value==null?'—':new Intl.NumberFormat(locale.value,{notation:'compact',maximumFractionDigits:1}).format(value)
const exact=(value:number|null|undefined)=>value==null?t('Нет данных'):new Intl.NumberFormat(locale.value).format(value)
const usd=computed(()=>new Intl.NumberFormat(locale.value,{style:'currency',currency:'USD',maximumFractionDigits:6}))
const usdFloor=0.000001
const money=(value:number|null)=>value==null?t('Нет данных'):value>0 && value<usdFloor?`<${usd.value.format(usdFloor)}`:usd.value.format(value)
const displayTotal=(value:Pick<SessionCost,'total_usd'|'partial'>)=>`${value.partial && (value.total_usd===0 || value.total_usd>=usdFloor)?'≥':''}${money(value.total_usd)}`
const costTotal=computed(()=>props.cost ? displayTotal(props.cost) : '')
const callsByModel=computed(()=>{
  const groups=new Map<string,NonNullable<typeof props.cost>['calls']>()
  for(const call of props.cost?.calls ?? []) groups.set(call.model,[...(groups.get(call.model) ?? []),call])
  return [...groups].map(([model,calls])=>({model,calls,total:calls.reduce((sum,call)=>sum+(call.cost_usd ?? 0),0),partial:calls.some(call=>call.cost_usd==null)}))
})
</script>

<template>
  <UPopover mode="hover" enable-touch :content="{align:'end'}">
    <UButton data-testid="token-usage" color="neutral" variant="ghost" size="sm" :aria-label="t(cost ? 'Токены и стоимость' : 'Статистика токенов')" class="gap-1 tabular-nums whitespace-nowrap">
      <UIcon name="i-lucide-chart-no-axes-column" class="size-4 shrink-0" />
      <span>{{ summary.partial && summary.total!=null?'≥':'' }}{{ compact(summary.total) }}</span>
      <span class="text-dimmed">·</span>
      <span class="text-muted">{{ summary.percent!=null?`${summary.percent}%`:compact(summary.context) }}</span>
      <template v-if="cost">
        <span class="text-dimmed">·</span>
        <span>{{ costTotal }}</span>
      </template>
    </UButton>
    <template #content>
      <div class="max-h-[65dvh] w-72 max-w-[calc(100vw-1rem)] space-y-3 overflow-y-auto p-3 text-sm" data-testid="token-usage-details">
        <section v-if="cost" data-testid="session-cost" :aria-label="t('Стоимость диалога')">
          <p class="mb-1 flex justify-between gap-2 font-medium tabular-nums">
            <span>{{ t('Стоимость диалога') }}</span><span>{{ costTotal }}</span>
          </p>
          <p v-if="runCost" class="mb-1 flex justify-between gap-2 text-muted tabular-nums"><span>{{ t('Последний ход') }}</span><span>{{ displayTotal(runCost) }}</span></p>
          <p v-if="cost.partial" class="mb-1 text-xs text-muted">{{ t('Учёт неполный: старые вызовы или данные провайдера могут отсутствовать.') }}</p>
          <p v-if="!cost.calls.length" class="text-xs text-muted">{{ t('Нет данных о вызовах.') }}</p>
          <ul v-else :aria-label="t('Вызовы модели')" class="space-y-2 text-xs">
            <li v-for="group in callsByModel" :key="group.model" data-testid="model-call-group" class="border-b border-default pb-1">
              <p class="mb-0.5 flex items-start justify-between gap-2 font-medium">
                <span class="min-w-0 [overflow-wrap:anywhere]">{{ group.model }}</span>
                <span class="shrink-0 tabular-nums">{{ displayTotal({total_usd:group.total,partial:group.partial}) }}</span>
              </p>
              <ul class="divide-y divide-default">
                <li v-for="call in group.calls" :key="call.id" class="flex items-start justify-between gap-2 py-1">
                  <span class="text-muted">{{ call.kind }}</span>
                  <span class="shrink-0 tabular-nums">{{ money(call.cost_usd) }}</span>
                </li>
              </ul>
            </li>
          </ul>
        </section>
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
        <p v-if="summary.partial && !cost?.partial" class="text-xs text-muted">{{ t('Учёт неполный: старые вызовы или данные провайдера могут отсутствовать.') }}</p>
      </div>
    </template>
  </UPopover>
</template>
