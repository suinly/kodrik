import {locale as currentLocale,t} from './i18n.ts'
export type UiDateFormat=Intl.DateTimeFormatOptions & {locale?:string}

export function createUiDateFormatter(raw:unknown=undefined):(value:unknown)=>string {
  const fields=['dateStyle','timeStyle','year','month','day','weekday','hour','minute','second','hour12','timeZone','locale']
  const options=raw===undefined?{}:typeof raw==='string'?{dateStyle:raw}:raw
  if(!options || typeof options!=='object' || Array.isArray(options)) throw new Error(t("Нужен стиль даты или объект dateFormat."))
  for(const [key,value] of Object.entries(options)){
    if(!fields.includes(key) || (key==='hour12'?typeof value!=='boolean':typeof value!=='string'||value.length>128)) throw new Error(t("Неверная настройка даты «{0}».", key))
  }
  const {locale=currentLocale.value,...settings}=options as UiDateFormat
  if(!Intl.DateTimeFormat.supportedLocalesOf(locale).length) throw new Error(t("Локаль даты не поддерживается."))
  if(!Object.keys(settings).some(key=>!['timeZone','hour12'].includes(key))) settings.dateStyle='short'
  const formatter=new Intl.DateTimeFormat(locale,{timeZone:'UTC',...settings})
  // Calendar dates have no instant or timezone; preserve their day in every display zone.
  const calendarFormatter=new Intl.DateTimeFormat(locale,{...settings,timeZone:'UTC'})
  return value=>{
    if(value===null || value===undefined) return t("Нет данных")
    if(typeof value!=='string' || !/^\d{4}-\d{2}-\d{2}(?:T(?:[01]\d|2[0-3]):[0-5]\d(?::[0-5]\d(?:\.\d{1,9})?)?(?:Z|[+-](?:[01]\d|2[0-3]):[0-5]\d))?$/.test(value)) throw new Error(t("Нужна ISO-дата или ISO-время с часовым поясом."))
    const day=value.slice(0,10),calendar=new Date(`${day}T00:00:00Z`),date=new Date(value)
    if(!Number.isFinite(calendar.getTime()) || calendar.toISOString().slice(0,10)!==day || !Number.isFinite(date.getTime())) throw new Error(t("Некорректная календарная дата."))
    return (value.length===10?calendarFormatter:formatter).format(date)
  }
}
