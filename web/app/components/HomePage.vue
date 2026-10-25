<script setup lang="ts">
import {t} from '../utils/i18n'

const repository = 'https://github.com/suinly/kodrik'
const features = [
  {icon: 'i-lucide-panels-top-left', title: 'Дневники и личные страницы', description: 'Помогает вести записи, следить за прогрессом и создавать страницы с таблицами и графиками.'},
  {icon: 'i-lucide-workflow', title: 'Привычные задачи', description: 'Сохраняет удачные способы работы в виде навыков. С помощью вебхуков запускает задачи по событиям в других сервисах.'},
  {icon: 'i-lucide-brain', title: 'Помнит важное', description: 'Запоминает ваши предпочтения и интересы. Обращается к прошлым разговорам, когда нужен контекст.'},
  {icon: 'i-lucide-terminal', title: 'Работает с файлами и кодом', description: 'Читает и редактирует файлы, выполняет команды, помогает писать код. Приглашённые пользователи работают в отдельной песочнице.'},
  {icon: 'i-lucide-search', title: 'Ищет и объясняет', description: 'Помогает найти информацию и разобраться в теме. Для поиска и чтения веб-страниц можно подключить SearXNG и Obscura.'},
  {icon: 'i-lucide-scan-text', title: 'Документы и изображения', description: 'Помогает разобраться в документах, извлечь нужные сведения и проанализировать изображения.'},
]
const config = `[gateway]

[provider.primary]
type = "openai"
model = "your-model"
api_key = "your-key"`

function scrollToSection(id: string) {
  // Native hash navigation makes the lightweight Nuxt router repeat the URL prefix.
  window.history.replaceState(window.history.state, '', `#${id}`)
  const section = document.getElementById(id)
  section?.scrollIntoView()
  section?.focus({preventScroll: true})
}

useHead(() => ({
  title: t('Кодрик | Локальный ИИ-агент'),
  meta: [{name: 'description', content: t('Кодрик помогает вести дневники, работать с данными и создавать личные страницы. Помнит ваши предпочтения и хранит историю на вашем компьютере.')}],
}))
</script>

