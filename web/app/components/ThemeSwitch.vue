<script setup lang="ts">
import {t} from '../utils/i18n'
const colorMode = useColorMode()
const options = [
  ['system', 'Системный', 'i-lucide-monitor'],
  ['light', 'Светлая', 'i-lucide-sun'],
  ['dark', 'Тёмная', 'i-lucide-moon'],
] as const
const items = computed(() => options.map(([value, label, icon]) => ({
  label: t(label), icon, type: 'checkbox' as const,
  checked: colorMode.preference === value,
  onSelect: () => { colorMode.preference = value },
})))
const icon = computed(() => options.find(([value]) => value === colorMode.preference)?.[2] ?? 'i-lucide-monitor')
</script>

<template>
  <UDropdownMenu :items="items">
    <UButton :icon="icon" :aria-label="t('Переключить тему')" :title="t('Переключить тему')" color="neutral" variant="ghost" size="lg" class="size-11 justify-center p-0" />
  </UDropdownMenu>
</template>
