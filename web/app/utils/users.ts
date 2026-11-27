import {base} from './api'
import {t} from './i18n'

export type AccountUsage = {calls:number;input_tokens:number;cached_tokens:number;output_tokens:number;cost_usd:number|null;partial:boolean;tokens_partial:boolean}
export type Account = {telegram_id:string|null;name:string;owner:boolean;blocked:boolean}
export type User = Account & {usage:AccountUsage|null}

export const accountKey=(account:Account)=>account.owner?'owner':account.telegram_id!
export const accountLabel=(account:Account)=>account.name.trim() || (account.owner?t('Владелец'):account.telegram_id ?? t('Аккаунт'))
export const usersPath=(account:string|null=null)=>new URL(`app/users${account===null?'':`/${encodeURIComponent(account)}`}`,base).pathname
export const usageMetrics=[['calls','Вызовы'],['input_tokens','Входные токены'],['cached_tokens','Кэшированные входные токены'],['output_tokens','Выходные токены']] as const
export const formatUsd=(value:number,locale:string)=>{
  const usd=new Intl.NumberFormat(locale,{style:'currency',currency:'USD',maximumFractionDigits:6})
  return value>0 && value<0.000001 ? `<${usd.format(0.000001)}` : usd.format(value)
}
