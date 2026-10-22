<script setup lang="ts">
import {languagePreference, setLanguage, t} from '../utils/i18n'
const items = computed(() => (['system', 'en', 'ru'] as const).map(value => ({
  label: value === 'system' ? t('Системный') : value === 'en' ? 'English' : 'Русский',
  type: 'checkbox' as const,
  checked: languagePreference.value === value,
  onSelect: () => setLanguage(value),
})))
</script>

<template>
  <UDropdownMenu :items="items">
    <UButton icon="i-lucide-languages" :aria-label="t('Язык')" :title="t('Язык')" color="neutral" variant="ghost" size="lg" />
  </UDropdownMenu>
</template>
