<script setup lang="ts">
import {AuthenticationError,base,checkedFetch} from '../utils/api'
import {locale,t} from '../utils/i18n'
import {accountKey,accountLabel,formatUsd,usageMetrics,type Account,type AccountUsage} from '../utils/users'

type Call = {id:string;model:string;created_at:number|null;cost_usd:number|null;partial:boolean}
type Report = {usage:AccountUsage;calls:Call[];next:string|null;undated_calls:number}

const props=defineProps<{account:Account;onUpdated:(account:Account)=>void}>()
const endpoint=`api/admin/users/${encodeURIComponent(accountKey(props.account))}`
const name=ref(props.account.name)
const saving=ref(false), saveError=ref('')
const unblocked=ref(false)
const start=ref(''), end=ref('')
let range:Record<string,string>={}
const loading=ref(true), error=ref('')
const report=ref<Report|null>(null)
const numbers=computed(()=>new Intl.NumberFormat(locale.value))
const dateTime=computed(()=>new Intl.DateTimeFormat(locale.value,{dateStyle:'medium',timeStyle:'short'}))
const invalidRange=computed(()=>!!(start.value && end.value && dayBoundary(start.value)>dayBoundary(end.value)))
const filtered=ref(false)

function dayBoundary(value:string,nextDay=false) {
  const [year,month,day]=value.split('-').map(Number) as [number,number,number]
  const midnight=new Date(0)
  midnight.setFullYear(year,month-1,day+(nextDay?1:0))
  midnight.setHours(0,0,0,0)
  return midnight.getTime()/1000
}

async function update(change:{name:string}|{blocked:boolean}) {
  if(saving.value || ('blocked' in change && props.account.owner) || ('name' in change && !change.name)) return
  saving.value=true;saveError.value=''
  try {
    const response=await checkedFetch(new URL(endpoint,base),{
      method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(change),
    })
    const account:Account=await response.json()
    props.onUpdated(account)
    if('name' in change) name.value=account.name
    else unblocked.value=!account.blocked
  } catch(reason) {
    saveError.value=reason instanceof AuthenticationError ? reason.message : 'Не удалось обновить аккаунт.'
  } finally {saving.value=false}
}

function toggleBlocked() {
  if(props.account.blocked || window.confirm(t('Заблокировать аккаунт? Веб-сессии и подключения устройств будут отозваны.'))) {
    void update({blocked:!props.account.blocked})
  }
}

async function load(before:string|null=null) {
  loading.value=true;error.value=''
  if(before===null) report.value=null
  const url=new URL(`${endpoint}/usage`,base)
  url.search=new URLSearchParams(range).toString()
  if(before!==null) url.searchParams.set('before',before)
  try {
    const response=await checkedFetch(url,{cache:'no-store'})
    const data:Report=await response.json()
    report.value=before!==null && report.value ? {...report.value,calls:[...report.value.calls,...data.calls],next:data.next} : data
  } catch(reason) {
    error.value=reason instanceof AuthenticationError ? reason.message : 'Не удалось загрузить статистику аккаунта.'
  } finally {loading.value=false}
}

function apply(allTime=false) {
  if(loading.value || (!allTime && invalidRange.value)) return
  if(allTime) {start.value='';end.value=''}
  range={}
  if(start.value) range.from=String(dayBoundary(start.value))
  if(end.value) range.to=String(dayBoundary(end.value,true))
  filtered.value=!!(start.value || end.value)
  void load()
}

onMounted(()=>void load())
</script>

