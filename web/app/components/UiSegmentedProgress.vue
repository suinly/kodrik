<script setup lang="ts">
import type {ProgressProps} from '@nuxt/ui'
import {ProgressRoot} from 'reka-ui'
import {tv} from '@nuxt/ui/utils/tv'
import theme from '#build/ui/progress'

const props=defineProps<{values:Record<string,unknown>}>()
const appConfig=useAppConfig()
const values=computed(()=>({...props.values,
  status:props.values.status===true||props.values.status==='',
  inverted:props.values.inverted===true||props.values.inverted==='',
}) as ProgressProps)
const segments=computed(()=>props.values.segments as {value:number;color:string;label?:string}[])
const max=computed(()=>Number(values.value.max??100))
const total=computed(()=>Math.min(max.value,segments.value.reduce((sum,segment)=>sum+segment.value,0)))
const percent=computed(()=>Math.round(total.value/max.value*100))
const vertical=computed(()=>values.value.orientation==='vertical')
const ui=computed(()=>tv({extend:theme,...appConfig.ui?.progress})({size:values.value.size,orientation:values.value.orientation??'horizontal',inverted:values.value.inverted}))
const description=computed(()=>segments.value.map(segment=>`${segment.label??segment.color}: ${segment.value}`).join('; '))
const attrs=computed(()=>Object.fromEntries(Object.entries(props.values)
  .map(([key,value])=>[key.replace(/([a-z])([A-Z])/g,'$1-$2').toLowerCase(),value])
  .filter(([key])=>['id','title'].includes(String(key))||/^(aria|data)-/.test(String(key)))))
const valueLabel=()=>String(attrs.value['aria-label']??`${percent.value}%`)
</script>

<template>
  <component :is="values.as??'div'" :class="ui.root({class:[values.ui?.root,values.class]})" :data-orientation="values.orientation??'horizontal'">
    <div v-if="values.status||$slots.status" data-slot="status" :class="ui.status({class:values.ui?.status})" :style="{'--percent':`${percent}%`}">
      <slot name="status" :percent="percent">{{ percent }}%</slot>
    </div>
    <ProgressRoot v-bind="attrs" :model-value="total" :max="max" :get-value-label="valueLabel" :aria-valuetext="String(attrs['aria-valuetext']??description)" :class="ui.base({class:[values.ui?.base,'flex']})"
      :style="{flexDirection:vertical?(values.inverted?'column':'column-reverse'):(values.inverted?'row-reverse':'row')}">
      <div v-for="(segment,index) in segments" :key="index" data-slot="segment" aria-hidden="true" :title="`${segment.label??segment.color}: ${segment.value}`"
        :style="{flexShrink:0,[vertical?'height':'width']:`${segment.value/max*100}%`,backgroundColor:`var(--ui-${segment.color==='neutral'?'bg-inverted':segment.color})`}" />
    </ProgressRoot>
  </component>
</template>
