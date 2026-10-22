<script setup lang="ts">
import {t} from '../utils/i18n'
import type { ChatMessage } from '../utils/api'
import {groupMessageParts,formatMessageTime} from '../utils/chat-presentation'
const props=defineProps<{ message: ChatMessage; actions?:boolean;superseded?:Set<string>;status?:string;editable?:boolean;saving?:boolean }>()
const emit=defineEmits<{prompt:[text:string];edit:[text:string];version:[id:string]}>()
const blocks=computed(()=>groupMessageParts(props.message.parts))
const editing=ref(false)
const text=ref('')
const hasActions=computed(()=>props.message.role==='user' && !editing.value && (props.editable || (props.message.versions?.length ?? 0)>1))
function beginEdit() {
  text.value=props.message.parts.filter(part=>part.type==='text').map(part=>part.text).join('\n')
  try {
    const pending=JSON.parse(sessionStorage.getItem(`kodrik.edit.${props.message.id}`)||'null')
    if(typeof pending?.text==='string') text.value=pending.text
  } catch {}
  editing.value=true
}
</script>

<template>
  <UChatMessage :id="message.id" :data-message-id="message.id" :role="message.role" :parts="[]"
    :side="message.role === 'user' ? 'right' : 'left'"
    :variant="message.role === 'user' ? 'soft' : 'naked'"
    :ui="{actions:'gap-1 group-focus-within/message:opacity-100',container:hasActions?'[@media(pointer:coarse)]:pb-12':undefined}"
    class="message min-w-0">
    <template #content>
      <div class="min-w-0 space-y-3">
        <template v-for="(block,index) in blocks" :key="index">
          <template v-if="block.kind==='content'">
            <ChatMarkdown v-if="block.part.type==='text' && block.part.text && !editing" :text="block.part.text" data-search-text />
            <div v-else-if="block.part.type==='file'" class="flex flex-col items-start gap-2">
              <img v-if="block.part.url && block.part.mediaType.startsWith('image/')" :src="block.part.url" :alt="block.part.filename" loading="lazy" class="max-h-72 max-w-full rounded-lg object-contain">
              <UButton :to="block.part.url" :disabled="!block.part.url" external download color="neutral" variant="outline"
                :icon="block.part.mediaType.startsWith('image/') ? 'i-lucide-image' : 'i-lucide-file'"
                :label="block.part.filename" class="max-w-full" :ui="{label:'truncate'}" />
            </div>
          </template>
          <template v-else>
            <ChatToolSteps :tools="block.tools" />
            <template v-for="tool in block.tools" :key="tool.toolCallId">
              <template v-if="tool.toolName==='show_ui' && tool.state==='output-available'">
                <UCollapsible v-if="superseded?.has(tool.toolCallId)" class="space-y-3">
                  <UButton :label="t('Предыдущая версия документа')" icon="i-lucide-history" variant="ghost" color="neutral" trailing-icon="i-lucide-chevron-down" />
                  <template #content>
                    <p class="text-sm text-muted"> {{ t("Ниже в переписке есть более новая версия этого документа.") }} </p>
                    <ChatDocument :output="tool.output" :actions="actions" @prompt="emit('prompt',$event)" />
                  </template>
                </UCollapsible>
                <ChatDocument v-else :output="tool.output" :actions="actions" @prompt="emit('prompt',$event)" />
              </template>
            </template>
          </template>
        </template>
        <p v-if="status" class="text-xs text-muted">{{ status }}</p>
        <div v-if="editing" class="space-y-3 min-w-64">
          <UTextarea v-model="text" :aria-label="t('Редактирование сообщения')" autoresize :rows="3" :maxrows="12" :maxlength="32768" :disabled="saving" class="w-full" autofocus />
          <p class="text-xs text-muted"> {{ t("Новый ответ появится в отдельной версии. Исходная переписка сохранится.") }} </p>
          <div class="flex justify-end gap-2">
            <UButton :label="t('Отмена')" color="neutral" variant="ghost" :disabled="saving" @click="editing=false" />
            <UButton :label="t('Сохранить и отправить')" :loading="saving" :disabled="!editable || (!text.trim() && !message.parts.some(part=>part.type==='file'))" @click="emit('edit',text)" />
          </div>
        </div>
      </div>
    </template>
    <template v-if="message.created_at || hasActions" #actions>
      <template v-if="hasActions && message.versions && message.versions.length>1">
        <UButton icon="i-lucide-chevron-left" :aria-label="t('Предыдущая версия сообщения')" color="neutral" variant="link" size="sm" :disabled="saving || (message.version ?? 0)===0" @click="emit('version',message.versions[(message.version ?? 0)-1]!)" />
        <span class="text-xs text-muted tabular-nums" :aria-label="t('Версия сообщения')">{{ (message.version ?? 0)+1 }} / {{ message.versions.length }}</span>
        <UButton icon="i-lucide-chevron-right" :aria-label="t('Следующая версия сообщения')" color="neutral" variant="link" size="sm" :disabled="saving || (message.version ?? 0)>=message.versions.length-1" @click="emit('version',message.versions[(message.version ?? 0)+1]!)" />
      </template>
      <time v-if="message.created_at" :datetime="message.created_at" class="text-xs text-muted tabular-nums whitespace-nowrap invisible group-hover/message:visible">{{ formatMessageTime(message.created_at) }}</time>
      <UButton v-if="hasActions && editable" icon="i-lucide-pencil" :aria-label="t('Редактировать сообщение')" color="neutral" variant="link" size="sm" @click="beginEdit" />
    </template>
  </UChatMessage>
</template>
