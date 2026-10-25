<script setup lang="ts">
import {t} from '../utils/i18n'

const minutes = ref<number | null>(30)
const savedMinutes = ref<number | null>(null)
const canAdd = computed(() => savedMinutes.value === null && minutes.value !== null && Number.isInteger(minutes.value) && minutes.value >= 5 && minutes.value <= 180)
const workouts = computed(() => [
  {day: t('Пн'), minutes: 20},
  {day: t('Ср'), minutes: 35},
  {day: t('Пт'), minutes: 25},
  ...(savedMinutes.value === null ? [] : [{day: t('Сегодня'), minutes: savedMinutes.value}]),
])
const total = computed(() => workouts.value.reduce((sum, workout) => sum + workout.minutes, 0))
const formatMinutes = (value: number) => t('{0} мин.', value)

function addWorkout() {
  if (canAdd.value) savedMinutes.value = minutes.value
}
function reset() {
  savedMinutes.value = null
  minutes.value = 30
}
</script>

<template>
  <section aria-labelledby="workout-demo-title" class="workout-demo min-w-0 rounded-2xl border border-default bg-elevated/40 p-5 sm:p-7">
    <header class="mb-6 flex items-center justify-between gap-4">
      <div>
        <p class="mb-2 text-xs text-muted">{{ t('Интерактивный пример') }}</p>
        <h2 id="workout-demo-title" class="text-xl font-semibold tracking-tight text-highlighted">{{ t('Дневник тренировок') }}</h2>
      </div>
      <UIcon name="i-lucide-chart-column" class="size-10 shrink-0 text-primary" />
    </header>

    <form class="flex items-end gap-3" @submit.prevent="addWorkout">
      <div class="min-w-0 flex-1">
        <label for="workout-minutes" class="mb-2 block text-xs font-medium text-toned">{{ t('Длительность, мин.') }}</label>
        <UInput id="workout-minutes" v-model.nullable="minutes" type="number" required :min="5" :max="180" :disabled="savedMinutes !== null" :aria-label="t('Длительность тренировки, минуты')" size="lg" class="w-full" :ui="{base: 'text-center tabular-nums'}" />
      </div>
      <UButton type="submit" :disabled="!canAdd" :label="t('Добавить')" :aria-label="t('Добавить тренировку')" size="lg" />
    </form>

    <div class="flex min-h-18 items-center py-4 text-sm" aria-live="polite" aria-atomic="true">
      <Transition name="workout-entry" mode="out-in">
        <div v-if="savedMinutes !== null" data-testid="workout-entry" class="flex w-full items-center gap-2 text-highlighted">
          <UIcon name="i-lucide-check" class="size-4 shrink-0 text-primary" />
          <span>{{ t('Сегодня') }}</span>
          <span class="ml-auto font-medium text-primary tabular-nums">{{ formatMinutes(savedMinutes) }}</span>
        </div>
        <p v-else class="text-muted">{{ t('Добавьте тренировку и посмотрите, как изменится график.') }}</p>
      </Transition>
    </div>

    <figure class="border-t border-default pt-4">
      <figcaption class="mb-2 flex items-center justify-between gap-2 text-xs text-muted">
        <span>{{ t('За неделю') }}</span>
        <span class="font-medium text-highlighted tabular-nums">{{ formatMinutes(total) }}</span>
      </figcaption>
      <div :class="{'workout-chart-updated': savedMinutes !== null}">
        <UChart type="bar" :data="workouts" index="day" :categories="[{key: 'minutes', label: t('Тренировка'), color: 'primary'}]" :height="132" :grid="false" :y-axis="false" :format="formatMinutes"
          :aria-label="t('Длительность тренировок, всего {0} мин. за неделю.', total)" />
      </div>
    </figure>
    <div class="mt-4 flex justify-end">
      <UButton :label="t('Сбросить пример')" icon="i-lucide-rotate-ccw" color="neutral" variant="link" size="xs" :disabled="savedMinutes === null" @click="reset" />
    </div>
  </section>
</template>

<style scoped>
@media (prefers-reduced-motion: no-preference) {
  .workout-entry-enter-active, .workout-entry-leave-active { transition: opacity 160ms ease, transform 160ms ease; }
  .workout-entry-enter-from { opacity: 0; transform: translateY(6px); }
  .workout-entry-leave-to { opacity: 0; transform: translateY(-4px); }
  .workout-chart-updated { animation: workout-chart-in 320ms ease-out 100ms both; }
}
@keyframes workout-chart-in {
  from { opacity: 0.4; transform: translateY(6px); }
  to { opacity: 1; transform: translateY(0); }
}
</style>
