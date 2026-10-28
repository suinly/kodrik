<script setup lang="ts">
import {languagePreference, setLanguage, t} from '../utils/i18n'
import {base} from '../utils/api'

defineProps<{active?:boolean;disabled?:boolean}>()
const emit=defineEmits<{settings:[]}>()
const logout=useTemplateRef('logout')
const logoutUrl=new URL('auth/logout',base).pathname
const colorMode=useColorMode()
const themes=[['system','Системный','i-lucide-monitor'],['light','Светлая','i-lucide-sun'],['dark','Тёмная','i-lucide-moon']] as const
const items=computed(()=>[
  {label:t('Язык'),icon:'i-lucide-languages',children:(['system','en','ru'] as const).map(value=>({
    label:value==='system'?t('Системный'):value==='en'?'English':'Русский',type:'checkbox' as const,
    checked:languagePreference.value===value,onSelect:()=>setLanguage(value),
  }))},
  {label:t('Тема'),icon:themes.find(([value])=>value===colorMode.preference)?.[2],children:themes.map(([value,label,icon])=>({
    label:t(label),icon,type:'checkbox' as const,checked:colorMode.preference===value,
    onSelect:()=>{colorMode.preference=value},
  }))},
  {label:t('Настройки профиля'),icon:'i-lucide-user-round',onSelect:()=>emit('settings')},
  {type:'separator' as const},
  {label:t('Выйти'),icon:'i-lucide-log-out',onSelect:()=>logout.value?.requestSubmit()},
])
</script>

<template>
  <UDropdownMenu :items="items" :modal="false" :content="{side:'right',align:'end'}" :ui="{content:'w-56 max-w-(--reka-dropdown-menu-content-available-width)',item:'min-h-11'}">
    <UButton icon="i-lucide-user-round" :aria-label="t('Профиль')" :title="t('Профиль')" :color="active?'primary':'neutral'" :variant="active?'soft':'ghost'"
      :aria-current="active?'page':undefined" :disabled="disabled" size="lg" class="size-11 justify-center p-0" />
  </UDropdownMenu>
  <form ref="logout" :action="logoutUrl" method="post" hidden />
</template>
