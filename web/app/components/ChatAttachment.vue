<script setup lang="ts">
import {attachmentLabel} from '../utils/chat-presentation'
import {t} from '../utils/i18n'
import {linkTarget} from '../utils/link-target'
import {attachmentUrl,type FilePart} from '../utils/api'
defineProps<{file:FilePart}>()
</script>

<template>
  <div class="flex flex-col items-start gap-2">
    <UModal v-if="file.url && file.mediaType.startsWith('image/')" :title="attachmentLabel(file)" :ui="{content:'w-[calc(100vw-2rem)] max-w-6xl',body:'flex min-h-0 items-center justify-center'}">
      <button type="button" :aria-label="attachmentLabel(file)" class="max-w-full cursor-zoom-in rounded-lg focus-visible:outline-2 focus-visible:outline-primary">
        <img :src="attachmentUrl(file.url)" :alt="attachmentLabel(file)" loading="lazy" class="max-h-72 max-w-full rounded-lg object-contain">
      </button>
      <template #close="{ui}"><UButton :aria-label="t('Закрыть')" :class="ui.close()" icon="i-lucide-x" color="neutral" variant="ghost" /></template>
      <template #body>
        <img :src="attachmentUrl(file.url)" :alt="attachmentLabel(file)" class="max-h-[calc(100dvh-12rem)] max-w-full object-contain">
      </template>
    </UModal>
    <UButton v-else :to="attachmentUrl(file.url)" :target="linkTarget(attachmentUrl(file.url))" :disabled="!file.url" external download color="neutral" variant="outline"
      :icon="file.mediaType.startsWith('image/') ? 'i-lucide-image' : 'i-lucide-file'"
      :label="attachmentLabel(file)" class="max-w-full" :ui="{label:'truncate'}" />
    <p v-if="file.origin" class="text-xs text-muted whitespace-pre-wrap break-all">{{ file.origin.label }} · {{ file.origin.project_root }} · {{ file.origin.environment.generation }}</p>
  </div>
</template>
