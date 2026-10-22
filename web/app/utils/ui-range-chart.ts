import {locale,t} from './i18n.ts'
import {areaY,lineY,defineChart,colorLegend,colorLegendItems} from '@tanstack/charts'
import {scaleLinear} from '@tanstack/charts/scales/linear'
import {scalePoint} from '@tanstack/charts/scales/point'
import {scaleOrdinal} from '@tanstack/charts/scales/ordinal'
import {crosshair} from '@tanstack/charts/crosshair'
import {decorative} from '@tanstack/charts/mark/decorative'
import type {Row} from './ui-document'

const midpointKey='__range_midpoint'
function check(value:unknown,message:string):asserts value {if(!value) throw new Error(message)}
const numeric=(value:unknown):value is number=>typeof value==='number'&&Number.isFinite(value)

export function prepareRangeChart(props:Record<string,unknown>){
  const range=props.range as Record<string,unknown>,index=props.index
  check(range && typeof range==='object' && !Array.isArray(range) && Object.keys(range).every(key=>['min','max'].includes(key)),t("Коридору нужны поля range.min и range.max."))
  for(const key of [range.min,range.max,index]) check(typeof key==='string' && key.length>0 && key.length<=128 && !['__proto__','constructor','prototype'].includes(key),t("Неверное поле коридора или оси."))
  const min=String(range.min),max=String(range.max),xKey=String(index)
  check((props.type===undefined||props.type==='line') && (props.curve===undefined||props.curve==='linear') && !props.stacked && props.definition==null,t("Коридор поддерживает линейный график без stacked, сглаживания и definition."))
  check(Array.isArray(props.data),t("Коридору нужен массив строк."))
  check(props.categories===undefined||Array.isArray(props.categories),t("Сериям коридора нужен массив categories."))
  const seenSeries=new Set([midpointKey])
  const categories=[{key:midpointKey,label:t("Среднее"),color:'primary' as string|undefined},...(props.categories as unknown[]??[]).flatMap(item=>{
    const category=typeof item==='string'?{key:item}:item as {key?:unknown;label?:unknown;color?:unknown}
    check(category && typeof category==='object' && typeof category.key==='string' && category.key.length>0 && category.key.length<=128,t("Неверная серия коридора."))
    if(category.key===min||category.key===max) return []
    check(!seenSeries.has(category.key) && (category.label===undefined||typeof category.label==='string') && (category.color===undefined||typeof category.color==='string'),t("Повтор или неверная подпись серии коридора."))
    seenSeries.add(category.key)
    return [{key:category.key,label:category.label??category.key,color:category.color}]
  })]
  const seenX=new Set<string>()
  const rows=(props.data as Row[]).map(row=>{
    check(row && typeof row==='object' && Object.hasOwn(row,xKey) && (typeof row[xKey]==='string'||numeric(row[xKey])),t("Неверное значение оси коридора."))
    const x=row[xKey] as string|number
    check(!seenX.has(String(x)),t("Значения index коридора должны быть уникальны."))
    seenX.add(String(x))
    check(Object.hasOwn(row,min)&&Object.hasOwn(row,max),t("Поле границы коридора не найдено."))
    const low=row[min],high=row[max]
    check((low===null||numeric(low))&&(high===null||numeric(high)),t("Границам коридора нужны числа или null."))
    check(low===null||high===null||low<=high,t("Минимум коридора превышает максимум."))
    const references:Record<string,number|undefined>={}
    for(const category of categories.slice(1)){
      check(Object.hasOwn(row,category.key)&&(row[category.key]===null||numeric(row[category.key])),t("Сериям коридора нужны числовые значения или null."))
      references[category.key]=row[category.key]===null?undefined:Number(row[category.key])
    }
    const complete=low!==null&&high!==null
    return {x,min:complete?low:undefined,max:complete?high:undefined,mid:complete?low/2+high/2:undefined,references}
  })
  const lines=rows.flatMap(row=>categories.map(category=>({...row,category:category.key,value:category.key===midpointKey?row.mid:row.references[category.key]})))
  const format=(value:number)=>new Intl.NumberFormat(locale.value,{maximumFractionDigits:6}).format(value)
  const definition=defineChart({
    marks:[
      decorative(areaY(rows,{id:'range-band',x:'x',y1:'min',y2:'max',fill:'var(--ts-chart-1)',fillOpacity:0.2})),
      lineY(lines,{id:'range-lines',x:'x',y:'value',z:'category',color:'category',strokeWidth:2,points:props.points===true||rows.length===1||lines.some(row=>row.value===undefined)}),
      crosshair({x:true,y:false}),
    ],
    scales:{
      x:{scale:()=>scalePoint().padding(0),axis:props.xAxis===false?false:{label:props.xLabel as string|undefined}},
      y:{scale:scaleLinear,nice:true,grid:props.grid!==false,axis:props.yAxis===false?false:{label:props.yLabel as string|undefined,ticks:{format}}},
    },
    color:{
      scale:scaleOrdinal().domain(categories.map(category=>category.key)).range(categories.map((_,i)=>`var(--ts-chart-${i+1})`)),
      legend:props.legend===true||props.legend!==false&&categories.length>1?colorLegend({items:colorLegendItems({justify:'center',gap:16,label:{format:value=>categories.find(category=>category.key===String(value))?.label??String(value)}})}):undefined,
    },
    focus:'group-x',maxFocusDistance:Number.POSITIVE_INFINITY,
  })
  return {rows,categories,definition}
}
