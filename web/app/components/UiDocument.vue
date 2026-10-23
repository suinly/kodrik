<script setup lang="ts">
import {t} from '../utils/i18n'
import {parseUiDocument,type UiDocument,type UiNode} from '../utils/ui-document'
import {prepareUiComponents} from '../utils/ui-registry'
const props=defineProps<{document:unknown;live?:boolean;actions?:boolean;baseUrl?:string}>()
const emit=defineEmits<{prompt:[text:string];refresh:[]}>()
const parsed=shallowRef<{document:UiDocument;nodes:UiNode[]}>()
const error=ref('')
onErrorCaptured(problem=>{console.error('Failed to render UI document',problem);error.value=problem.message;return false})
watch(()=>props.document,async(raw,_,cleanup)=>{
  let stale=false;cleanup(()=>{stale=true});parsed.value=undefined;error.value=''
  try {const next=await parseUiDocument(raw);await prepareUiComponents(next.nodes);if(!stale) parsed.value=next}
  catch(problem){if(!stale) error.value=problem instanceof Error?problem.message:t("Не удалось прочитать документ.")}
},{immediate:true})
</script>

<template>
  <section class="min-w-0 space-y-4" :aria-label="parsed?.document.title || t('Документ')" data-testid="ui-document">
    <div v-if="error" role="alert" class="space-y-1 text-sm text-error break-words">
      <p> {{ t("Не удалось отобразить документ") }} </p>
      <p>{{ t(error) }}</p>
    </div>
    <template v-else-if="parsed">
      <UiDocumentNode v-for="(node,index) in parsed.nodes" :key="index" :node="node" :sources="parsed.document.sources" :views="parsed.document.views" :document-actions="parsed.document.actions" :live="live" :actions="actions" :base-url="baseUrl" @prompt="emit('prompt',$event)" @refresh="emit('refresh')" />
    </template>
    <USkeleton v-else class="h-24 w-full" />
  </section>
</template>
