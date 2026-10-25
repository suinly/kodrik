<script setup lang="ts">
defineProps<{title: string; label: string; placeholder: string; loading?: boolean}>()
const query = defineModel<string>({required: true})
const open = ref(!!query.value)
const input = useTemplateRef('input')
const button = useTemplateRef('button')
const id = useId()
watch(query, value => { if (value) open.value = true })

async function toggle() {
  if (open.value) { close(); return }
  open.value = true
  await nextTick()
  input.value?.inputRef?.focus({preventScroll: true})
}
function close() {
  open.value = false
  query.value = ''
  button.value?.$el.focus({preventScroll: true})
}
</script>

<template>
  <div class="sidebar-section-header">
    <h2 class="min-w-0 flex-1 text-sm font-semibold text-highlighted"><slot name="title">{{ title }}</slot></h2>
    <UButton ref="button" :aria-label="label" :title="label" :aria-expanded="open" :aria-controls="id" icon="i-lucide-search" color="neutral" :variant="open ? 'soft' : 'ghost'" size="sm" class="sidebar-action" @click="toggle" />
    <slot />
  </div>
  <UInput v-show="open" :id="id" ref="input" v-model="query" :aria-label="label" :placeholder="placeholder" :loading="loading" icon="i-lucide-search" size="lg" class="w-full" :ui="{base:'text-base'}" @keydown.esc.stop.prevent="close" />
</template>
