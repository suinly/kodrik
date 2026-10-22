<script setup lang="ts">
import {locale,formatUiValue,t} from '../utils/i18n'
import {h} from 'vue'
import {UButton} from '#components'
import type {TableColumn} from '@nuxt/ui'
import {sourceRows,type Row,type UiSource} from '../utils/ui-document'
import {prepareUiTableColumns} from '../utils/ui-table'
const props=defineProps<{source:UiSource;live?:boolean;title?:string;columns?:unknown[]}>()
const filter=ref(''),problem=ref(''),loading=ref(false)
const current=shallowRef(props.source.data)
const cursors=ref<(string|null)[]>([null]),page=ref(0)
watch(()=>props.source,source=>{current.value=source.data;cursors.value=[null];page.value=0;problem.value='';filter.value=''})
const rows=computed(()=>sourceRows(current.value))
const next=computed(()=>{const data=current.value as {next_cursor?:string};return data?.next_cursor || null})
const columns=computed<TableColumn<Row>[]>(()=>{
  const definitions:TableColumn<Row>[]=props.columns===undefined?[...new Set(rows.value.flatMap(row=>Object.keys(row)))].map(key=>({id:key,accessorFn:(row:Row)=>Object.hasOwn(row,key)?row[key]:undefined,header:key})):prepareUiTableColumns(props.columns,true)
  return definitions.map(definition=>({
  ...definition,id:definition.id!,header:({column})=>h(UButton,{color:'neutral',variant:'ghost',label:String(definition.header),
    trailingIcon:column.getIsSorted()==='asc'?'i-lucide-arrow-up':column.getIsSorted()==='desc'?'i-lucide-arrow-down':'i-lucide-arrow-up-down',
    onClick:()=>column.toggleSorting(column.getIsSorted()==='asc')}),cell:definition.cell??(({getValue})=>formatUiValue(getValue())),
  }))
})
async function load(index:number,cursor:string|null) {
  if(!props.live || !props.source.url || loading.value) return
  loading.value=true;problem.value=''
  try {
    const url=new URL(props.source.url,window.location.href)
    if(url.origin!==location.origin) throw new Error(t("Неверный адрес источника."))
    if(cursor) url.searchParams.set('cursor',cursor)
    const response=await fetch(url,{headers:{'Accept-Language':locale.value}})
    const body=await response.json()
    if(!response.ok) throw new Error(body.error ? t(body.error) : t("Не удалось загрузить данные ({0}).", response.status))
    sourceRows(body)
    current.value=body;cursors.value[index]=cursor;page.value=index;filter.value=''
  } catch(error){problem.value=error instanceof Error?error.message:t("Не удалось загрузить данные.")}
  finally{loading.value=false}
}
</script>

<template>
  <UCard class="min-w-0">
    <template #header><div class="flex flex-wrap items-center justify-between gap-3">
      <h3 v-if="title" class="font-semibold text-highlighted">{{ title }}</h3>
      <UInput v-model="filter" :aria-label="t('Фильтр таблицы')" :placeholder="t('Найти на странице…')" icon="i-lucide-search" class="w-full sm:w-64" />
    </div></template>
    <UTable v-model:global-filter="filter" :data="rows" :columns="columns" :loading="loading" class="w-full" />
    <template v-if="next || page || problem" #footer>
      <div class="flex flex-wrap items-center justify-between gap-2">
        <p class="text-sm text-muted">{{ live && source.url ? t("Страница {0}", page+1) : t("Показан сохранённый срез данных") }}</p>
        <div v-if="live && source.url" class="flex gap-2">
          <UButton :label="t('Назад')" color="neutral" variant="outline" :disabled="page===0 || loading" @click="load(page-1,cursors[page-1] ?? null)" />
          <UButton :label="t('Далее')" variant="outline" :disabled="!next || loading" @click="load(page+1,next)" />
        </div>
      </div>
      <UAlert v-if="problem" color="error" :description="t(problem)" class="mt-3" :actions="[{label:t('Начать заново'),onClick:()=>load(0,null)}]" />
    </template>
  </UCard>
</template>
