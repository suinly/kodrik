<script setup lang="ts">
const prefix = useRuntimeConfig().app.baseURL
</script>

<template>
  <span role="img" aria-label="kodrik" class="inline-flex aspect-[26/7] shrink-0">
    <img :src="`${prefix}chat/logo.svg`" alt="" width="208" height="56" class="size-full dark:hidden">
    <img :src="`${prefix}chat/logo-dark.svg`" alt="" width="208" height="56" class="hidden size-full dark:block">
  </span>
</template>
