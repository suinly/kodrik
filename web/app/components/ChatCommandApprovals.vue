<script setup lang="ts">
import type {CommandApproval} from '../utils/api'
import {t} from '../utils/i18n'
import {commandAuditReason,commandApprovalDetails} from '../utils/command-audit'

defineProps<{approvals:CommandApproval[];deciding:Set<string>;problems:Record<string,string>;disabled:boolean}>()
const emit=defineEmits<{decide:[id:string,approve:boolean];refresh:[]}>()
</script>

<template>
  <section :aria-label="t('Подтверждение действий')" class="min-w-0 rounded-lg border border-warning/30 bg-warning/5 p-3">
    <p role="status" class="mb-2 text-sm font-medium text-highlighted">{{ t('Действия ожидают вашего решения') }}</p>
    <ul class="min-w-0 divide-y divide-default">
      <li v-for="approval in approvals" :key="approval.id" :data-testid="`command-approval-${approval.id}`" class="min-w-0 space-y-2 py-2 text-sm">
        <pre tabindex="0" :aria-label="t('Вызов инструмента')" class="max-h-48 max-w-full overflow-auto overscroll-contain rounded bg-elevated p-2 font-mono text-xs whitespace-pre"><code>{{ commandApprovalDetails(approval) }}</code></pre>
        <p class="whitespace-pre-wrap [overflow-wrap:anywhere]">{{ commandAuditReason(approval) }}</p>
        <div class="flex flex-wrap gap-2">
          <UButton :label="t('Разрешить один раз')" color="warning" variant="soft" :disabled="disabled || deciding.has(approval.id)" :loading="deciding.has(approval.id)" @click="emit('decide',approval.id,true)" />
          <UButton :label="t('Отклонить')" color="neutral" variant="outline" :disabled="disabled || deciding.has(approval.id)" @click="emit('decide',approval.id,false)" />
        </div>
        <template v-if="problems[approval.id]">
          <p role="alert" class="text-error [overflow-wrap:anywhere]">{{ problems[approval.id] }}</p>
          <UButton :label="t('Обновить историю')" color="neutral" variant="link" size="sm" class="px-0" @click="emit('refresh')" />
        </template>
      </li>
    </ul>
  </section>
</template>
