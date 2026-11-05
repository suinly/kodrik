<script setup lang="ts">
import {t} from '../utils/i18n'
import type {EnvironmentStatus} from '../utils/api'
defineProps<{status:EnvironmentStatus}>()
</script>

<template>
  <div role="status" :aria-label="t('Среда выполнения')" class="space-y-1 text-xs text-muted break-words">
    <div class="flex flex-wrap items-center gap-2">
      <UBadge :color="status.state==='connected' ? 'success' : 'neutral'" variant="soft">{{ status.state==='connected' ? t('Подключено') : status.state==='disconnected' ? t('Отключено') : status.state }}</UBadge>
      <span>{{ status.label }}</span>
    </div>
    <p class="font-mono whitespace-pre-wrap">{{ status.project_root }}</p>
    <p v-if="status.reason" class="whitespace-pre-wrap">{{ status.reason }}</p>
  </div>
</template>