<template>
  <UContainer class="home-page max-w-7xl">
    <a href="#main" class="sr-only focus:not-sr-only focus:inline-block focus:py-3" @click.exact.prevent="scrollToSection('main')">{{ t('К содержимому') }}</a>
    <header class="flex min-h-20 items-center justify-between gap-4 border-b border-default">
      <KodrikLogo class="h-7 sm:h-8" />
      <nav :aria-label="t('Навигация')" class="flex items-center gap-1 sm:gap-4">
        <a href="#capabilities" class="home-link hidden text-sm sm:inline-flex" @click.exact.prevent="scrollToSection('capabilities')">{{ t('Возможности') }}</a>
        <a href="#installation" class="home-link hidden text-sm sm:inline-flex" @click.exact.prevent="scrollToSection('installation')">{{ t('Установка') }}</a>
        <UButton :to="repository" external icon="i-lucide-github" aria-label="GitHub" title="GitHub" color="neutral" variant="ghost" size="lg" />
        <LanguageSwitch />
        <ThemeSwitch />
      </nav>
    </header>

    <main id="main" tabindex="-1">
      <section aria-labelledby="home-title" class="grid items-center gap-10 py-14 sm:py-20 lg:grid-cols-[1.3fr_1fr] lg:gap-16 lg:py-24">
        <div>
          <p class="mb-5 text-sm font-medium text-primary">{{ t('Локальный ИИ-агент с открытым кодом') }}</p>
          <h1 id="home-title" class="text-[clamp(2rem,5.2vw,4.5rem)] leading-[1.08] font-semibold tracking-[-0.045em] text-highlighted">
            <span class="block">{{ t('От разговора') }}</span>
            <span class="block text-primary">{{ t('к делу.') }}</span>
          </h1>
          <p class="mt-6 max-w-lg text-lg leading-relaxed text-muted">{{ t('Расскажите, что хотите сделать. Кодрик поможет разобраться в вопросе, решить задачу или создать нужный вам инструмент.') }}</p>
          <UButton to="#installation" external :label="t('Установка и настройка')" trailing-icon="i-lucide-arrow-down" size="xl" class="mt-8" @click.exact.prevent="scrollToSection('installation')" />
        </div>
        <HomeWorkoutDemo />
      </section>

      <section id="local-first" aria-labelledby="local-first-title" class="grid gap-6 border-t border-default py-10 md:grid-cols-[1fr_2fr] md:gap-12">
        <h2 id="local-first-title" class="flex items-start gap-3 text-2xl font-semibold tracking-tight text-highlighted">
          <UIcon name="i-lucide-hard-drive" class="mt-1 size-6 shrink-0 text-primary" />{{ t('Данные на вашем компьютере') }}
        </h2>
        <dl class="grid gap-6 sm:grid-cols-2">
          <div>
            <dt class="font-medium text-highlighted">{{ t('История и память') }}</dt>
            <dd class="mt-2 text-sm leading-relaxed text-muted">{{ t('Кодрик сохраняет разговоры, предпочтения и навыки на вашем компьютере.') }}</dd>
          </div>
          <div>
            <dt class="font-medium text-highlighted">{{ t('Модель по вашему выбору') }}</dt>
            <dd class="mt-2 text-sm leading-relaxed text-muted">{{ t('Вы сами выбираете модель. Для обработки запросов и вложений Кодрик обращается к её провайдеру.') }}</dd>
          </div>
        </dl>
      </section>

      <section aria-labelledby="interfaces-title" class="grid gap-6 border-y border-default py-8 md:grid-cols-[1fr_2fr] md:gap-12">
        <h2 id="interfaces-title" class="flex max-w-xs items-start gap-3 text-xl font-medium tracking-tight text-highlighted">
          <UIcon name="i-lucide-messages-square" class="mt-0.5 size-6 shrink-0 text-primary" />{{ t('Продолжайте там, где удобно') }}
        </h2>
        <div>
          <ul class="flex flex-wrap gap-x-8 gap-y-4 text-sm font-medium text-highlighted">
            <li class="flex items-center gap-2"><UIcon name="i-lucide-terminal" class="size-5 text-primary" />{{ t('Терминал') }}</li>
            <li class="flex items-center gap-2"><UIcon name="i-lucide-send" class="size-5 text-primary" />Telegram</li>
            <li class="flex items-center gap-2"><UIcon name="i-lucide-globe" class="size-5 text-primary" />{{ t('Браузер') }}</li>
          </ul>
          <p class="mt-4 max-w-2xl text-sm leading-relaxed text-muted">{{ t('Начните с терминала. Позже можно подключить Telegram и веб-интерфейс.') }}</p>
        </div>
      </section>

      <section id="capabilities" tabindex="-1" aria-labelledby="capabilities-title" class="home-section">
        <div class="flex items-center gap-4">
          <UIcon name="i-lucide-list-checks" class="size-8 shrink-0 text-primary" />
          <h2 id="capabilities-title" class="home-heading">{{ t('Помощь в повседневных делах') }}</h2>
        </div>
        <p class="mt-4 max-w-2xl leading-relaxed text-muted">{{ t('Например, попросите Кодрика вести дневник тренировок: он сохранит записи и создаст страницу, на которой удобно следить за прогрессом.') }}</p>
        <div class="mt-10 grid gap-x-16 gap-y-10 sm:grid-cols-2 sm:gap-y-12">
          <article v-for="feature in features" :key="feature.title" class="flex gap-4">
            <UIcon :name="feature.icon" class="mt-1 size-6 shrink-0 text-primary" />
            <div>
              <h3 class="text-lg font-medium text-highlighted">{{ t(feature.title) }}</h3>
              <p class="mt-2 max-w-lg text-sm leading-relaxed text-muted">{{ t(feature.description) }}</p>
            </div>
          </article>
        </div>
      </section>

      <section id="installation" tabindex="-1" aria-labelledby="installation-title" class="home-section border-t border-default">
        <h2 id="installation-title" class="home-heading">{{ t('Как начать пользоваться Кодриком') }}</h2>
        <p class="mt-4 max-w-2xl leading-relaxed text-muted">{{ t('Для начала понадобятся Homebrew и API-ключ модели с поддержкой OpenAI Responses API и вызова инструментов.') }}</p>

        <div class="mt-10 grid gap-10 lg:grid-cols-[0.75fr_1.25fr] lg:gap-16">
          <div>
            <h3 class="text-xl font-medium text-highlighted">{{ t('Установите Кодрик') }}</h3>
            <p class="mt-3 text-sm text-muted">macOS 14+ · Apple Silicon</p>
            <pre class="home-code mt-5"><code>brew install suinly/tap/kodrik
