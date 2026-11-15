<script setup lang="ts">
import {configLabel,type ConfigField,type ConfigValue,type ConfigValues} from '../utils/configForm'
import {t} from '../utils/i18n'
const props=defineProps<{schema:ConfigField;modelValue:ConfigValue|undefined;path:string;label?:string;disabled?:boolean;showKeys?:boolean}>()
const emit=defineEmits<{'update:modelValue':[value:ConfigValue]}>()
const name=ref('')
const openEntries=ref<Record<string,boolean>>({})
const fieldLabel=computed(()=>props.label ?? configLabel(props.path.split('.').at(-1) ?? ''))
const entries=computed(()=>props.modelValue && typeof props.modelValue==='object' && !Array.isArray(props.modelValue)?props.modelValue as ConfigValues:{})
const items=computed(()=>Array.isArray(props.modelValue)?props.modelValue:[])
const commonFields=computed(()=>Object.entries(props.schema.fields??{}).filter(([,field])=>!field.advanced))
const advancedFields=computed(()=>Object.entries(props.schema.fields??{}).filter(([,field])=>field.advanced))
const description=computed(()=>props.schema.description?t(props.schema.description):props.schema.optional?t('Необязательное поле; пустое значение использует поведение по умолчанию.'):undefined)
const childPath=(key:string)=>props.path?`${props.path}.${key}`:key
function update(key:string,value:ConfigValue) {emit('update:modelValue',{...entries.value,[key]:value})}
function remove(key:string) {
  const copy={...entries.value};delete copy[key];emit('update:modelValue',copy)
  delete openEntries.value[key]
}
const validName=computed(()=>name.value.trim().length>0 && !Object.hasOwn(entries.value,name.value.trim()) && !['__proto__','prototype','constructor'].includes(name.value.trim()))
function add() {
  if(!validName.value) return
  const key=name.value.trim()
  openEntries.value[key]=true
  update(key,JSON.parse(JSON.stringify(props.schema.initial??'')));name.value=''
}
function scalar(value:string|number) {
  emit('update:modelValue',value==='' && props.schema.optional?null:props.schema.kind==='number'?Number(value):String(value))
}
</script>

