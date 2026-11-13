<script setup lang="ts">
import {base} from '../utils/api'
import {t} from '../utils/i18n'
import {wakeupStatus, type WakeupsState} from '../utils/wakeups'
const props=defineProps<{manager:WakeupsState}>()
const phaseReason=computed(()=>{
  const phase=props.manager.events?.phase
  return phase && typeof phase==='object' ? Object.values(phase).find(value=>value?.reason)?.reason : null
})
const sourceUrl = (id:string)=>new URL(`app/chat/${encodeURIComponent(id)}`,base).pathname
</script>

<template>
  <section :aria-label="t('Пробуждения')" class="mx-auto w-full max-w-4xl space-y-6 p-4 sm:p-6">
    <p v-if="manager.error" role="alert" class="text-sm text-error">{{ t(manager.error) }}</p>
    <template v-if="manager.current">
      <UCard>
        <div class="space-y-3">
          <p class="whitespace-pre-wrap break-words">{{ manager.current.instruction }}</p>
          <UBadge color="neutral" variant="subtle">{{ wakeupStatus(manager.current.status) }}</UBadge>
          <p class="text-sm text-muted">{{ t('Запланировано на') }}: <time :datetime="manager.current.due_at">{{ manager.dateFormat.format(new Date(manager.current.due_at)) }}</time></p>
          <p v-if="manager.current.retry_at" class="text-sm text-muted">{{ t('Следующая попытка') }}: <time :datetime="manager.current.retry_at">{{ manager.dateFormat.format(new Date(manager.current.retry_at)) }}</time></p>
          <UButton v-if="manager.current.source_session" :to="sourceUrl(manager.current.source_session)" external target="_self" :label="t('Исходный диалог')" icon="i-lucide-message-square" color="neutral" variant="link" />
          <p v-if="manager.current.message" class="whitespace-pre-wrap break-words">{{ manager.current.message }}</p>
          <p v-if="manager.current.last_error" role="alert" class="text-sm text-error whitespace-pre-wrap break-words">{{ manager.current.last_error }}</p>
        </div>
      </UCard>
      <p v-if="manager.eventsLoading" role="status" class="text-sm text-muted">{{ t('Загружаю события…') }}</p>
      <p v-if="manager.eventsError" role="alert" class="text-sm text-error">{{ t(manager.eventsError) }}</p>
      <template v-if="manager.events">
        <p v-if="manager.events.busy" role="status" class="text-sm text-muted">{{ t('Выполняется') }}</p>
        <p v-if="manager.events.blocked_reason" role="status" class="text-sm text-muted whitespace-pre-wrap break-words">{{ manager.events.blocked_reason }}</p>
        <p v-if="phaseReason && phaseReason!==manager.events.blocked_reason" role="status" class="text-sm text-muted whitespace-pre-wrap break-words">{{ phaseReason }}</p>
        <ol v-if="manager.events.plan" :aria-label="t('План выполнения')" class="space-y-1 text-sm text-muted">
          <li v-for="(step,index) in manager.events.plan.plan" :key="index">{{ step.status==='completed'?'✓':step.status==='in_progress'?'…':'·' }} {{ step.step }}</li>
        </ol>
        <ChatItem v-for="message in manager.events.messages" :key="message.id" :message="message" :environment="manager.events.environment" :actions="false" :editable="false" />
        <ChatItem v-if="manager.events.live?.current?.text" :message="{id:'wakeup-progress',role:'assistant',parts:[{type:'text',text:manager.events.live.current.text}]}" :actions="false" :editable="false" />
        <p v-if="!manager.events.messages.length && !manager.events.live?.current?.text && !manager.events.busy" class="text-sm text-muted">{{ t('Пока нет событий.') }}</p>
      </template>
    </template>
    <p v-else-if="manager.selected && !manager.loading && !manager.error" class="text-sm text-muted">{{ t('Пробуждение удалено или не найдено.') }}</p>
    <p v-else-if="!manager.selected" class="text-sm text-muted">{{ t('Выберите пробуждение в боковом меню.') }}</p>
  </section>
</template>
