<script setup lang="ts">
import {t} from '../utils/i18n'
import type {WebhooksState} from '../utils/webhooks'
defineProps<{manager:WebhooksState}>()
const emit=defineEmits<{open:[id:string]}>()
</script>

<template>
  <section :aria-label="t('Просмотр вебхука')" class="mx-auto w-full max-w-4xl space-y-6 p-4 sm:p-6">
    <p v-if="manager.error" role="alert" class="text-sm text-error">{{ t(manager.error) }}</p>
    <div v-if="manager.editing" class="min-w-0 space-y-6">
      <form class="space-y-4" @submit.prevent="manager.save">
        <UFormField :label="t('Название вебхука')" required>
          <UInput v-model="manager.draft.name" :aria-label="t('Название вебхука')" :maxlength="200" :disabled="manager.saving" required class="w-full" />
        </UFormField>
        <UFormField :label="t('Инструкция агенту')" :description="t('Опишите задачу, репозиторий и условия успешного выполнения.')" required>
          <UTextarea v-model="manager.draft.instruction" :aria-label="t('Инструкция агенту')" :maxlength="16384" :rows="6" :disabled="manager.saving" required class="w-full" />
        </UFormField>
        <USwitch v-model="manager.draft.enabled" :label="t('Вебхук включён')" :disabled="manager.saving" />
        <UButton type="submit" :label="t('Сохранить вебхук')" :loading="manager.saving" :disabled="!manager.draft.name.trim() || !manager.draft.instruction.trim()" />
      </form>
      <template v-if="manager.current">
        <UFormField :label="t('URL вебхука')"><UInput :model-value="manager.current.url" :aria-label="t('URL вебхука')" readonly class="w-full font-mono" /></UFormField>
        <div v-if="manager.token" class="space-y-3 rounded-lg border border-warning/40 p-4">
          <p class="text-sm text-highlighted">{{ t('Сохраните токен сейчас. После выхода из раздела он больше не отобразится.') }}</p>
          <UFormField :label="t('Bearer-токен')"><UInput :model-value="manager.token" :aria-label="t('Bearer-токен')" readonly autocomplete="off" class="w-full font-mono" /></UFormField>
          <UButton :label="manager.copied?t('Скопировано'):t('Скопировать токен')" icon="i-lucide-copy" color="neutral" variant="soft" @click="manager.copyToken" />
        </div>
        <div class="space-y-2">
          <p class="text-sm font-medium text-highlighted">{{ t('Пример запроса') }}</p>
          <pre class="overflow-x-auto rounded-lg bg-elevated p-3 text-xs"><code>{{ manager.curl }}</code></pre>
          <p class="text-xs leading-relaxed text-muted">{{ t('Тело: произвольный текст или JSON в UTF-8, до 64 KiB. До 10 новых событий в минуту. Для повторов передавайте одинаковый Idempotency-Key.') }}</p>
        </div>
        <div class="flex flex-wrap gap-2">
          <UButton :label="t('Перевыпустить токен')" icon="i-lucide-key-round" color="neutral" variant="outline" :disabled="manager.saving" @click="manager.confirmation='rotate'" />
          <UButton :label="t('Удалить вебхук')" icon="i-lucide-trash-2" color="error" variant="ghost" :disabled="manager.saving" @click="manager.confirmation='delete'" />
        </div>
        <section class="space-y-3" :aria-label="t('Последние события')">
          <h3 class="text-sm font-medium text-highlighted">{{ t('Последние события') }}</h3>
          <p v-if="!manager.deliveries.length" class="text-sm text-muted">{{ t('Принятые события появятся здесь.') }}</p>
          <div v-for="event in manager.deliveries" :key="`${event.accepted_at}:${event.conversation_id}`" class="flex flex-wrap items-center justify-between gap-2 text-sm">
            <div><time :datetime="event.accepted_at">{{ manager.dateFormat.format(new Date(event.accepted_at)) }}</time><span class="ml-2 text-muted">{{ t(manager.statusLabels[event.status] ?? event.status) }}</span></div>
            <UButton v-if="event.conversation_id" :label="t('Открыть обработку')" color="neutral" variant="link" @click="emit('open',event.conversation_id)" />
          </div>
        </section>
      </template>
    </div>
    <p v-else class="text-sm leading-relaxed text-muted">{{ t('Создайте вебхук и задайте инструкцию. Каждый запрос запустит агента в отдельном диалоге.') }}</p>
  </section>
  <UModal :open="manager.confirmation!==null" :title="manager.confirmation==='rotate'?t('Перевыпустить токен?'):t('Удалить вебхук?')" :dismissible="!manager.saving" :close="manager.saving?false:undefined" @update:open="value=>{if(!value) manager.confirmation=null}">
    <template #body><p class="text-sm text-muted">{{ manager.confirmation==='rotate'?t('Старый токен сразу перестанет работать. Обновите его у отправителя событий.'):t('Приём новых событий прекратится. Уже принятые задачи и их диалоги сохранятся.') }}</p></template>
    <template #footer>
      <UButton :label="t('Отмена')" color="neutral" variant="ghost" :disabled="manager.saving" @click="manager.confirmation=null" />
      <UButton :label="manager.confirmation==='rotate'?t('Подтвердить перевыпуск'):t('Подтвердить удаление')" :color="manager.confirmation==='delete'?'error':'primary'" :loading="manager.saving" @click="manager.confirm" />
    </template>
  </UModal>
</template>
