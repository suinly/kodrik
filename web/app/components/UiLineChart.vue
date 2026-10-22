<script setup lang="ts">
import {locale, t} from '../utils/i18n'
import {sourceRows} from '../utils/ui-document'
const props=defineProps<{data:unknown;x:string;y:string;label:string;unit?:string}>()
const points=computed(()=>sourceRows(props.data).filter(row=>typeof row[props.y]==='number').map(row=>({label:String(row[props.x]??''),value:Number(row[props.y])})))
const range=computed(()=>({min:Math.min(...points.value.map(p=>p.value)),max:Math.max(...points.value.map(p=>p.value))}))
const format=(value:number)=>`${new Intl.NumberFormat(locale.value,{maximumFractionDigits:6}).format(value)}${props.unit?` ${props.unit}`:''}`
</script>

<template>
  <UCard class="min-w-0">
    <template #header><h3 class="font-semibold text-highlighted">{{ label }}<span v-if="unit" class="font-normal text-muted"> · {{ unit }}</span></h3></template>
    <figure v-if="points.length" class="space-y-2">
      <UChart type="line" :data="points" index="label" :categories="[{key:'value',label,color:'primary'}]" :aria-label="label" :format="format" :height="260" points />
      <figcaption class="text-sm text-muted">{{ t('{0} точек · {1}–{2}', points.length, format(range.min), format(range.max)) }}</figcaption>
      <UCollapsible><UButton :label="t('Данные графика')" variant="ghost" color="neutral" trailing-icon="i-lucide-chevron-down" /><template #content>
        <ul class="max-h-64 overflow-auto text-sm"><li v-for="(point,index) in points" :key="index" class="flex justify-between gap-4 py-1"><span>{{ point.label }}</span><span class="tabular-nums">{{ format(point.value) }}</span></li></ul>
      </template></UCollapsible>
    </figure>
    <p v-else class="text-sm text-muted"> {{ t("Нет данных для графика.") }} </p>
  </UCard>
</template>
