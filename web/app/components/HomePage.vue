<script setup lang="ts">
import {t} from '../utils/i18n'
defineProps<{ prefix: string; loggedIn: boolean }>()
useHead(() => ({ title: t("Кодрик — личный AI-компаньон") }))
</script>

<template>
  <UContainer class="flex min-h-dvh max-w-5xl flex-col">
    <header class="flex flex-wrap items-center justify-between gap-4 border-b border-default py-5">
      <KodrikLogo class="h-6" />
      <nav :aria-label="t('Аккаунт и оформление')" class="flex items-center gap-2">
        <LanguageSwitch />
        <ThemeSwitch />
        <form v-if="loggedIn" :action="`${prefix}auth/logout`" method="post">
          <UButton type="submit" :aria-label="t('Выйти')" :title="t('Выйти')" icon="i-lucide-log-out" color="neutral" variant="ghost" size="sm" />
        </form>
        <UButton v-else :to="`${prefix}auth/login`" external :label="t('Войти')" icon="i-lucide-log-in" color="neutral" variant="ghost" size="lg" />
      </nav>
    </header>

    <main class="flex flex-1 flex-col justify-center py-16 sm:py-24">
      <section aria-labelledby="home-title" class="max-w-2xl">
        <p class="mb-5 flex items-center gap-2 text-sm font-medium text-primary">
          <UIcon name="i-lucide-orbit" class="size-4" /> {{ t("Личный AI-компаньон") }} </p>
        <h1 id="home-title" class="text-4xl leading-tight font-semibold tracking-tight text-highlighted text-balance sm:text-6xl"> {{ t("Для идей, вопросов и повседневных дел.") }} </h1>
        <p class="mt-6 max-w-xl text-lg leading-relaxed text-muted text-pretty"> {{ t("Обсуждайте то, что вам интересно, разбирайтесь в сложном и решайте задачи. Кодрик помнит ваши предпочтения и сохраняет историю разговоров.") }} </p>
        <UButton :to="`${prefix}app/chat`" external :label="t('Открыть чат')" trailing-icon="i-lucide-arrow-right" size="xl" class="mt-8" />
      </section>

      <section aria-labelledby="pages-title" class="mt-14 flex max-w-xl items-start gap-4 border-t border-default pt-6 sm:mt-20">
        <UIcon name="i-lucide-panels-top-left" class="mt-0.5 size-5 shrink-0 text-muted" />
        <div>
          <h2 id="pages-title" class="font-medium text-highlighted"> {{ t("Страницы и отчёты") }} </h2>
          <p class="mt-1 text-sm leading-relaxed text-muted"> {{ t("Попросите Кодрика создать страницу, таблицу или график в чате. Опубликованные страницы открываются по ссылке, которую он пришлёт.") }} </p>
        </div>
      </section>
    </main>

    <footer class="border-t border-default py-5 text-sm text-muted"> {{ t("Память, идеи и разговоры — в одном месте.") }} </footer>
  </UContainer>
</template>