<template>
  <fieldset v-if="schema.kind==='object' || schema.kind==='map'" :disabled="disabled" class="min-w-0 space-y-3 rounded-lg border border-default p-4">
    <legend v-if="path" class="px-1 text-sm font-semibold">{{ fieldLabel }}</legend>
    <span v-if="showKeys && path" class="block text-xs text-muted font-mono">{{ path }}</span>
    <p v-if="description" class="text-sm text-muted">{{ description }}</p>
    <UCheckbox v-if="schema.optional" :model-value="modelValue!=null" :label="t('Включить {0}',fieldLabel)" :disabled="disabled" @update:model-value="emit('update:modelValue',$event?{}:null)" />
    <template v-if="schema.kind==='object'">
      <ConfigFieldEditor v-for="[key,field] in commonFields" :key="key" :schema="field" :path="childPath(key)" :model-value="entries[key]" :show-keys="showKeys" :disabled="disabled || (schema.optional && modelValue==null)" @update:model-value="update(key,$event)" />
      <details v-if="advancedFields.length" class="rounded-lg border border-default p-3">
        <summary class="cursor-pointer text-sm font-medium">{{ t('Дополнительные параметры') }}</summary>
        <div class="mt-3 space-y-3">
          <ConfigFieldEditor v-for="[key,field] in advancedFields" :key="key" :schema="field" :path="childPath(key)" :model-value="entries[key]" :show-keys="showKeys" :disabled="disabled || (schema.optional && modelValue==null)" @update:model-value="update(key,$event)" />
        </div>
      </details>
    </template>
    <template v-else>
      <div v-for="(value,key) in entries" :key="key" :class="schema.item?.kind==='object'?'relative rounded-lg border border-default':'flex items-start gap-2'">
        <details v-if="schema.item?.kind==='object'" :open="openEntries[String(key)]??false" class="p-3" @toggle="openEntries[String(key)]=($event.target as HTMLDetailsElement).open">
          <summary class="cursor-pointer break-all pr-28 text-sm font-medium">{{ key }}</summary>
          <ConfigFieldEditor :schema="schema.item" :label="String(key)" :path="childPath(String(key))" :model-value="value" :show-keys="showKeys" :disabled="disabled" class="mt-3" @update:model-value="update(String(key),$event)" />
        </details>
        <ConfigFieldEditor v-else :schema="schema.item!" :label="String(key)" :path="childPath(String(key))" :model-value="value" :show-keys="showKeys" :disabled="disabled" class="min-w-0 grow" @update:model-value="update(String(key),$event)" />
        <UButton :label="t('Удалить {0}',String(key))" :class="schema.item?.kind==='object'?'absolute right-2 top-2 max-w-28':undefined" color="error" variant="ghost" :disabled="disabled" @click="remove(String(key))" />
      </div>
      <div class="flex flex-wrap gap-2">
        <UInput v-model="name" :aria-label="t('{0} имя',path)" :placeholder="t('Имя')" autocomplete="off" :disabled="disabled" />
        <UButton :label="t('Добавить {0}',fieldLabel)" variant="soft" :disabled="disabled || !validName" @click="add" />
      </div>
    </template>
  </fieldset>
  <div v-else-if="schema.kind==='array'" class="space-y-2">
    <p class="text-sm font-medium">{{ fieldLabel }}</p>
    <span v-if="showKeys" class="block text-xs text-muted font-mono">{{ path }}</span>
    <p v-if="description" class="text-sm text-muted">{{ description }}</p>
    <div v-for="(item,index) in items" :key="index" class="flex items-center gap-2">
      <ConfigFieldEditor :schema="schema.item!" :path="childPath(String(index))" :model-value="item" :show-keys="showKeys" :disabled="disabled" class="grow" @update:model-value="emit('update:modelValue',items.map((v,i)=>i===index?$event:v))" />
      <UButton :label="t('Удалить {0}',String(index+1))" color="error" variant="ghost" :disabled="disabled" @click="emit('update:modelValue',items.filter((_,i)=>i!==index))" />
    </div>
    <UButton :label="t('Добавить {0}',fieldLabel)" variant="soft" :disabled="disabled" @click="emit('update:modelValue',[...items,''])" />
  </div>
  <div v-else-if="schema.kind==='boolean'" class="space-y-1">
    <span v-if="showKeys" class="block text-xs text-muted font-mono">{{ path }}</span>
    <p v-if="description" class="text-sm text-muted">{{ description }}</p>
    <UCheckbox v-if="schema.optional" :model-value="modelValue!=null" :label="t('Включить {0}',fieldLabel)" :disabled="disabled" @update:model-value="emit('update:modelValue',$event?false:null)" />
    <UCheckbox :model-value="modelValue===true" :label="fieldLabel" :disabled="disabled || (schema.optional && modelValue==null)" @update:model-value="emit('update:modelValue',$event===true)" />
  </div>
  <UFormField v-else :label="fieldLabel" :description="description">
    <span v-if="showKeys" class="block text-xs text-muted font-mono">{{ path }}</span>
    <UTextarea v-if="path==='system_prompt'" :model-value="modelValue==null?'':String(modelValue)" :aria-label="path" :disabled="disabled" :rows="5" autocomplete="off" class="w-full" @update:model-value="scalar" />
    <UInput v-else :model-value="modelValue==null?'':String(modelValue)" :aria-label="path" :type="schema.secret?'password':schema.kind==='number'?'number':'text'" :min="schema.min" :max="schema.max" :step="schema.kind==='number'?1:undefined" :disabled="disabled" :required="!schema.optional" :readonly="path.endsWith('.type')" :autocomplete="schema.secret?'new-password':'off'" :spellcheck="false" class="w-full" @update:model-value="scalar" />
  </UFormField>
</template>
