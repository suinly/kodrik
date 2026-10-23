<script setup lang="ts">
import {locale,t} from '../utils/i18n'
const props=defineProps<{url:string;chatUrl:string;loginUrl:string;embedded?:boolean}>()
const documentBase = computed(() => props.embedded ? new URL('./', props.url).href : undefined)
const document=shallowRef<unknown>(),error=ref(''),loading=ref(false),expired=ref(false)
const saved=ref(false)
async function load(){
  loading.value=true;error.value=''
  try {const response=await fetch(props.url,{headers:{'Accept-Language':locale.value}});expired.value=response.status===401;if(!response.ok) throw new Error(t("Не удалось загрузить страницу."));document.value=await response.json()}
  catch(problem){error.value=problem instanceof Error?problem.message:t("Не удалось загрузить страницу.")}
  finally{loading.value=false}
}
onMounted(load)
</script>

<template>
  <UContainer class="max-w-6xl py-6 space-y-6">
    <header v-if="!embedded" class="flex flex-wrap items-center justify-between gap-3">
      <UButton :to="chatUrl" external :label="t('Открыть чат')" icon="i-lucide-message-circle" color="neutral" variant="ghost" class="-ms-2.5" />
      <div class="flex items-center gap-2"><LanguageSwitch /><UButton :label="t('Обновить')" icon="i-lucide-refresh-cw" variant="ghost" color="neutral" :loading="loading" @click="load" /><ThemeSwitch /></div>
    </header>
    <UAlert v-if="error" color="error" :description="t(error)" :actions="expired?[{label:t('Войти'),to:loginUrl,external:true}]:[]" />
    <UAlert v-if="saved" color="success" :description="t('Запись сохранена.')" role="status" />
    <UiDocument v-if="document" :document="document" :base-url="documentBase" live @refresh="saved=true;load()" />
    <USkeleton v-else-if="loading" class="h-48 w-full" />
  </UContainer>
</template>
