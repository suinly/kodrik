import {formatUiValue,t} from './i18n.ts'
import type {TableColumn} from '@nuxt/ui'
import type {Row} from './ui-document'
import {createUiDateFormatter} from './ui-date.ts'

export function prepareUiTableColumns(value:unknown,simple=false,depth=0):TableColumn<Row>[] {
  if(!Array.isArray(value) || value.length>(simple?34:1000) || depth>16) throw new Error(t("Неверные столбцы таблицы."))
  const keys=new Set<string>()
  return value.map(raw=>{
    if(!raw || typeof raw!=='object' || Array.isArray(raw)) throw new Error(t("Нужно описание столбца таблицы."))
    const column={...raw}
    if(simple){
      if(Object.keys(column).some(key=>!['accessorKey','header','dateFormat'].includes(key))
        || typeof column.accessorKey!=='string' || !column.accessorKey.length || column.accessorKey.length>128
        || /[\x00-\x1f]/.test(column.accessorKey) || ['__proto__','prototype','constructor'].includes(column.accessorKey)
        || keys.has(column.accessorKey) || column.header!==undefined && (typeof column.header!=='string'||column.header.length>200)) throw new Error(t("Неверное имя или повтор столбца таблицы."))
      keys.add(column.accessorKey)
      const key=column.accessorKey
      column.id=key;column.accessorFn=(row:Row)=>Object.hasOwn(row,key)?row[key]:undefined
      column.header??=key
    }
    if(column.columns!==undefined) column.columns=prepareUiTableColumns(column.columns,false,depth+1)
    if(column.dateFormat!==undefined){
      const format=createUiDateFormatter(column.dateFormat)
      column.cell=({getValue}:{getValue:()=>unknown})=>format(getValue())
      delete column.dateFormat
    } else if(column.cell===undefined) column.cell=({getValue}:{getValue:()=>unknown})=>formatUiValue(getValue())
    return column
  })
}
