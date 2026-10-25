<script setup lang="ts">
import {t} from '../utils/i18n'
import type { AuthFormField } from '@nuxt/ui'

defineProps<{ returnTo: string; error: string }>()
useHead(() => ({ title: t("Вход · Кодрик") }))
const shortInvite = window.location.pathname.endsWith('/i')
const action = shortInvite ? window.location.pathname.replace(/\/i$/, '/auth/login') : window.location.pathname
const fragment = new URLSearchParams(window.location.hash.slice(1))
const invite = shortInvite && window.location.hash ? window.location.hash.slice(1) : fragment.has('invite') ? fragment.get('invite')! : undefined
if (invite !== undefined) {
  const historyState = { ...window.history.state }
  if (typeof historyState.current === 'string') historyState.current = historyState.current.split('#')[0]
  window.history.replaceState(historyState, '', window.location.pathname + window.location.search)
}
const submitting = ref(false)
const passwordChanged = new URLSearchParams(window.location.search).get('password_changed')==='1'
const telegram = ref(false)
const providers = computed(() => [{ label: t("Войти через Telegram"), icon: 'i-lucide-send', type: 'button' as const, onClick: () => { telegram.value = true } }])
const fields = computed<AuthFormField[]>(() => [{
  name: 'password', type: 'password', label: t("Пароль"),
  autocomplete: 'current-password', required: true, autofocus: true, maxlength: 1024, size: 'xl',
}])

function submit(event: SubmitEvent) {
  const form = event.target as HTMLFormElement
  submitting.value = true
  // Keep the existing native POST, cookie and redirect flow.
  form.submit()
}
</script>

<template>
  <main class="app-safe-area flex min-h-dvh items-center justify-center p-6">
    <UPageCard class="w-full max-w-sm">
      <div class="flex justify-end"><LanguageSwitch /></div>
      <KodrikLogo class="mx-auto h-8" />
      <TelegramLogin v-if="telegram" :return-to="returnTo" :invite="invite" @cancel="telegram = false" />
      <section v-else-if="invite !== undefined" class="space-y-5 text-center" aria-labelledby="invite-title">
        <h1 id="invite-title" class="text-2xl font-semibold"> {{ t("Приглашение в Кодрик") }} </h1>
        <p class="text-sm text-muted"> {{ t("Подтвердите свой Telegram, чтобы принять приглашение и начать общаться с агентом.") }} </p>
        <UButton block size="xl" icon="i-lucide-send" @click="telegram = true"> {{ t("Войти через Telegram") }} </UButton>
      </section>
      <!-- Keep the button still during a tap: validation on blur can move this centered form. -->
      <UAuthForm v-else :fields="fields" :providers="providers" :separator="t('или')"
        :description="t('Для первого входа через Telegram нужно приглашение владельца. Если доступ уже открыт, войдите в свой аккаунт.')"
        :submit="{ label: t('Войти'), icon: 'i-lucide-log-in', size: 'xl' }" :validate-on="[]"
        :validate="state => state.password ? [] : [{ name: 'password', message: t('Введите пароль.') }]"
        :action="action" method="post" :loading="submitting" :loading-auto="false" @submit="submit">
        <template #title><h1> {{ t("Вход в Кодрик") }} </h1></template>
        <template #validation>
          <input type="hidden" name="return_to" :value="returnTo">
          <UAlert v-if="error" role="alert" color="error" variant="soft" :description="t(error)" />
          <UAlert v-else-if="passwordChanged" role="status" color="success" variant="soft" :description="t('Пароль изменён. Войдите снова.')" />
        </template>
      </UAuthForm>
    </UPageCard>
  </main>
</template>
