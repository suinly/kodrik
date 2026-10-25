<script setup lang="ts">
import type {Subagent} from '../utils/api'
import {t} from '../utils/i18n'
import {shortConversationTitle} from '../utils/chat-presentation'

const props=defineProps<{agents:Subagent[];stopping:Set<string>;problems:Record<string,string>;disabled:boolean}>()
const emit=defineEmits<{stop:[id:string];refresh:[]}>()
const open=ref(props.agents.some(agent=>agent.status==='running') ? 'subagents' : undefined)
const running=computed(()=>props.agents.filter(agent=>agent.status==='running').length)
const attention=computed(()=>props.agents.filter(agent=>agent.status==='failed' || agent.status==='interrupted').length)
const summary=computed(()=>running.value ? t('Дочерние задачи · в работе: {0}',running.value)
  : attention.value ? t('Дочерние задачи · требуют внимания: {0}',attention.value) : t('Дочерние задачи · {0}',props.agents.length))
const announcement=computed(()=>t('Дочерние задачи: в работе {0}, готово {1}, остановлено {2}, требуют внимания {3}.',running.value,
  props.agents.filter(agent=>agent.status==='completed').length,props.agents.filter(agent=>agent.status==='stopped').length,attention.value))
const states={
  running:{label:'В работе',icon:'i-lucide-loader-circle',color:'text-primary'},
  completed:{label:'Готово',icon:'i-lucide-circle-check',color:'text-success'},
  stopped:{label:'Остановлено',icon:'i-lucide-circle-stop',color:'text-muted'},
  failed:{label:'Ошибка',icon:'i-lucide-circle-alert',color:'text-error'},
  interrupted:{label:'Прервано',icon:'i-lucide-circle-pause',color:'text-warning'},
}
const title=(agent:Subagent)=>shortConversationTitle(agent.task?.split('\n').find(line=>line.trim())?.trim() || t('Дочерняя задача'))
const state=(agent:Subagent)=>states[agent.status] ?? {label:'Нет данных',icon:'i-lucide-circle-help',color:'text-muted'}
const stopping=(agent:Subagent)=>props.stopping.has(agent.id) || !!agent.cancel_requested
const status=(agent:Subagent)=>t(stopping(agent) && agent.status==='running' ? 'Останавливаю…' : state(agent).label)
</script>

<template>
  <section :aria-label="t('Дочерние задачи')" data-testid="subagents-panel" class="rounded-lg bg-elevated/50 px-3">
  <p role="status" aria-live="polite" class="sr-only">{{ announcement }}</p>
  <UAccordion v-model="open"
    :items="[{label:summary,icon:'i-lucide-git-branch',value:'subagents'}]"
    :ui="{body:'max-h-64 overflow-y-auto overscroll-contain',trigger:'py-2.5'}">
    <template #body>
      <ul class="divide-y divide-default">
        <li v-for="agent in agents" :key="agent.id" :data-testid="`subagent-${agent.id}`" class="grid grid-cols-[minmax(0,1fr)_auto] gap-x-2 py-1">
          <UCollapsible class="min-w-0">
            <UButton :aria-label="t('Подробности задачи «{0}»',title(agent))" :aria-describedby="`status-${agent.id}`"
              :icon="state(agent).icon" color="neutral" variant="ghost" class="w-full justify-start px-0 text-left"
              :ui="{label:'min-w-0 flex-1',leadingIcon:agent.status==='running' ? 'motion-safe:animate-spin text-primary' : state(agent).color}">
              <span class="min-w-0 flex-1 truncate text-sm text-highlighted">{{ title(agent) }}</span>
              <span :id="`status-${agent.id}`" class="shrink-0 whitespace-nowrap text-xs" :class="state(agent).color">{{ status(agent) }}</span>
            </UButton>
            <template #content>
              <div class="space-y-2 py-2 text-sm [overflow-wrap:anywhere]">
                <p v-if="agent.task" class="whitespace-pre-wrap text-muted">{{ agent.task }}</p>
                <ChatMarkdown v-if="agent.result" :text="agent.result" />
                <p v-if="agent.status==='running'" class="text-muted">{{ t('Результат появится после завершения задачи.') }}</p>
                <p v-if="agent.status==='interrupted'" class="text-warning">{{ t('Запуск прерван. Проверьте результат и дайте новое указание в чате.') }}</p>
                <p v-if="agent.status==='stopped'" class="text-muted">{{ t('Уже выполненные действия не отменяются.') }}</p>
                <p v-if="agent.error && agent.status!=='stopped'" class="whitespace-pre-wrap text-muted">{{ agent.error }}</p>
              </div>
            </template>
          </UCollapsible>
          <UButton v-if="agent.status==='running'" :label="t('Остановить')" :aria-label="t('Остановить задачу «{0}»',title(agent))"
            :loading="stopping(agent)" :disabled="disabled || stopping(agent)" icon="i-lucide-square"
            color="neutral" variant="ghost" size="sm" class="mt-1 self-start" @click="emit('stop',agent.id)" />
          <div v-if="problems[agent.id]" class="col-span-2 space-y-1 pb-2 [overflow-wrap:anywhere]">
            <p role="alert" class="text-sm text-error">{{ problems[agent.id] }}</p>
            <UButton :label="t('Обновить задачи')" color="neutral" variant="link" size="sm" class="px-0" @click="emit('refresh')" />
          </div>
        </li>
      </ul>
      <p v-if="running" class="pt-2 text-xs text-muted">{{ t('Дочерние задачи остановятся, когда завершится основной ответ.') }}</p>
    </template>
  </UAccordion>
  </section>
</template>
