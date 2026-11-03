import {locale,t} from './i18n.ts'
import {areaY,barY,lineY,ruleY,group,defineChart,colorLegend,colorLegendItems} from '@tanstack/charts'
import {scaleLinear} from '@tanstack/charts/scales/linear'
import {scalePoint} from '@tanstack/charts/scales/point'
import {scaleBand} from '@tanstack/charts/scales/band'
import {scaleOrdinal} from '@tanstack/charts/scales/ordinal'
import {crosshair} from '@tanstack/charts/crosshair'
import {decorative} from '@tanstack/charts/mark/decorative'
import {chartCurves} from '@nuxt/ui/utils/chart'
import type {ChartSlots} from '@nuxt/ui'
type TooltipScope=Parameters<NonNullable<ChartSlots['tooltip']>>[0]

export function checkChartYDomain(props:Record<string,unknown>):[number,number]|undefined {
  const domain=props.yDomain
  if(domain===undefined) return
  if(!Array.isArray(domain)||domain.length!==2||domain.some(value=>typeof value!=='number'||!Number.isFinite(value))||domain[0]>=domain[1]) throw new Error(t("Шкале Y нужны два конечных числа: минимум меньше максимума."))
  // TanStack tick generation and coordinate mapping cannot represent overflowing/subnormal spans.
  const span=domain[1]-domain[0]
  if(!Number.isFinite(span)||!Number.isFinite(1/span)) throw new Error(t("Диапазон шкалы Y слишком велик или слишком мал для отображения."))
  if(![undefined,'line','area','bar'].includes(props.type as string|undefined)||props.definition!=null) throw new Error(t("Шкала Y поддерживает line, area и bar без definition."))
  return [domain[0],domain[1]]
}

export function chartYScale(props:Record<string,unknown>){
  const domain=checkChartYDomain(props)
  // The matching viewport also clips TanStack's separately rendered keyboard focus rings.
  return {scale:domain?scaleLinear().domain(domain):scaleLinear,nice:!domain,...domain?{viewport:{domain,translate:0}}:{}}
}

// Match Nuxt UI's generated Cartesian definition only when an explicit domain is requested.
export function prepareScaledChart(props:Record<string,unknown>){
  if(!checkChartYDomain(props)) return
  const categories=((props.categories??[]) as (string|{key:string;label?:string;color?:string})[]).map(category=>typeof category==='string'?{key:category,label:category}:({...category,label:category.label??category.key}))
  const rows=((props.data??[]) as Record<string,unknown>[]).flatMap(datum=>{
    let positive=0,negative=0
    return categories.map(({key:category})=>{
      const x=datum[String(props.index)] as string|number,value=datum[category]
      if(typeof value!=='number'||!Number.isFinite(value)) return {x,category,value:undefined,base:undefined,top:undefined}
      const base=props.stacked?(value<0?negative:positive):0
      if(value<0) negative+=value;else positive+=value
      return {x,category,value,base,top:base+value}
    })
  })
  const format=(value:number)=>typeof props.format==='function'?props.format(value):new Intl.NumberFormat(locale.value).format(value)
  const curve=chartCurves[props.curve as keyof typeof chartCurves]
  const channels={x:'x',y:'value',z:'category',color:'category'} as const
  const gradients=categories.map((_,i)=>({id:`area-${i+1}`,x1:0,y1:0,x2:0,y2:1,stops:[{offset:0,color:`var(--ts-chart-${i+1})`,opacity:0.4},{offset:1,color:`var(--ts-chart-${i+1})`,opacity:0.05}]}))
  const marks=props.type==='bar'?[
    barY(rows,{...channels,inset:1,...props.stacked?{radius:{end:4,stack:'outer' as const}}:{radius:{end:4},layout:group({padding:0.1})}}),ruleY([0]),
  ]:props.type==='area'?[
    ...categories.flatMap((category,i)=>{
      const series=rows.filter(row=>row.category===category.key)
      return [decorative(areaY(series,{x:'x',y1:'base',y2:'top',z:'category',color:'category',curve,fill:`url(#${gradients[i]!.id})`,fillOpacity:1})),lineY(series,{x:'x',y:'top',z:'category',color:'category',curve,strokeWidth:2})]
    }),crosshair({x:true,y:false}),
  ]:[lineY(rows,{...channels,curve,strokeWidth:2,points:props.points===true}),crosshair({x:true,y:false})]
  const definition=defineChart({
    marks,clip:true,
    scales:{
      x:{scale:props.type==='bar'?()=>scaleBand().padding(0.2):()=>scalePoint().padding(0),axis:props.xAxis===false?false:{label:props.xLabel as string|undefined}},
      y:{...chartYScale(props),grid:props.grid!==false,axis:props.yAxis===false?false:{label:props.yLabel as string|undefined,ticks:{format}}},
    },
    color:{scale:scaleOrdinal().domain(categories.map(category=>category.key)).range(categories.map((_,i)=>`var(--ts-chart-${i+1})`)),legend:props.legend===true||props.legend!==false&&categories.length>1?colorLegend({items:colorLegendItems({justify:'center',gap:16,label:{format:value=>categories.find(category=>category.key===String(value))?.label??String(value)}})}):undefined},
    gradients:props.type==='area'?gradients:undefined,focus:'group-x',maxFocusDistance:Number.POSITIVE_INFINITY,
  })
  const tooltipScope=(scope:TooltipScope):TooltipScope=>{
    const points=[...scope.points].sort((a,b)=>categories.findIndex(category=>category.key===a.datum.category)-categories.findIndex(category=>category.key===b.datum.category))
    return {...scope,points,rows:points.map(point=>({label:categories.find(category=>category.key===point.datum.category)?.label??String(point.datum.category),value:format(point.datum.value),color:point.color,active:points.length>1&&point===scope.primaryPoint}))}
  }
  return {definition,categories,format,tooltipScope}
}
