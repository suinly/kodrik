<script setup lang="ts">
import {t} from '../utils/i18n'
import {pushEnabled, pushReady, pushWorking, pushProblem, pushPermission, pushSupported, initializePush, enablePush, disablePush} from '../utils/push'

onMounted(initializePush)
</script>

<template>
  <section class="space-y-4" :aria-label="t('Уведомления')">
    <h2 class="text-lg font-semibold text-highlighted">{{ t('Уведомления') }}</h2>
    <p class="text-sm leading-relaxed text-muted">{{ t('Push о готовых ответах и новых сообщениях агента, даже когда чат закрыт. Текст переписки в уведомлениях не показывается.') }}</p>
    <p class="text-sm leading-relaxed text-muted">{{ t('На iPhone и iPad сначала добавьте Kodrik на домашний экран через меню «Поделиться».') }}</p>
    <p v-if="!pushSupported" role="status" class="text-sm text-muted">{{ t('Этот браузер не поддерживает push. Откройте Kodrik в совместимом браузере или установленном приложении.') }}</p>
    <template v-else>
      <p v-if="pushPermission==='denied'" role="status" class="text-sm text-muted">{{ t('Уведомления заблокированы. Разрешите их в настройках сайта в браузере.') }}</p>
      <p v-else role="status" class="text-sm text-muted">{{ t(pushEnabled?'Уведомления включены на этом устройстве.':'Уведомления выключены на этом устройстве.') }}</p>
      <UButton v-if="pushEnabled" :label="t('Выключить уведомления')" icon="i-lucide-bell-off" color="neutral" variant="soft" :loading="pushWorking" @click="disablePush" />
      <UButton v-else :label="t('Включить уведомления')" icon="i-lucide-bell" :loading="pushWorking || !pushReady && !pushProblem" :disabled="!pushReady || pushPermission==='denied'" @click="enablePush" />
      <UButton v-if="!pushReady && pushProblem" :label="t('Повторить')" color="neutral" variant="link" @click="initializePush" />
    </template>
    <p v-if="pushProblem" role="alert" class="text-sm text-error">{{ t(pushProblem) }}</p>
  </section>
</template>
