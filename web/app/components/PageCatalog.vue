<script setup lang="ts">
import {locale, t} from '../utils/i18n'
import {base, login, type PublishedPage} from '../utils/api'

const props = defineProps<{selected?: string; revision?: number}>()
const emit = defineEmits<{open: [page: PublishedPage]; delete: [page: PublishedPage]}>()
const pages = ref<PublishedPage[]>([])
const search = ref('')
const loading = ref(false)
const error = ref('')
const expired = ref(false)
const visiblePages = computed(() => {
  const query = search.value.trim().toLowerCase()
  return pages.value.filter(page => `${page.title}\n${page.url}`.toLowerCase().includes(query))
})

async function load() {
  loading.value = true
  error.value = ''
  try {
    const response = await fetch(new URL('api/pages', base), {headers: {'Accept-Language': locale.value}})
    expired.value = response.status === 401
    if (!response.ok) throw new Error(t('Не удалось загрузить страницы.'))
    pages.value = await response.json()
  } catch {
    error.value = t('Не удалось загрузить страницы.')
  } finally { loading.value = false }
}
onMounted(load)
watch(() => props.revision, load)
</script>

<template>
  <div class="flex min-h-0 flex-1 flex-col gap-2">
    <div class="flex items-center gap-1">
      <UInput v-model="search" :aria-label="t('Поиск страниц')" :placeholder="t('Поиск страниц…')" icon="i-lucide-search" size="xl" class="min-w-0 flex-1" />
      <UButton icon="i-lucide-refresh-cw" :aria-label="t('Обновить')" :title="t('Обновить')" color="neutral" variant="ghost" :loading="loading" @click="load" />
    </div>
    <p v-if="loading" role="status" class="text-sm text-muted">{{ t('Загружаю страницы…') }}</p>
    <template v-else-if="error">
      <p role="alert" class="text-sm text-error">{{ t(error) }}</p>
      <UButton :label="t('Повторить загрузку')" color="neutral" variant="link" @click="load" />
      <UButton v-if="expired" :label="t('Войти')" :to="login" external color="neutral" variant="link" />
    </template>
    <template v-else>
      <nav :aria-label="t('Каталог страниц')" class="flex min-h-0 flex-1 flex-col gap-1 overflow-y-auto overscroll-contain">
        <div v-for="page in visiblePages" :key="page.slug" class="page-row relative flex shrink-0 items-center">
        <UButton @click="emit('open', page)"
          :title="page.title" color="neutral" :variant="selected === page.slug ? 'soft' : 'ghost'" :aria-current="selected === page.slug ? 'page' : undefined" class="w-full justify-start pr-11">
          <UIcon name="i-lucide-file-text" class="shrink-0 text-muted" />
          <span class="min-w-0 flex-1 text-left">
            <span class="block truncate">{{ page.title }}</span>
            <span class="block truncate text-xs text-muted">{{ page.slug }}</span>
            <span class="block text-xs text-muted">{{ page.visibility === 'public' ? t('Публичная') : t('Приватная') }}</span>
          </span>
        </UButton>
        <UButton icon="i-lucide-trash-2" :aria-label="t('Удалить страницу «{0}»', page.title)" :title="t('Удалить страницу')" color="error" variant="ghost" size="sm" class="page-actions absolute right-1" @click="emit('delete', page)" />
        </div>
        <p v-if="!visiblePages.length" class="text-sm text-muted">{{ pages.length ? t('Страницы не найдены.') : t('Здесь появятся опубликованные страницы.') }}</p>
      </nav>
    </template>
  </div>
</template>

<style scoped>
@media (hover: hover) and (pointer: fine) {
  .page-actions { opacity: 0; }
  .page-row:hover .page-actions,
  .page-row:focus-within .page-actions { opacity: 1; }
}
</style>
