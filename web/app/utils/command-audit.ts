import type {CommandApproval} from './api'
import {t} from './i18n.ts'
import {en} from './translations.ts'

export function commandApprovalDetails(approval:Pick<CommandApproval,'tool_name'|'arguments_json'|'cmd'|'workdir'>) {
  // Keep server JSON as text so large MCP IDs are never rounded by JavaScript.
  const args=approval.arguments_json ?? JSON.stringify({cmd:approval.cmd,workdir:approval.workdir},null,2)
  return `{\n  "tool": ${JSON.stringify(approval.tool_name ?? 'exec_command')},\n  "arguments": ${args.replaceAll('\n','\n  ')}\n}`
    .replace(/[\u007f-\u009f\u202a-\u202e\u2066-\u2069]/g,char=>'\\u'+char.charCodeAt(0).toString(16).padStart(4,'0'))
}

const reasons = {
  safe:'Вызов инструмента признан безопасным',
  destructive:'Может удалить данные, перезаписать работу или изменить работающий сервис',
  disclosure:'Может раскрыть учётные данные или личную информацию',
  security:'Может изменить права доступа или ослабить защиту',
  uncertain:'Не удалось безопасно определить последствия вызова',
  unavailable:'Независимая проверка инструмента недоступна',
  ask:'Режим «Спрашивать всегда» требует разрешения для каждого вызова инструмента',
} as const

const legacyReasons:Record<string,keyof typeof reasons> = {
  'Command assessed as safe':'safe',
  'Command effects could not be established safely':'uncertain',
  'Independent command review was unavailable':'unavailable',
  'This session requires permission for every command':'ask',
}

export function commandAuditReason(approval:Pick<CommandApproval,'reason'|'reason_code'>) {
  const key = approval.reason_code && Object.hasOwn(reasons,approval.reason_code)
    ? reasons[approval.reason_code as keyof typeof reasons]
    : Object.hasOwn(legacyReasons,approval.reason) ? reasons[legacyReasons[approval.reason]!] : Object.values(reasons).find(key=>en[key]===approval.reason)
  return key ? t(key) : approval.reason
}
