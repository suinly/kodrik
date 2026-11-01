<script setup lang="ts">
import type {CommandAuditMode} from '../utils/api'
import {t} from '../utils/i18n'

const props=defineProps<{mode:CommandAuditMode;disabled?:boolean;loading?:boolean;profile?:boolean}>()
const emit=defineEmits<{select:[mode:CommandAuditMode]}>()
const commandAuditModes = [
  {value:'trusted',label:'Без проверок',icon:'i-lucide-shield-off',description:'Команды выполняются без проверки и подтверждения.'},
  {value:'review',label:'Проверка ИИ',icon:'i-lucide-shield-check',description:'ИИ проверяет команды и запрашивает разрешение для рискованных или неясных действий.'},
  {value:'ask',label:'Спрашивать всегда',icon:'i-lucide-hand',description:'Каждая команда требует вашего разрешения. Проверка ИИ не используется.'},
] as const
const current=computed(()=>commandAuditModes.find(item=>item.value===props.mode) ?? commandAuditModes[0])
const items=computed(()=>commandAuditModes.map(item=>({
  label:t(item.label),description:t(item.description),icon:item.icon,type:'checkbox' as const,checked:props.mode===item.value,
  onSelect:()=>emit('select',item.value),
})))
</script>

<template>
  <UDropdownMenu :items="items" :disabled="disabled" :content="{align:'end'}" :ui="{content:'w-72 max-w-[calc(100vw-1rem)]',itemDescription:'whitespace-normal'}">
    <UButton :icon="current.icon" :label="t(current.label)" :aria-label="t(profile ? 'Режим новых диалогов: {0}' : 'Проверка команд: {0}',t(current.label))"
      :disabled="disabled" :loading="loading" trailing-icon="i-lucide-chevron-down" color="neutral" variant="ghost" size="sm" />
  </UDropdownMenu>
</template>
