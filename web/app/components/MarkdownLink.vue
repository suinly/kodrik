<script setup lang="ts">
import {linkTarget} from '../utils/link-target'
defineProps<{href?:string}>()
</script>

<template>
  <ProseA :href="href" :target="linkTarget(href)" external><slot /></ProseA>
</template>