<template>
  <section class="space-y-6">
    <div class="space-y-3">
      <h2 class="text-lg font-semibold break-words text-highlighted">{{ accountLabel(account) }}</h2>
      <div class="flex flex-wrap items-center gap-2 text-sm text-muted">
        <UBadge v-if="account.owner && accountLabel(account)!==t('Владелец')" color="neutral" variant="soft">{{ t('Владелец') }}</UBadge>
        <UBadge v-else-if="account.blocked" color="error" variant="soft">{{ t('Заблокирован') }}</UBadge>
        <span v-if="account.telegram_id!==null">{{ t('ID в Telegram') }}: {{ account.telegram_id }}</span>
      </div>
      <form class="flex flex-wrap items-end gap-2" @submit.prevent="update({name:name.trim()})">
        <UFormField :label="t('Имя')" required>
          <UInput v-model="name" :aria-label="t('Имя')" :maxlength="200" :disabled="saving" required />
        </UFormField>
        <UButton type="submit" :label="t('Сохранить имя')" :disabled="saving || !name.trim() || name.trim()===account.name" />
      </form>
      <UButton v-if="!account.owner" :label="t(account.blocked?'Разблокировать аккаунт':'Заблокировать аккаунт')" :color="account.blocked?'neutral':'error'" variant="soft" :disabled="saving" @click="toggleBlocked" />
      <p v-if="!account.owner && (account.blocked || unblocked)" class="text-sm text-muted" :role="unblocked?'status':undefined">{{ t('После разблокировки пользователь должен снова войти и подключить устройства.') }}</p>
      <p v-if="saveError" role="alert" class="text-sm text-error">{{ t(saveError) }}</p>
    </div>

    <section class="space-y-3" :aria-label="t('Статистика аккаунта')">
      <h3 class="font-semibold text-highlighted">{{ t('Статистика аккаунта') }}</h3>
      <p class="text-sm text-muted">{{ t('Даты и время указаны в часовом поясе браузера.') }}</p>
      <form class="flex flex-wrap items-end gap-2" @submit.prevent="apply()">
        <UFormField :label="t('Дата начала')">
          <UInput v-model="start" type="date" :aria-label="t('Дата начала')" :disabled="loading" />
        </UFormField>
        <UFormField :label="t('Дата окончания (включительно)')">
          <UInput v-model="end" type="date" :aria-label="t('Дата окончания (включительно)')" :disabled="loading" />
        </UFormField>
        <UButton type="submit" :label="t('Применить даты')" :disabled="loading || invalidRange" />
        <UButton :label="t('За всё время')" color="neutral" variant="soft" :disabled="loading || (!filtered && !start && !end)" @click="apply(true)" />
      </form>
      <p v-if="invalidRange" class="text-sm text-error">{{ t('Дата окончания не может быть раньше даты начала.') }}</p>
      <p v-if="report" class="text-sm text-muted">{{ t(filtered?'Статистика за выбранный период':'За всё время') }}</p>
      <dl v-if="report" class="flex flex-wrap gap-x-8 gap-y-3 rounded-lg border border-default p-3 text-sm tabular-nums">
        <div v-for="[key,label] in usageMetrics" :key="key">
          <dt class="text-muted">{{ t(label) }}</dt><dd>{{ numbers.format(report.usage[key]) }}</dd>
        </div>
        <div>
          <dt class="text-muted">{{ t('Стоимость, USD') }}</dt>
          <dd>{{ report.usage.cost_usd===null ? t('Нет данных о стоимости') : formatUsd(report.usage.cost_usd,locale) }}</dd>
          <dd v-if="report.usage.cost_usd!==null && report.usage.partial" class="text-xs text-muted">{{ t('Известная часть стоимости') }}</dd>
        </div>
      </dl>
      <template v-if="report">
        <p v-if="report.usage.tokens_partial" class="text-sm text-muted">{{ t('Данные о токенах неполные.') }}</p>
        <p class="text-sm text-muted">{{ t('Кэш входит во входящие токены, а не прибавляется к ним.') }}</p>
      </template>
    </section>

    <section class="space-y-3" :aria-label="t('История запросов к моделям')">
      <h3 class="font-semibold text-highlighted">{{ t('История запросов к моделям') }}</h3>
      <p v-if="report?.undated_calls" class="text-sm text-muted">{{ t('Вызовы без даты ({0}) отображаются только за всё время.',numbers.format(report.undated_calls)) }}</p>
      <p v-if="report && !report.calls.length" class="text-sm text-muted">{{ t('Нет записей о запросах к моделям.') }}</p>
      <div v-if="report?.calls.length" class="overflow-x-auto rounded-lg border border-default" tabindex="0" role="region" :aria-label="t('История запросов к моделям')">
        <table class="w-full text-sm tabular-nums" :aria-label="t('История запросов к моделям')">
          <thead class="bg-muted text-highlighted">
            <tr>
              <th scope="col" class="px-3 py-2 text-start">{{ t('Модель') }}</th>
              <th scope="col" class="px-3 py-2 text-start">{{ t('Дата и время') }}</th>
              <th scope="col" class="px-3 py-2 text-end">{{ t('Стоимость, USD') }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="call in report.calls" :key="call.id" class="border-t border-default align-top">
              <th scope="row" class="px-3 py-2 text-start [overflow-wrap:anywhere]">{{ call.model }}</th>
              <td class="px-3 py-2">
                <time v-if="call.created_at!==null" :datetime="new Date(call.created_at*1000).toISOString()">{{ dateTime.format(call.created_at*1000) }}</time>
                <span v-else class="text-muted">{{ t('Дата неизвестна') }}</span>
              </td>
              <td class="px-3 py-2 text-end">
                {{ call.cost_usd===null ? t('Нет данных о стоимости') : formatUsd(call.cost_usd,locale) }}
                <div v-if="call.cost_usd!==null && call.partial" class="text-xs text-muted">{{ t('Известная часть стоимости') }}</div>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <p v-if="loading" role="status" class="text-sm text-muted">{{ t('Загрузка статистики…') }}</p>
      <p v-if="error" role="alert" class="text-sm text-error">{{ t(error) }}</p>
      <UButton v-if="error || (report && report.next!==null)" :label="t(error?'Повторить':'Показать ещё')" :disabled="loading" color="neutral" variant="soft" @click="load(report?.next ?? null)" />
    </section>
  </section>
</template>
