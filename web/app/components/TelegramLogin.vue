<script setup lang="ts">
import {locale,t} from '../utils/i18n'
import { renderSVG } from 'uqr'

const props = defineProps<{ returnTo: string; invite?: string }>()
const emit = defineEmits<{ cancel: [] }>()
const endpoint = new URL(window.location.pathname.endsWith('/i') ? 'auth/telegram/' : 'telegram/', window.location.href)
const state = ref<'starting' | 'pending' | 'denied' | 'expired' | 'error'>('starting')
const link = ref('')
const verification = ref('')
const error = ref('')
const remaining = ref(300)
const qr = computed(() => link.value ? `data:image/svg+xml,${encodeURIComponent(renderSVG(link.value, { border: 4, ecc: 'M' }))}` : '')
let deadline = 0
let timer: ReturnType<typeof setTimeout> | undefined
let disposed = false
let polling = false
const canceling = ref(false)
const controller = new AbortController()

async function request(path: string, body?: object) {
  const response = await fetch(new URL(path, endpoint), {
    method: 'POST', credentials: 'same-origin', cache: 'no-store', signal: controller.signal,
    headers: {'Accept-Language':locale.value,...(body?{'Content-Type':'application/json'}:{})},
    ...(body ? { body: JSON.stringify(body) } : {}),
  })
  if (!response.ok) {
    const result = await response.json().catch(() => ({}))
    throw new Error(result.error ? t(result.error) : (response.status === 429 ? t("Слишком много попыток. Подождите минуту.") : t("Не удалось связаться с Кодриком. Попробуйте ещё раз.")))
  }
  return response.status === 204 ? null : response.json()
}

async function poll() {
  if (disposed || polling || state.value !== 'pending') return
  clearTimeout(timer)
  polling = true
  try {
    const result = await request('poll')
    if (disposed || canceling.value) return
    remaining.value = Math.max(0, Math.ceil((deadline - Date.now()) / 1000))
    if (result.status === 'approved') {
      const redirect = new URL(result.redirect, window.location.href)
      if (redirect.origin !== window.location.origin) throw new Error(t("Некорректный адрес перехода."))
      window.location.assign(redirect.href)
    } else if (result.status === 'denied') state.value = 'denied'
    else if (result.status === 'expired' || remaining.value === 0) state.value = 'expired'
    else timer = setTimeout(poll, 2000)
  } catch (cause) {
    if (!disposed) { state.value = 'error'; error.value = (cause as Error).message }
  } finally { polling = false }
}

async function start() {
  clearTimeout(timer)
  state.value = 'starting'
  error.value = ''
  try {
    const result = await request('start', { return_to: props.returnTo, ...(props.invite !== undefined ? { invite: props.invite } : {}) })
    if (disposed) return
    link.value = result.url
    verification.value = result.verification
    deadline = Date.now() + result.expiresIn * 1000
    remaining.value = result.expiresIn
    state.value = 'pending'
    timer = setTimeout(poll, 1500)
  } catch (cause) {
    if (!disposed) { state.value = 'error'; error.value = (cause as Error).message }
  }
}

async function cancel() {
  canceling.value = true
  clearTimeout(timer)
  try { await request('cancel') } catch { /* The request expires even if the connection is lost. */ }
  emit('cancel')
}

function visible() { if (document.visibilityState === 'visible') void poll() }
onMounted(() => { document.addEventListener('visibilitychange', visible); void start() })
onBeforeUnmount(() => { disposed = true; clearTimeout(timer); controller.abort(); document.removeEventListener('visibilitychange', visible) })
</script>

<template>
  <section class="space-y-5 text-center" aria-labelledby="telegram-login-title">
    <UIcon name="i-lucide-send" class="mx-auto size-10 text-primary" />
    <h1 id="telegram-login-title" class="text-2xl font-semibold"> {{ t("Вход через Telegram") }} </h1>
    <div v-if="state === 'starting'" role="status" class="flex justify-center gap-2 text-muted">
      <UIcon name="i-lucide-loader-circle" class="size-5 animate-spin" /> {{ t("Подготавливаем вход…") }} </div>
    <template v-else-if="state === 'pending'">
      <p class="text-sm text-muted"> {{ t("Откройте бота и подтвердите вход. Эта вкладка войдёт автоматически.") }} </p>
      <UButton :to="link" target="_blank" rel="noopener noreferrer" block size="xl" icon="i-lucide-external-link"> {{ t("Открыть Telegram") }} </UButton>
      <USeparator :label="t('или сканируйте с телефона')" />
      <img :src="qr" :alt="t('QR-код для входа через Telegram')" width="208" height="208" class="mx-auto rounded-lg bg-white">
      <p class="text-sm text-muted"> {{ t("Код запроса:") }} <strong class="font-mono text-base tracking-widest text-highlighted" data-login-verification>{{ verification }}</strong></p>
      <p class="text-xs text-muted"> {{ t("Сверьте этот код в боте. Осталось") }} {{ Math.floor(remaining / 60) }}:{{ String(remaining % 60).padStart(2, '0') }}</p>
    </template>
    <template v-else>
      <UAlert role="alert" :color="state === 'denied' ? 'warning' : 'error'" variant="soft"
        :description="state === 'denied' ? (invite !== undefined ? t('Вход отклонён или приглашение больше не действует. Попросите владельца прислать новое приглашение.') : t('Вход отклонён. Для первого входа нужно приглашение владельца.')) : state === 'expired' ? t('Время запроса истекло. Начните вход заново.') : t(error)" />
      <UButton block size="xl" icon="i-lucide-refresh-cw" @click="start"> {{ t("Попробовать снова") }} </UButton>
    </template>
    <UButton variant="link" color="neutral" :disabled="state === 'starting' || canceling" @click="cancel">{{ invite !== undefined ? t("Отменить") : t("Другой способ входа") }}</UButton>
  </section>
</template>
