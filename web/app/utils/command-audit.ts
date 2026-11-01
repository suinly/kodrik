import type {CommandApproval} from './api'
import {t} from './i18n.ts'
import {en} from './translations.ts'

const reasons = {
  safe:'Команда признана безопасной',
  destructive:'Может удалить данные, перезаписать работу или изменить работающий сервис',
  disclosure:'Может раскрыть учётные данные или личную информацию',
  security:'Может изменить права доступа или ослабить защиту',
  uncertain:'Не удалось безопасно определить последствия команды',
  unavailable:'Независимая проверка команды недоступна',
  ask:'Режим «Спрашивать всегда» требует разрешения для каждой команды',
} as const

export function commandAuditReason(approval:Pick<CommandApproval,'reason'|'reason_code'>) {
  const key = approval.reason_code && Object.hasOwn(reasons,approval.reason_code)
    ? reasons[approval.reason_code as keyof typeof reasons]
    : Object.values(reasons).find(key=>en[key]===approval.reason)
  return key ? t(key) : approval.reason
}
