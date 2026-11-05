<script setup lang="ts">
import {base,checkedFetch,messageOf,type RegisteredDevice} from '../utils/api'
import {t,locale} from '../utils/i18n'

const devices=ref<RegisteredDevice[]>([])
const loading=ref(false),working=ref(false),loaded=ref(false)
const problem=ref(''),approved=ref(false)
const requestId=ref(''),code=ref(''),compared=ref(false)
const valid=computed(()=>/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(requestId.value.trim()) && !!code.value.trim() && compared.value)
watch([requestId,code],()=>{compared.value=false;approved.value=false},{flush:'sync'})

async function load() {
  if(loading.value) return
  loading.value=true;problem.value=''
  try {devices.value=await(await checkedFetch(new URL('api/devices',base))).json();loaded.value=true}
  catch(error) {problem.value=messageOf(error)}
  finally {loading.value=false}
}

async function approve() {
  if(!valid.value || working.value || loading.value) return
  working.value=true;problem.value='';approved.value=false
  try {
    await checkedFetch(new URL('api/devices/pair/approve',base),{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify({request_id:requestId.value.trim(),code:code.value.trim()})})
    requestId.value='';code.value='';compared.value=false;approved.value=true
    await load()
  } catch(error) {problem.value=messageOf(error)}
  finally {working.value=false}
}

async function revoke(id:string) {
  if(working.value || loading.value) return
  working.value=true;problem.value='';approved.value=false
  try {
    await checkedFetch(new URL(`api/devices/${encodeURIComponent(id)}`,base),{method:'DELETE'})
    devices.value=devices.value.filter(device=>device.id!==id)
  } catch(error) {problem.value=messageOf(error)}
  finally {working.value=false}
}

onMounted(()=>{void load()})
</script>

<template>
  <section :aria-label="t('Подключения устройств')" class="space-y-4">
    <div class="flex flex-wrap items-center justify-between gap-2">
      <h2 class="text-lg font-semibold text-highlighted">{{ t('Подключения устройств') }}</h2>
      <UButton :label="t('Обновить')" color="neutral" variant="soft" :loading="loading" :disabled="working" @click="load" />
    </div>
    <p class="text-sm text-muted">{{ t('Сверьте UUID и код с терминалом на вашем устройстве. Подтверждайте только запрос, который вы начали сами.') }}</p>
    <form class="space-y-3" @submit.prevent="approve">
      <UFormField :label="t('UUID запроса подключения')" required>
        <UInput v-model="requestId" :aria-label="t('UUID запроса подключения')" autocomplete="off" :spellcheck="false" :maxlength="36" :disabled="working" required class="w-full" />
      </UFormField>
      <UFormField :label="t('Код сравнения')" required>
        <UInput v-model="code" :aria-label="t('Код сравнения')" autocomplete="off" :spellcheck="false" :maxlength="128" :disabled="working" required class="w-full" />
      </UFormField>
      <UCheckbox v-model="compared" :label="t('Я сверил UUID и код на своём устройстве')" :disabled="working" />
      <UButton type="submit" :label="t('Подтвердить подключение')" :disabled="!valid || loading" :loading="working" />
    </form>
    <p v-if="loading" role="status" class="text-sm text-muted">{{ t('Загружаю устройства…') }}</p>
    <p v-if="problem" role="alert" class="text-sm text-error break-words">{{ t(problem) }}</p>
    <p v-if="approved" role="status" class="text-sm text-success">{{ t('Подключение подтверждено.') }}</p>
    <ul class="space-y-3">
      <li v-for="device in devices" :key="device.id" class="flex flex-wrap items-center justify-between gap-3">
        <div class="min-w-0 text-sm break-words">
          <p>{{ device.label }}</p>
          <p class="text-muted">{{ t('Доступ до {0}',new Date(device.expires_at*1000).toLocaleString(locale)) }}</p>
        </div>
        <UButton :label="t('Отозвать доступ')" color="error" variant="soft" :disabled="working || loading" @click="revoke(device.id)" />
      </li>
    </ul>
    <p v-if="loaded && !loading && !devices.length" class="text-sm text-muted">{{ t('Нет зарегистрированных устройств.') }}</p>
    <p class="text-sm text-muted">{{ t('Закрытие этой страницы не отзывает доступ устройства. Доступ к среде заканчивается при закрытии TUI.') }}</p>
  </section>
</template>
