<script setup lang="ts">
import {locale,t} from '../utils/i18n'
import {api,type ModelCall,type SessionCost,type TokenUsage} from '../utils/api'
import {tokenUsageSummary} from '../utils/token-usage'

const props=defineProps<{sessionId?:string;usage?:TokenUsage|null;cost?:SessionCost;runCost?:Pick<SessionCost,'total_usd'|'partial'>|null;context?:number|null;limit?:number|null;threshold?:number|null}>()
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
  for(const call of props.cost?.calls ?? []) {
    if(!groups.has(call.model)) groups.set(call.model,[])
    groups.get(call.model)!.push(call)
  }
  return props.cost?.models ?? [...groups].map(([model,calls])=>({model,count:calls.length,total_usd:calls.reduce((sum,call)=>sum+(call.cost_usd ?? 0),0),partial:calls.some(call=>call.cost_usd==null)}))
})
const pages=reactive(new Map<string,{calls:ModelCall[];next:number|null;loading:boolean;error:boolean;loaded:boolean}>())
async function load(model:string,more=false) {
  if(!props.sessionId || !props.cost?.models) return
  if(!pages.has(model)) pages.set(model,{calls:[],next:null,loading:false,error:false,loaded:false})
  const state=pages.get(model)!
  if(state.loading || (more && state.next===null)) return
  const query=new URLSearchParams({model})
  if(more) query.set('before',String(state.next))
  state.loading=true
  state.error=false
  try {
    const page=await api<{calls:ModelCall[];next:number|null}>(`sessions/${encodeURIComponent(props.sessionId)}/cost/calls?${query}`)
    state.calls=more ? [...state.calls,...page.calls] : page.calls
    state.next=page.next
    state.loaded=true
  } catch { state.error=true }
  finally {state.loading=false}
}
const rows=(model:string)=>{
  const recent=props.cost?.calls.filter(call=>call.model===model) ?? []
  const stored=pages.get(model)?.calls ?? []
  const seen=new Set(recent.map(call=>call.id))
  return [...recent,...stored.filter(call=>!seen.has(call.id))]
}
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
          <p class="mb-1 text-xs text-muted">{{ t('Стоимость включает вызовы субагентов.') }}</p>
          <p v-if="cost.partial" class="mb-1 text-xs text-muted">{{ t('Учёт неполный: старые вызовы или данные провайдера могут отсутствовать.') }}</p>
          <p v-if="!callsByModel.length" class="text-xs text-muted">{{ t('Нет данных о вызовах.') }}</p>
          <ul v-else :aria-label="t('Вызовы модели')" class="space-y-2 text-xs">
            <li v-for="group in callsByModel" :key="group.model" data-testid="model-call-group" class="border-b border-default pb-1">
              <details @toggle="($event.currentTarget as HTMLDetailsElement).open && !pages.get(group.model)?.loaded && load(group.model)">
                <summary class="cursor-pointer py-1 font-medium [overflow-wrap:anywhere]">
                  <span>{{ group.model }}</span> · <span class="tabular-nums">{{ displayTotal(group) }}</span> · {{ exact(group.count) }}
                </summary>
                <div class="max-h-48 overflow-y-auto overscroll-contain" data-testid="model-call-scroll">
                <ul class="divide-y divide-default">
                <li v-for="call in rows(group.model)" :key="call.id" class="flex items-start justify-between gap-2 py-1">
                  <span class="text-muted">{{ call.kind }}</span>
                  <span class="shrink-0 tabular-nums">{{ money(call.cost_usd) }}</span>
                </li>
                </ul>
                <UButton v-if="pages.get(group.model)?.next || pages.get(group.model)?.error" color="neutral" variant="ghost" size="xs" :loading="pages.get(group.model)?.loading" @click="load(group.model,pages.get(group.model)?.next != null)">{{ t(pages.get(group.model)?.error ? 'Повторить' : 'Показать ещё') }}</UButton>
                </div>
              </details>
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
        <p class="text-xs text-muted">{{ t('Токены — только текущая ветка, без сабагентов, названий и сжатия. Не включает прерванные запросы без usage.') }}</p>
        <p v-if="summary.partial && !cost?.partial" class="text-xs text-muted">{{ t('Учёт неполный: старые вызовы или данные провайдера могут отсутствовать.') }}</p>
      </div>
    </template>
  </UPopover>
</template>
