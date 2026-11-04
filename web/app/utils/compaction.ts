import type {Compaction} from './api.ts'
import {t} from './i18n.ts'

export function compactionActive(state:Compaction | null | undefined):boolean {
  return state?.status === 'preparing' && !state.progress?.finished_at
}

export function formatCompaction(state:Compaction, now:number):string {
  const labels = {
    preparing: t('Готовлю краткий контекст…'), ready: t('Краткий контекст подготовлен'),
    applied: t('Контекст сокращён; полная история сохранена'), stale: t('Подготовленный контекст устарел'),
    failed: t('Не удалось подготовить краткий контекст'), cancelled: t('Подготовка контекста отменена'),
  }
  const progress=state.progress
  if(!progress) return labels[state.status]
  const active=compactionActive(state)
  const percent=progress.total_tokens>0 ? Math.min(100,Math.max(0,Math.floor(progress.completed_tokens/progress.total_tokens*100))) : 0
  const parts=[active ? t(progress.background ? 'Сжимаю контекст в фоне' : 'Ожидаю сжатия контекста') : labels[state.status]]
  if(progress.after_tokens != null && progress.finished_at) parts.push(t('{0} → {1} токенов',progress.before_tokens,progress.after_tokens))
  else parts.push(t('{0}% (оценка по токенам)',percent),t('Блоки: {0}/{1}',progress.completed_chunks,progress.total_chunks))
  parts.push(t('Вызовы модели: {0}',progress.model_calls))
  const seconds=(from:string,to:number)=>Math.max(0,Math.floor((to-Date.parse(from))/1000))
  if(active || progress.finished_at) {
    const end=progress.finished_at ? Date.parse(progress.finished_at) : now
    if(Number.isFinite(end) && Number.isFinite(Date.parse(progress.started_at))) parts.push(t('Прошло {0} с',seconds(progress.started_at,end)))
    if(active && progress.request_started_at && Number.isFinite(Date.parse(progress.request_started_at))) parts.push(t('ожидание ответа {0} с',seconds(progress.request_started_at,end)))
  }
  return parts.join(' · ')
}