install -d -m 700 ~/.kodrik</code></pre>
          </div>
          <div>
            <h3 class="text-xl font-medium text-highlighted">{{ t('Подключите модель') }}</h3>
            <p class="mt-3 text-sm leading-relaxed text-muted">{{ t('Создайте файл ~/.kodrik/config.toml с настройками модели. Замените your-model на её название, а your-key на свой API-ключ.') }}</p>
            <pre class="home-code mt-5"><code>{{ config }}</code></pre>
          </div>
        </div>

        <div class="mt-10 grid gap-5 rounded-2xl border border-default bg-elevated/40 p-6 sm:p-8 lg:grid-cols-[0.75fr_1.25fr] lg:gap-16">
          <div>
            <h3 class="text-xl font-medium text-highlighted">{{ t('Запустите Кодрик') }}</h3>
            <p class="mt-3 text-sm leading-relaxed text-muted">{{ t('Защитите файл с API-ключом и запустите Кодрик в терминале.') }}</p>
          </div>
          <pre class="home-code self-center"><code>chmod 600 ~/.kodrik/config.toml
kodrik</code></pre>
        </div>

        <a :href="`${repository}/blob/main/docs/getting-started.md`" class="home-link mt-8 inline-flex items-center gap-2 text-sm font-medium">{{ t('Подробная инструкция') }}<UIcon name="i-lucide-arrow-up-right" class="size-4" /></a>
      </section>
    </main>

    <footer class="flex flex-col gap-5 border-t border-default py-8 text-sm sm:flex-row sm:items-center sm:justify-between">
      <p class="text-muted">{{ t('Кодрик. Личный помощник на вашем компьютере.') }}</p>
      <nav :aria-label="t('Ресурсы проекта')" class="flex gap-6">
        <a :href="repository" class="home-link inline-flex items-center gap-1.5">GitHub<UIcon name="i-lucide-arrow-up-right" class="size-4" /></a>
        <a :href="`${repository}/blob/main/docs/index.md`" class="home-link">{{ t('Документация') }}</a>
      </nav>
    </footer>
  </UContainer>
</template>

<style scoped>
.home-page .grid > * { min-width: 0; }
.home-section { padding-block: clamp(3.5rem, 7vw, 6rem); scroll-margin-top: 2rem; }
.home-heading { max-width: 28ch; font-size: clamp(1.8rem, 3vw, 2.6rem); line-height: 1.2; font-weight: 600; letter-spacing: -0.035em; color: var(--ui-text-highlighted); }
.home-code { min-width: 0; overflow-x: auto; border-radius: 0.75rem; padding: 1.25rem; background: var(--ui-bg-muted); color: var(--ui-text-highlighted); font-size: 0.8125rem; line-height: 1.8; }
.home-link { color: var(--ui-text-toned); transition: color 150ms; }
.home-link:hover { color: var(--ui-primary); }
.home-page :is(a, button, summary):focus-visible { outline: 2px solid var(--ui-primary); outline-offset: 5px; border-radius: 0.25rem; }
.home-page :is(a, button):active { transform: translateY(1px); }
</style>
