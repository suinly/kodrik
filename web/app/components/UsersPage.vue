<script setup lang="ts">
import {AuthenticationError,base,checkedFetch} from '../utils/api'
import {locale,t} from '../utils/i18n'

type Usage = {calls:number;input_tokens:number;cached_tokens:number;output_tokens:number;cost_usd:number|null;partial:boolean;tokens_partial:boolean}
type User = {telegram_id:string|null;name:string;owner:boolean;usage:Usage|null}

const users=ref<User[]>([])
const loading=ref(true)
const error=ref('')
const numbers=computed(()=>new Intl.NumberFormat(locale.value))
const usd=computed(()=>new Intl.NumberFormat(locale.value,{style:'currency',currency:'USD',maximumFractionDigits:6}))
const money=(value:number)=>value>0 && value<0.000001 ? `<${usd.value.format(0.000001)}` : usd.value.format(value)
const tokenFields=['input_tokens','cached_tokens','output_tokens'] as const
const missing=computed(()=>users.value.some(user=>user.usage===null))
const total=computed<Usage|null>(()=>{
  const known=users.value.flatMap(user=>user.usage ? [user.usage] : [])
  if(!known.length) return null
  const sum=(key:'calls'|typeof tokenFields[number]|'cost_usd')=>known.reduce((sum,usage)=>sum+(usage[key] ?? 0),0)
  const calls=sum('calls')
  const cost=sum('cost_usd')
  const partial=missing.value || known.some(usage=>usage.partial || usage.cost_usd===null)
  const tokensPartial=missing.value || known.some(usage=>usage.tokens_partial)
  return {
    calls,input_tokens:sum('input_tokens'),cached_tokens:sum('cached_tokens'),output_tokens:sum('output_tokens'),
    cost_usd:cost>0 || (calls===0 && !partial && !tokensPartial) ? cost : null,
    partial,tokens_partial:tokensPartial,
  }
})
const rows=computed(()=>[
  {key:'total',name:t('Итого по списку'),id:null,owner:false,usage:total.value,total:true},
  ...users.value.map((user,index)=>({key:index,name:user.name,id:user.telegram_id,owner:user.owner,usage:user.usage,total:false})),
])

async function load() {
  loading.value=true
  error.value=''
  try {
    const response=await checkedFetch(new URL('api/admin/users',base),{cache:'no-store'})
    const data:{users:User[]}=await response.json()
    users.value=data.users
  } catch (reason) {
    error.value=reason instanceof AuthenticationError ? reason.message : 'Не удалось загрузить список пользователей и статистику.'
  } finally {loading.value=false}
}
onMounted(load)
</script>

<template>
  <div class="mx-auto max-w-6xl space-y-4 p-4 sm:p-6">
    <p class="max-w-prose text-sm text-muted">{{ t('Учтены только сохранённые записи вызовов в личных пространствах. Общие, отозванные и удалённые данные не учитываются. Это не счёт от поставщика.') }}</p>
    <p v-if="loading" role="status" class="text-sm text-muted">{{ t('Загрузка статистики…') }}</p>
    <div v-else-if="error" class="space-y-2">
      <p role="alert" class="text-sm text-error">{{ t(error) }}</p>
      <UButton :label="t('Повторить')" color="neutral" variant="soft" @click="load" />
    </div>
    <p v-else-if="!users.length" class="text-sm text-muted">{{ t('Нет аккаунтов для отображения.') }}</p>
    <template v-else>
      <p class="text-sm text-muted">{{ t('Кэш входит во входящие токены, а не прибавляется к ним.') }}</p>
      <div class="overflow-x-auto rounded-lg border border-default" tabindex="0" role="region" :aria-label="t('Статистика по аккаунтам')">
        <table class="w-full text-sm tabular-nums" :aria-label="t('Статистика по аккаунтам')">
          <thead class="bg-muted text-highlighted">
            <tr>
              <th scope="col" class="px-3 py-2 text-start">{{ t('Аккаунт') }}</th>
              <th v-for="label in ['Вызовы','Входные токены','Кэшированные входные токены','Выходные токены','Стоимость, USD']" :key="label" scope="col" class="px-3 py-2 text-end">{{ t(label) }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="row in rows" :key="row.key" class="border-t border-default align-top" :class="{'bg-elevated/50':row.total}">
              <th scope="row" class="min-w-36 px-3 py-2 text-start">
                <div class="break-words [overflow-wrap:anywhere]">{{ row.name }}</div>
                <UBadge v-if="row.owner" color="neutral" variant="soft" size="sm">{{ t('Владелец') }}</UBadge>
                <div v-if="row.id!==null" class="text-xs font-normal text-muted">{{ t('ID в Telegram') }}: {{ row.id }}</div>
                <div v-if="row.total && missing" class="text-xs font-normal text-muted">{{ t('Не удалось прочитать данные части аккаунтов.') }}</div>
                <div v-if="row.usage?.tokens_partial" class="text-xs font-normal text-muted">{{ t('Данные о токенах неполные.') }}</div>
              </th>
              <template v-if="row.usage">
                <td class="px-3 py-2 text-end">{{ numbers.format(row.usage.calls) }}</td>
                <td v-for="field in tokenFields" :key="field" class="px-3 py-2 text-end">{{ numbers.format(row.usage[field]) }}</td>
                <td class="px-3 py-2 text-end">
                  <template v-if="row.usage.cost_usd!==null">
                    <span class="whitespace-nowrap">{{ money(row.usage.cost_usd) }}</span>
                    <div v-if="row.usage.partial" class="text-xs text-muted">{{ t('Известная часть стоимости') }}</div>
                  </template>
                  <span v-else class="text-muted">{{ t('Нет данных о стоимости') }}</span>
                </td>
              </template>
              <td v-else colspan="5" class="px-3 py-2 text-muted">{{ t('Статистика недоступна') }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </template>
  </div>
</template>
